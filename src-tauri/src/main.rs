#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod application;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod automatic_download;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod automatic_protection;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod clipboard;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod diagnostics;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod launch_at_login;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod local_log;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod manual_session;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod network_activity;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod power_source;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod product_composition;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod settings;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod sleep_inhibitor;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod tray;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod tray_host;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod updater;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod wake_coordinator;

#[cfg(test)]
use application::{ApplicationLifecycle, CapabilityId, LifecyclePlan, OwnedResource};
#[cfg(test)]
use launch_at_login::LaunchAtLoginFailure;
use launch_at_login::LaunchAtLoginService;
use local_log::{LocalLog, SafeEvent};
use product_composition::ProductRuntime;
#[cfg(test)]
use settings::SettingsService;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use tauri::Manager;
use tray_host::PresentationRuntime;
#[cfg(test)]
use wake_coordinator::WakeCoordinator;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

type ProductOwner = Arc<Mutex<Option<ProductRuntime>>>;
type PresentationOwner = Arc<Mutex<Option<PresentationRuntime>>>;
type ShutdownBridge = Arc<OnceLock<ShutdownCoordinator>>;

#[derive(Clone)]
struct ShutdownCoordinator {
    product: ProductOwner,
    presentation: PresentationOwner,
    log: LocalLog,
    shutdown_logged: Arc<std::sync::atomic::AtomicBool>,
    finishing: Arc<(Mutex<FinishState>, Condvar)>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FinishState {
    Idle,
    Running,
    Complete,
}

struct FinishCompletion(Arc<(Mutex<FinishState>, Condvar)>);

impl Drop for FinishCompletion {
    fn drop(&mut self) {
        let (state, completed) = &*self.0;
        *state.lock().expect("shutdown completion lock poisoned") = FinishState::Complete;
        completed.notify_all();
    }
}

impl ShutdownCoordinator {
    fn new(product: ProductOwner, presentation: PresentationOwner, log: LocalLog) -> Self {
        Self {
            product,
            presentation,
            log,
            shutdown_logged: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finishing: Arc::new((Mutex::new(FinishState::Idle), Condvar::new())),
        }
    }

    fn begin(&self) {
        if !self
            .shutdown_logged
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.log.record(SafeEvent::ShutdownRequested);
        }
        if let Some(product) = self
            .product
            .lock()
            .expect("product owner lock poisoned")
            .as_mut()
        {
            product.begin_shutdown();
        }
    }

    fn request_presentation_exit(&self, code: i32) -> bool {
        request_presentation_exit(&self.presentation, code)
    }

    fn finish(&self) {
        let (state, completed) = &*self.finishing;
        let mut current = state.lock().expect("shutdown completion lock poisoned");
        match *current {
            FinishState::Idle => *current = FinishState::Running,
            FinishState::Running => {
                while *current != FinishState::Complete {
                    current = completed
                        .wait(current)
                        .expect("shutdown completion lock poisoned");
                }
                return;
            }
            FinishState::Complete => return,
        }
        drop(current);
        self.finish_claimed();
    }

    fn finish_claimed(&self) {
        let _completion = FinishCompletion(self.finishing.clone());
        self.begin();
        if let Err(error) = finish_presentation_shutdown(&self.presentation) {
            eprintln!("Chiù tray presentation cleanup failed: {error}");
            self.log
                .record(SafeEvent::PresentationFailure { stage: "shutdown" });
        }
        let report = finish_product_shutdown(&self.product);
        for failure in report.failures() {
            eprintln!(
                "Chiù cleanup failed for {:?}: {}",
                failure.capability(),
                failure.message()
            );
            self.log.record(SafeEvent::CleanupFailed {
                capability: failure.capability().as_str(),
            });
        }
        self.log.record(SafeEvent::ProcessStopped);
    }
}

fn updater_shutdown_callback(
    shutdown: Weak<OnceLock<ShutdownCoordinator>>,
) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        if let Some(bridge) = shutdown.upgrade() {
            bridge
                .get()
                .expect("shutdown coordinator initialized before updater")
                .finish();
        }
    })
}

enum HostStartup<R, E> {
    Operational(R),
    ProductFailed(E),
}

fn orchestrate_startup<F, S, R, E>(
    presentation_owner: &PresentationOwner,
    start_presentation: F,
    start_product: S,
) -> Result<HostStartup<R, E>, BoxError>
where
    F: FnOnce() -> Result<PresentationRuntime, BoxError>,
    S: FnOnce() -> Result<R, E>,
{
    let mut presentation = start_presentation()?;
    let control = presentation.control();
    {
        let mut owner = presentation_owner
            .lock()
            .expect("presentation owner lock poisoned");
        if owner.is_some() {
            drop(owner);
            presentation.shutdown()?;
            return Err(
                std::io::Error::other("tray presentation initialized more than once").into(),
            );
        }
        *owner = Some(presentation);
    }

    let outcome = match start_product() {
        Ok(startup) => HostStartup::Operational(startup),
        Err(error) => HostStartup::ProductFailed(error),
    };
    control.request_refresh();
    Ok(outcome)
}

fn main() {
    match run() {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) => {
            eprintln!("Chiù failed to start: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> tauri::Result<i32> {
    let owner: ProductOwner = Arc::new(Mutex::new(None));
    let presentation_owner: PresentationOwner = Arc::new(Mutex::new(None));
    let shutdown: ShutdownBridge = Arc::new(OnceLock::new());
    let setup_owner = owner.clone();
    let setup_shutdown = shutdown.clone();
    let event_shutdown = shutdown.clone();
    let setup_presentation_owner = presentation_owner.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .setup(move |app| {
            if let Err(error) = configure_host(app) {
                eprintln!("Chiù failed to initialize its tray host: {error}");
                app.handle().exit(1);
                return Ok(());
            }

            let log = LocalLog::initialize(app.path().app_log_dir().ok());
            log.record(SafeEvent::ProcessStarted);
            let copy_error_presenter =
                tray_host::register_copy_diagnostics_error_presenter(app.handle());
            let native_dialogs_available = copy_error_presenter.is_some();
            if !native_dialogs_available {
                eprintln!("Chiù native error presentation is unavailable");
                log.record(SafeEvent::PresentationFailure {
                    stage: "dialog-initialization",
                });
            }

            let settings_directory = app.path().app_config_dir().ok();
            let launch_app = app.handle().clone();
            let updater_app = app.handle().clone();
            let updater_shutdown = updater_shutdown_callback(Arc::downgrade(&setup_shutdown));
            let runtime = ProductRuntime::build(
                settings_directory,
                log.clone(),
                move |settings, log| LaunchAtLoginService::register(settings, &launch_app, log),
                move |settings, log| {
                    updater::register(
                        &updater_app,
                        settings,
                        updater_shutdown,
                        native_dialogs_available,
                        log,
                    )
                },
            );
            let tray = runtime.tray();
            setup_shutdown
                .set(ShutdownCoordinator::new(
                    setup_owner.clone(),
                    setup_presentation_owner.clone(),
                    log.clone(),
                ))
                .unwrap_or_else(|_| panic!("shutdown coordinator initialized more than once"));
            *setup_owner.lock().expect("product owner lock poisoned") = Some(runtime);
            let presentation_tray = tray.clone();
            let presentation_app = app.handle().clone();
            let presentation_copy_error_presenter = copy_error_presenter;
            let startup = orchestrate_startup(
                &setup_presentation_owner,
                move || {
                    tray_host::start(
                        &presentation_app,
                        presentation_tray,
                        presentation_copy_error_presenter,
                    )
                },
                || {
                    setup_owner
                        .lock()
                        .expect("product owner lock poisoned")
                        .as_mut()
                        .expect("product runtime initialized before presentation startup")
                        .start()
                },
            );

            match startup {
                Ok(HostStartup::Operational(startup)) => {
                    let report = startup.report();
                    let settings_health = startup.settings_health();
                    if report.optional_failures().is_empty() {
                        log.record(SafeEvent::ApplicationReady);
                    } else {
                        log.record(SafeEvent::ApplicationDegraded);
                    }
                    if !settings_health.is_clean() {
                        eprintln!(
                            "Chiù settings recovery: {:?}; fields: {:?}; artifacts: {:?}; persistence: {:?}",
                            settings_health.load_condition(),
                            settings_health.field_recoveries(),
                            settings_health.artifact_warnings(),
                            settings_health.persistence(),
                        );
                    }
                    for failure in report.optional_failures() {
                        eprintln!(
                            "Chiù capability {:?} is unavailable: {}",
                            failure.capability(),
                            failure.message()
                        );
                        log.record(SafeEvent::OptionalCapabilityUnavailable {
                            capability: failure.capability().as_str(),
                        });
                    }
                }
                Ok(HostStartup::ProductFailed(error)) => {
                    log.record(SafeEvent::ApplicationFailed);
                    eprintln!("Chiù application startup failed: {error}");
                    if let Some(failure_point) = error.failure_point() {
                        eprintln!("Chiù startup failure point: {failure_point:?}");
                    }
                    for failure in error.cleanup_failures() {
                        eprintln!(
                            "Chiù startup rollback failed for {:?}: {}",
                            failure.capability(),
                            failure.message()
                        );
                        log.record(SafeEvent::CleanupFailed {
                            capability: failure.capability().as_str(),
                        });
                    }
                }
                Err(error) => {
                    log.record(SafeEvent::PresentationFailure { stage: "startup" });
                    eprintln!("Chiù failed to initialize its tray recovery surface: {error}");
                    app.handle().exit(1);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())?;

    let exit_code = app.run_return(move |_, event| {
        if let tauri::RunEvent::ExitRequested { code, api, .. } = event
            && let Some(shutdown) = event_shutdown.get()
        {
            shutdown.begin();
            if shutdown.request_presentation_exit(code.unwrap_or(0)) {
                api.prevent_exit();
            } else {
                shutdown.finish();
            }
        }
    });
    if let Some(shutdown) = shutdown.get() {
        shutdown.finish();
    }

    Ok(exit_code)
}

#[cfg(test)]
fn test_launch_at_login(
    settings: SettingsService,
    log: LocalLog,
) -> Result<LaunchAtLoginService, LaunchAtLoginFailure> {
    struct MatchingAdapter(Mutex<bool>);

    impl launch_at_login::LaunchAtLoginAdapter for MatchingAdapter {
        fn observe(&self) -> Result<bool, String> {
            Ok(*self.0.lock().expect("launch adapter lock poisoned"))
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), String> {
            *self.0.lock().expect("launch adapter lock poisoned") = enabled;
            Ok(())
        }
    }

    let desired = settings.snapshot().launch_at_login();
    Ok(LaunchAtLoginService::available(
        settings,
        MatchingAdapter(Mutex::new(desired)),
        log,
    ))
}

#[cfg(test)]
fn test_lifecycle(
    wake: WakeCoordinator,
    manual: manual_session::ManualSession,
) -> ApplicationLifecycle<SettingsService> {
    let wake_cleanup = wake.clone();
    LifecyclePlan::new(|| {
        Ok(SettingsService::unavailable(LocalLog::unavailable(
            local_log::LoggingFailureStage::Open,
        )))
    })
    .required(CapabilityId::new("wake-coordination"), move |_| {
        Ok(OwnedResource::new(move || {
            wake_cleanup
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    })
    .required(CapabilityId::new("manual-session"), move |_| {
        let scheduler = manual
            .start_scheduler()
            .map_err(|error| Box::new(error) as BoxError)?;
        Ok(OwnedResource::new(move || {
            scheduler
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    })
    .build()
}

fn request_presentation_exit(owner: &PresentationOwner, code: i32) -> bool {
    owner
        .lock()
        .expect("presentation owner lock poisoned")
        .as_ref()
        .is_some_and(|presentation| presentation.control().request_exit(code))
}

fn finish_product_shutdown(owner: &ProductOwner) -> application::ShutdownReport {
    match owner.lock().expect("product owner lock poisoned").take() {
        Some(mut product) => product.shutdown(),
        None => application::ShutdownReport::default(),
    }
}

fn finish_presentation_shutdown(owner: &PresentationOwner) -> Result<(), BoxError> {
    if let Some(mut presentation) = owner
        .lock()
        .expect("presentation owner lock poisoned")
        .take()
    {
        presentation.shutdown()?;
    }
    Ok(())
}

fn configure_host(app: &mut tauri::App) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        app.handle()
            .set_activation_policy(tauri::ActivationPolicy::Accessory)?;
        app.handle().set_dock_visibility(false)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests;
