use super::*;
use crate::{
    application::CapabilityId,
    launch_at_login::{LaunchAtLoginAdapter, LaunchAtLoginFailure, LaunchAtLoginService},
    settings::{SettingsService, SettingsUpdate},
    tray::{TrayCommand, TrayCommandOutcome},
    updater::{UpdaterRegistration, UpdaterService},
};
use std::{fs, path::PathBuf};

fn unavailable_log() -> LocalLog {
    LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
}

fn unconfigured_updater(settings: SettingsService, _log: LocalLog) -> UpdaterRegistration {
    UpdaterRegistration {
        service: UpdaterService::unconfigured(&settings),
        runtime: updater::UpdaterRuntime::empty(),
        failure: None,
    }
}

fn build_runtime<L>(
    settings_directory: Option<PathBuf>,
    initialize_launch_at_login: L,
) -> ProductRuntime
where
    L: FnOnce(SettingsService, LocalLog) -> Result<LaunchAtLoginService, LaunchAtLoginFailure>
        + Send
        + 'static,
{
    ProductRuntime::build(
        settings_directory,
        unavailable_log(),
        initialize_launch_at_login,
        unconfigured_updater,
    )
}

#[derive(Clone)]
struct LifecycleLaunchAdapter {
    observations: Arc<Mutex<Vec<bool>>>,
    mutations: Arc<Mutex<Vec<bool>>>,
    set_failure: Option<String>,
}

impl LifecycleLaunchAdapter {
    fn new(observations: Vec<bool>) -> Self {
        Self {
            observations: Arc::new(Mutex::new(observations.into_iter().rev().collect())),
            mutations: Arc::new(Mutex::new(Vec::new())),
            set_failure: None,
        }
    }
}

impl LaunchAtLoginAdapter for LifecycleLaunchAdapter {
    fn observe(&self) -> Result<bool, String> {
        Ok(self
            .observations
            .lock()
            .unwrap()
            .pop()
            .expect("lifecycle launch observation exhausted"))
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.mutations.lock().unwrap().push(enabled);
        match &self.set_failure {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("chiu-main-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn launch_at_login_reconciles_restored_settings_without_shutdown_mutation() {
    let directory = TestDirectory::new("launch-startup-order");
    SettingsService::load(directory.0.clone(), unavailable_log())
        .update(SettingsUpdate::LaunchAtLogin(false))
        .unwrap();
    let adapter = LifecycleLaunchAdapter::new(vec![true, false]);
    let initialized_adapter = adapter.clone();
    let mut runtime = build_runtime(Some(directory.0.clone()), move |settings, log| {
        assert!(!settings.snapshot().launch_at_login());
        Ok(LaunchAtLoginService::available(
            settings,
            initialized_adapter,
            log,
        ))
    });

    runtime.start().unwrap();

    assert!(adapter.observations.lock().unwrap().is_empty());
    assert_eq!(*adapter.mutations.lock().unwrap(), vec![false]);
    runtime.shutdown();
    assert_eq!(*adapter.mutations.lock().unwrap(), vec![false]);
}

#[test]
fn planned_unconfigured_updater_does_not_degrade_startup() {
    let directory = TestDirectory::new("updater-unconfigured");
    let mut runtime = build_runtime(Some(directory.0.clone()), test_launch_at_login);

    let startup = runtime.start().unwrap();

    assert!(
        startup
            .report()
            .optional_failures()
            .iter()
            .all(|failure| failure.capability() != CapabilityId::new("updater"))
    );
    runtime.shutdown();
}

#[test]
fn successful_start_activates_the_exact_tray_owned_by_the_runtime() {
    let directory = TestDirectory::new("exact-runtime-tray");
    let mut runtime = build_runtime(Some(directory.0.clone()), test_launch_at_login);
    let tray = runtime.tray();

    runtime.start().unwrap();

    assert!(tray.view().commands_enabled);
    assert!(tray.view().automatic_checked);
    assert_eq!(
        tray.execute(TrayCommand::ToggleAutomaticProtection),
        TrayCommandOutcome::RefreshRequested
    );
    assert!(!runtime.tray().view().automatic_checked);
    runtime.shutdown();
}

#[test]
fn exit_request_and_event_loop_completion_share_one_product_owner() {
    let mut runtime = build_runtime(None, test_launch_at_login);
    let tray = runtime.tray();
    runtime.start().unwrap();
    let owner = Arc::new(Mutex::new(Some(runtime)));

    owner.lock().unwrap().as_mut().unwrap().begin_shutdown();
    assert_eq!(tray.view().primary, "Shutting down…");

    let report = finish_product_shutdown(&owner);
    assert!(report.failures().is_empty());
    assert_eq!(tray.view().primary, "Shutting down…");
}

#[test]
fn centralized_shutdown_is_idempotent_for_updater_and_normal_exit_paths() {
    let (events, observed_events) = std::sync::mpsc::channel();
    let (updater, updater_runtime) = updater::shutdown_test_registration();
    let registered_updater = updater.clone();
    let (log, lines) = LocalLog::recording();
    let mut runtime =
        ProductRuntime::build(None, log.clone(), test_launch_at_login, move |_, _| {
            UpdaterRegistration {
                service: registered_updater,
                runtime: updater_runtime,
                failure: None,
            }
        });
    runtime.start().unwrap();
    let owner = Arc::new(Mutex::new(Some(runtime)));
    let presentation = Arc::new(Mutex::new(Some(PresentationRuntime::for_shutdown_test(
        move || events.send("presentation").unwrap(),
    ))));
    let shutdown = ShutdownCoordinator::new(owner, presentation, log);

    shutdown.finish();
    shutdown.finish();

    assert_eq!(observed_events.recv().unwrap(), "presentation");
    assert!(observed_events.try_recv().is_err());
    assert!(!updater.is_accepting());
    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted.matches("application.shutdown-requested").count(),
        1
    );
    assert_eq!(persisted.matches("process.stopped").count(), 1);
}
