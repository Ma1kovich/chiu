use crate::{
    application::{
        ApplicationLifecycle, ApplicationState, CapabilityId, LifecyclePlan, OwnedResource,
        ShutdownReport, StartError, StartupReport,
    },
    automatic_download::{AutomaticDownloadMonitor, AutomaticDownloadTuningService},
    automatic_protection::{
        AutomaticProtectionController, AutomaticProtectionService, AutomaticProtectionSettings,
    },
    diagnostics::{DiagnosticInput, DiagnosticSnapshotSource, DiagnosticsService},
    launch_at_login::{LaunchAtLoginFailure, LaunchAtLoginService},
    local_log::LocalLog,
    manual_session::ManualSession,
    network_activity::NetworkActivitySource,
    power_source::PowerSourceObserver,
    settings::{SettingsHealth, SettingsService},
    tray::{TrayApplication, TrayProduct},
    updater::{UpdaterRegistration, UpdaterService},
    wake_coordinator::WakeCoordinator,
};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

const AUTOMATIC_DOWNLOAD_CAPABILITY: CapabilityId = CapabilityId::new("automatic-download");
const LAUNCH_AT_LOGIN_CAPABILITY: CapabilityId = CapabilityId::new("launch-at-login");
const MANUAL_SESSION_CAPABILITY: CapabilityId = CapabilityId::new("manual-session");
const NETWORK_ACTIVITY_CAPABILITY: CapabilityId = CapabilityId::new("network-activity");
const POWER_SOURCE_CAPABILITY: CapabilityId = CapabilityId::new("power-source");
const UPDATER_CAPABILITY: CapabilityId = CapabilityId::new("updater");
const WAKE_COORDINATION_CAPABILITY: CapabilityId = CapabilityId::new("wake-coordination");

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct AutomaticSubsystem {
    monitor: AutomaticDownloadMonitor,
    tuning: AutomaticDownloadTuningService,
    controller: AutomaticProtectionController,
    settings: AutomaticProtectionService,
    power_source: PowerSourceObserver,
}

#[derive(Clone)]
struct BootstrapSlots {
    settings: Arc<OnceLock<SettingsService>>,
    automatic: Arc<OnceLock<AutomaticSubsystem>>,
    launch_at_login: Arc<OnceLock<LaunchAtLoginService>>,
    updater: Arc<OnceLock<UpdaterService>>,
}

#[derive(Clone)]
struct ProductComposition {
    slots: BootstrapSlots,
    manual: ManualSession,
    network: NetworkActivitySource,
    wake: WakeCoordinator,
}

impl ProductComposition {
    fn operational(&self) -> Option<TrayProduct> {
        let automatic = self.slots.automatic.get()?;
        Some(TrayProduct::new(
            self.manual.clone(),
            automatic.monitor.clone(),
            automatic.tuning.clone(),
            automatic.controller.clone(),
            automatic.settings.clone(),
            self.network.clone(),
            automatic.power_source.clone(),
            self.slots.launch_at_login.get()?.clone(),
            self.slots.updater.get()?.clone(),
            self.slots.settings.get()?.clone(),
            self.wake.clone(),
        ))
    }

    fn diagnostic_source(
        &self,
        application: ApplicationState,
    ) -> Arc<dyn DiagnosticSnapshotSource> {
        Arc::new(ApplicationDiagnosticSource {
            application,
            composition: self.clone(),
        })
    }

    fn begin_shutdown(&self) {
        if let Some(updater) = self.slots.updater.get() {
            updater.begin_shutdown();
        }
    }
}

#[derive(Debug)]
pub(crate) struct RuntimeStartup {
    report: StartupReport,
    settings_health: SettingsHealth,
}

impl RuntimeStartup {
    pub(crate) fn report(&self) -> &StartupReport {
        &self.report
    }

    pub(crate) fn settings_health(&self) -> &SettingsHealth {
        &self.settings_health
    }
}

pub(crate) struct ProductRuntime {
    lifecycle: ApplicationLifecycle<SettingsService>,
    composition: ProductComposition,
    diagnostics: DiagnosticsService,
    tray: TrayApplication,
}

impl ProductRuntime {
    pub(crate) fn build<L, U>(
        settings_directory: Option<PathBuf>,
        log: LocalLog,
        initialize_launch_at_login: L,
        initialize_updater: U,
    ) -> Self
    where
        L: FnOnce(SettingsService, LocalLog) -> Result<LaunchAtLoginService, LaunchAtLoginFailure>
            + Send
            + 'static,
        U: FnOnce(SettingsService, LocalLog) -> UpdaterRegistration + Send + 'static,
    {
        let wake = WakeCoordinator::new(log.clone());
        let manual = ManualSession::new(wake.clone(), log.clone());
        Self::build_with_services(
            settings_directory,
            log,
            wake,
            manual,
            initialize_launch_at_login,
            start_network_activity,
            initialize_updater,
        )
    }

    fn build_with_services<L, N, U>(
        settings_directory: Option<PathBuf>,
        log: LocalLog,
        wake: WakeCoordinator,
        manual: ManualSession,
        initialize_launch_at_login: L,
        start_network: N,
        initialize_updater: U,
    ) -> Self
    where
        L: FnOnce(SettingsService, LocalLog) -> Result<LaunchAtLoginService, LaunchAtLoginFailure>
            + Send
            + 'static,
        N: FnOnce(
                NetworkActivitySource,
                AutomaticDownloadMonitor,
            ) -> Result<OwnedResource, BoxError>
            + Send
            + 'static,
        U: FnOnce(SettingsService, LocalLog) -> UpdaterRegistration + Send + 'static,
    {
        let (plan, composition) = lifecycle_plan(
            settings_directory,
            wake,
            manual,
            log.clone(),
            initialize_launch_at_login,
            start_network,
            initialize_updater,
        );
        let lifecycle = plan.build();
        let diagnostics =
            DiagnosticsService::register(composition.diagnostic_source(lifecycle.state()), log);
        let tray = TrayApplication::new(lifecycle.state(), diagnostics.clone());
        Self {
            lifecycle,
            composition,
            diagnostics,
            tray,
        }
    }

    pub(crate) fn tray(&self) -> TrayApplication {
        self.tray.clone()
    }

    pub(crate) fn start(&mut self) -> Result<RuntimeStartup, StartError> {
        let report = self.lifecycle.start()?;
        let product = self
            .composition
            .operational()
            .expect("successful startup initialized product services");
        let settings_health = product.settings_health();
        self.tray.activate(product);
        Ok(RuntimeStartup {
            report,
            settings_health,
        })
    }

    pub(crate) fn begin_shutdown(&mut self) {
        self.diagnostics.begin_shutdown();
        self.composition.begin_shutdown();
        self.lifecycle.request_shutdown();
    }

    pub(crate) fn shutdown(&mut self) -> ShutdownReport {
        self.begin_shutdown();
        self.lifecycle.shutdown()
    }
}

fn start_network_activity(
    network: NetworkActivitySource,
    monitor: AutomaticDownloadMonitor,
) -> Result<OwnedResource, BoxError> {
    let mut runtime = network
        .start(monitor)
        .map_err(|error| Box::new(error) as BoxError)?;
    Ok(OwnedResource::new(move || {
        runtime
            .shutdown()
            .map_err(|error| Box::new(error) as BoxError)
    }))
}

#[derive(Clone)]
struct ApplicationDiagnosticSource {
    application: ApplicationState,
    composition: ProductComposition,
}

impl DiagnosticSnapshotSource for ApplicationDiagnosticSource {
    fn capture(&self) -> Result<DiagnosticInput, ()> {
        let automatic = self.composition.slots.automatic.get();
        Ok(DiagnosticInput::capture(
            &self.application,
            &self.composition.manual,
            &self.composition.network,
            &self.composition.wake,
            self.composition.slots.settings.get(),
            automatic.map(|value| &value.monitor),
            automatic.map(|value| &value.controller),
            automatic.map(|value| &value.power_source),
            self.composition.slots.launch_at_login.get(),
            self.composition.slots.updater.get(),
        ))
    }
}

fn lifecycle_plan<L, N, U>(
    settings_directory: Option<PathBuf>,
    wake_coordinator: WakeCoordinator,
    manual_session: ManualSession,
    log: LocalLog,
    initialize_launch_at_login: L,
    start_network: N,
    initialize_updater: U,
) -> (LifecyclePlan<SettingsService>, ProductComposition)
where
    L: FnOnce(SettingsService, LocalLog) -> Result<LaunchAtLoginService, LaunchAtLoginFailure>
        + Send
        + 'static,
    N: FnOnce(NetworkActivitySource, AutomaticDownloadMonitor) -> Result<OwnedResource, BoxError>
        + Send
        + 'static,
    U: FnOnce(SettingsService, LocalLog) -> UpdaterRegistration + Send + 'static,
{
    let slots = BootstrapSlots {
        settings: Arc::new(OnceLock::new()),
        automatic: Arc::new(OnceLock::new()),
        launch_at_login: Arc::new(OnceLock::new()),
        updater: Arc::new(OnceLock::new()),
    };
    let composition = ProductComposition {
        slots: slots.clone(),
        manual: manual_session.clone(),
        network: NetworkActivitySource::new(log.clone()),
        wake: wake_coordinator.clone(),
    };
    let load_settings = slots.settings.clone();
    let initialize_automatic = slots.automatic.clone();
    let start_power_source = slots.automatic.clone();
    let initialize_launch = slots.launch_at_login.clone();
    let start_network_monitor = slots.automatic.clone();
    let network_activity = composition.network.clone();
    let initialize_updater_handle = slots.updater.clone();
    let detector_coordinator = wake_coordinator.clone();
    let settings_log = log.clone();
    let automatic_log = log.clone();
    let launch_log = log.clone();
    let updater_log = log;
    let plan = LifecyclePlan::new(move || {
        let settings = match settings_directory {
            Some(directory) => SettingsService::load(directory, settings_log.clone()),
            None => SettingsService::unavailable(settings_log.clone()),
        };
        load_settings.set(settings.clone()).map_err(|_| {
            Box::new(std::io::Error::other("settings initialized more than once")) as BoxError
        })?;
        Ok(settings)
    })
    .required(WAKE_COORDINATION_CAPABILITY, move |_: &SettingsService| {
        Ok(OwnedResource::new(move || {
            wake_coordinator
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    })
    .required(MANUAL_SESSION_CAPABILITY, move |_: &SettingsService| {
        let scheduler = manual_session
            .start_scheduler()
            .map_err(|error| Box::new(error) as BoxError)?;
        Ok(OwnedResource::new(move || {
            scheduler
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    })
    .required(
        AUTOMATIC_DOWNLOAD_CAPABILITY,
        move |settings: &SettingsService| {
            let restored = settings.snapshot();
            let automatic_protection = AutomaticProtectionController::new(
                AutomaticProtectionSettings::new(
                    restored.automatic_download_protection_enabled(),
                    restored.protect_on_battery(),
                ),
                detector_coordinator,
                automatic_log.clone(),
            );
            let automatic_protection_service =
                AutomaticProtectionService::new(settings.clone(), automatic_protection.clone());
            let automatic_download = AutomaticDownloadMonitor::new(
                restored.automatic_download(),
                automatic_protection.clone(),
                automatic_log.clone(),
            );
            let automatic_download_tuning =
                AutomaticDownloadTuningService::new(settings.clone(), automatic_download.clone());
            let power_source =
                PowerSourceObserver::new(automatic_protection.clone(), automatic_log.clone());
            initialize_automatic
                .set(AutomaticSubsystem {
                    monitor: automatic_download.clone(),
                    tuning: automatic_download_tuning,
                    controller: automatic_protection,
                    settings: automatic_protection_service,
                    power_source,
                })
                .map_err(|_| {
                    Box::new(std::io::Error::other(
                        "automatic subsystem initialized more than once",
                    )) as BoxError
                })?;
            Ok(OwnedResource::new(move || {
                automatic_download
                    .shutdown()
                    .map_err(|error| Box::new(error) as BoxError)
            }))
        },
    )
    .optional(
        LAUNCH_AT_LOGIN_CAPABILITY,
        move |settings: &SettingsService| {
            let service = match initialize_launch_at_login(settings.clone(), launch_log.clone()) {
                Ok(service) => service,
                Err(failure) => {
                    let service = LaunchAtLoginService::unavailable(
                        settings.clone(),
                        failure.clone(),
                        launch_log.clone(),
                    );
                    initialize_launch.set(service).map_err(|_| {
                        Box::new(std::io::Error::other(
                            "launch-at-login facade initialized more than once",
                        )) as BoxError
                    })?;
                    return Err(Box::new(failure) as BoxError);
                }
            };
            initialize_launch.set(service.clone()).map_err(|_| {
                Box::new(std::io::Error::other(
                    "launch-at-login facade initialized more than once",
                )) as BoxError
            })?;
            if let Err(error) = service.reconcile() {
                eprintln!("Chiù launch-at-login reconciliation failed: {error}");
            }
            Ok(OwnedResource::new(|| Ok(())))
        },
    )
    .optional(POWER_SOURCE_CAPABILITY, move |_: &SettingsService| {
        let observer = start_power_source
            .get()
            .map(|automatic| automatic.power_source.clone())
            .ok_or_else(|| {
                Box::new(std::io::Error::other(
                    "automatic policy was not initialized before power observation",
                )) as BoxError
            })?;
        let mut runtime = observer
            .start()
            .map_err(|error| Box::new(error) as BoxError)?;
        Ok(OwnedResource::new(move || {
            runtime
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    })
    .optional(NETWORK_ACTIVITY_CAPABILITY, move |_: &SettingsService| {
        let monitor = start_network_monitor
            .get()
            .map(|automatic| automatic.monitor.clone())
            .ok_or_else(|| {
                Box::new(std::io::Error::other(
                    "automatic detection was not initialized before network startup",
                )) as BoxError
            })?;
        start_network(network_activity, monitor)
    })
    .optional(UPDATER_CAPABILITY, move |settings: &SettingsService| {
        let registration = initialize_updater(settings.clone(), updater_log);
        initialize_updater_handle
            .set(registration.service.clone())
            .map_err(|_| {
                Box::new(std::io::Error::other(
                    "updater facade initialized more than once",
                )) as BoxError
            })?;
        if let Some(failure) = registration.failure {
            return Err(Box::new(failure) as BoxError);
        }
        let mut runtime = registration.runtime;
        Ok(OwnedResource::new(move || {
            runtime
                .shutdown()
                .map_err(|error| Box::new(error) as BoxError)
        }))
    });
    (plan, composition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{ApplicationStatus, StartupFailurePoint},
        automatic_download::{AutomaticDownloadMonitor, AutomaticDownloadState},
        launch_at_login::LaunchAtLoginAdapter,
        local_log::LoggingFailureStage,
        network_activity::{NetworkActivityConsumer, NetworkActivityEvent, ReceiveActivitySample},
        settings::SettingsUpdate,
        wake_coordinator::WakeReason,
    };
    use std::{fs, sync::Mutex, time::Duration};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "chiu-product-composition-{name}-{}",
                std::process::id()
            ));
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

    struct FailingLaunchAdapter(Mutex<Vec<bool>>);

    impl LaunchAtLoginAdapter for FailingLaunchAdapter {
        fn observe(&self) -> Result<bool, String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .pop()
                .expect("launch observation exhausted"))
        }

        fn set_enabled(&self, _: bool) -> Result<(), String> {
            Err("scripted enable failure".to_owned())
        }
    }

    fn unavailable_log() -> LocalLog {
        LocalLog::unavailable(LoggingFailureStage::Open)
    }

    fn unconfigured_updater(settings: SettingsService, _log: LocalLog) -> UpdaterRegistration {
        UpdaterRegistration {
            service: UpdaterService::unconfigured(&settings),
            runtime: crate::updater::UpdaterRuntime::empty(),
            failure: None,
        }
    }

    fn build_test_runtime<L, U>(
        settings_directory: Option<PathBuf>,
        initialize_launch_at_login: L,
        initialize_updater: U,
    ) -> ProductRuntime
    where
        L: FnOnce(SettingsService, LocalLog) -> Result<LaunchAtLoginService, LaunchAtLoginFailure>
            + Send
            + 'static,
        U: FnOnce(SettingsService, LocalLog) -> UpdaterRegistration + Send + 'static,
    {
        let log = unavailable_log();
        let wake = WakeCoordinator::new(log.clone());
        let manual = ManualSession::new(wake.clone(), log.clone());
        ProductRuntime::build_with_services(
            settings_directory,
            log,
            wake,
            manual,
            initialize_launch_at_login,
            |_, _| Ok(OwnedResource::new(|| Ok(()))),
            initialize_updater,
        )
    }

    #[test]
    fn diagnostics_remain_bounded_and_truthful_across_partial_required_startup() {
        let log = unavailable_log();
        let wake = WakeCoordinator::new(log.clone());
        let manual = ManualSession::new(wake.clone(), log.clone());
        let scheduler = manual
            .start_scheduler()
            .expect("prestarted scheduler should force the required startup failure");
        let mut runtime = ProductRuntime::build_with_services(
            None,
            log,
            wake,
            manual,
            crate::test_launch_at_login,
            |_, _| Ok(OwnedResource::new(|| Ok(()))),
            unconfigured_updater,
        );

        let starting = runtime.diagnostics.report().unwrap();
        assert!(starting.len() <= 64 * 1024);
        assert!(starting.contains("[application]\nstatus=starting"));
        assert!(starting.contains("[settings]\nstatus=not-initialized"));
        assert!(starting.contains("[updater]\nstatus=not-initialized"));

        let error = runtime.start().unwrap_err();
        assert_eq!(
            error.failure_point(),
            Some(StartupFailurePoint::Required(MANUAL_SESSION_CAPABILITY))
        );
        let failed = runtime.diagnostics.report().unwrap();
        assert!(failed.len() <= 64 * 1024);
        assert!(failed.contains("[application]\nstatus=failed"));
        assert!(failed.contains("startup_failure=required-capability:manual-session"));
        assert!(failed.contains("[settings]\nautomatic_download_protection="));
        assert!(failed.contains("[detector]\nstatus=not-initialized"));
        assert!(failed.contains("[updater]\nstatus=not-initialized"));
        scheduler.shutdown().unwrap();
    }

    #[test]
    fn optional_failure_keeps_diagnostics_and_the_exact_tray_operational() {
        let mut runtime = build_test_runtime(
            None,
            |_, _| Err(LaunchAtLoginFailure::initialize("launch unavailable")),
            unconfigured_updater,
        );
        let tray = runtime.tray();

        let startup = runtime.start().unwrap();

        assert_eq!(
            runtime.lifecycle.state().snapshot().status(),
            ApplicationStatus::Degraded
        );
        assert!(
            startup
                .report()
                .optional_failures()
                .iter()
                .any(|failure| failure.capability() == LAUNCH_AT_LOGIN_CAPABILITY)
        );
        assert!(tray.view().commands_enabled);
        let report = runtime.diagnostics.report().unwrap();
        assert!(report.contains("[launch_at_login]\navailable=false"));
        assert!(report.contains("[updater]\navailability=unconfigured"));
        runtime.shutdown();
    }

    #[test]
    fn launch_reconciliation_failure_keeps_exact_diagnostic_stage() {
        let directory = TestDirectory::new("launch-reconciliation");
        let mut runtime = build_test_runtime(
            Some(directory.0.clone()),
            |settings, log| {
                Ok(LaunchAtLoginService::available(
                    settings,
                    FailingLaunchAdapter(Mutex::new(vec![false, false])),
                    log,
                ))
            },
            unconfigured_updater,
        );

        let startup = runtime.start().unwrap();

        assert!(
            startup
                .report()
                .optional_failures()
                .iter()
                .all(|failure| failure.capability() != LAUNCH_AT_LOGIN_CAPABILITY)
        );
        assert!(runtime.diagnostics.report().unwrap().contains(
            "[launch_at_login]\navailable=true\ndesired=true\nobserved=disabled\nfailure_stage=enable"
        ));
        runtime.shutdown();
    }

    #[test]
    fn configured_updater_initialization_failure_keeps_exact_diagnostic_availability() {
        let directory = TestDirectory::new("updater-initialization");
        let mut runtime = build_test_runtime(
            Some(directory.0.clone()),
            crate::test_launch_at_login,
            |settings, _| UpdaterRegistration {
                service: UpdaterService::initialization_failed(&settings),
                runtime: crate::updater::UpdaterRuntime::empty(),
                failure: Some(crate::updater::UpdaterInitializationFailure),
            },
        );

        let startup = runtime.start().unwrap();

        assert!(
            startup
                .report()
                .optional_failures()
                .iter()
                .any(|failure| failure.capability() == UPDATER_CAPABILITY)
        );
        assert!(
            runtime
                .diagnostics
                .report()
                .unwrap()
                .contains("[updater]\navailability=initialization-failed")
        );
        runtime.shutdown();
    }

    #[test]
    fn restored_policy_is_applied_before_network_activity_can_qualify() {
        let directory = TestDirectory::new("restored-policy");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        settings
            .update(SettingsUpdate::AutomaticDownloadProtectionEnabled(false))
            .unwrap();
        settings
            .update(SettingsUpdate::ProtectOnBattery(true))
            .unwrap();
        settings
            .update(SettingsUpdate::AutomaticDownloadTuning {
                meaningful_receive_rate_bytes_per_second: 100,
                activation_qualification_milliseconds: 1_000,
                grace_milliseconds: 0,
            })
            .unwrap();
        let wake = WakeCoordinator::new(unavailable_log());
        let network_wake = wake.clone();
        let manual = ManualSession::new(wake.clone(), unavailable_log());
        let mut runtime = ProductRuntime::build_with_services(
            Some(directory.0.clone()),
            unavailable_log(),
            wake,
            manual,
            crate::test_launch_at_login,
            move |_, mut monitor: AutomaticDownloadMonitor| {
                monitor.consume(NetworkActivityEvent::Sample(
                    ReceiveActivitySample::try_new(100, Duration::from_secs(1)).unwrap(),
                ));
                assert!(network_wake.snapshot().active_reasons().is_empty());
                Ok(OwnedResource::new(|| Ok(())))
            },
            unconfigured_updater,
        );

        runtime.start().unwrap();

        let report = runtime.diagnostics.report().unwrap();
        assert!(report.contains(
            "[automatic_protection]\npreference_enabled=false\nprotect_on_battery=true\ndetector_intent=true\ndesired_automatic_reason=false\nsuppression=automatic-disabled"
        ));
        runtime.shutdown();
    }

    #[test]
    fn runtime_shutdown_clears_qualified_automatic_intent() {
        let directory = TestDirectory::new("automatic-cleanup");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        settings
            .update(SettingsUpdate::ProtectOnBattery(true))
            .unwrap();
        settings
            .update(SettingsUpdate::AutomaticDownloadTuning {
                meaningful_receive_rate_bytes_per_second: 100,
                activation_qualification_milliseconds: 1_000,
                grace_milliseconds: 0,
            })
            .unwrap();
        let wake = WakeCoordinator::new(unavailable_log());
        let manual = ManualSession::new(wake.clone(), unavailable_log());
        let mut runtime = ProductRuntime::build_with_services(
            Some(directory.0.clone()),
            unavailable_log(),
            wake.clone(),
            manual,
            crate::test_launch_at_login,
            move |_, mut monitor: AutomaticDownloadMonitor| {
                monitor.consume(NetworkActivityEvent::Sample(
                    ReceiveActivitySample::try_new(100, Duration::from_secs(1)).unwrap(),
                ));
                assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
                Ok(OwnedResource::new(|| Ok(())))
            },
            unconfigured_updater,
        );
        runtime.start().unwrap();
        assert_eq!(
            wake.snapshot().active_reasons(),
            &[WakeReason::AutomaticDownload]
        );

        let report = runtime.shutdown();

        assert!(report.failures().is_empty());
        assert!(wake.snapshot().active_reasons().is_empty());
    }

    #[test]
    fn restored_detector_tuning_is_active_before_network_start() {
        let directory = TestDirectory::new("restored-detector");
        SettingsService::load(directory.0.clone(), unavailable_log())
            .update(SettingsUpdate::AutomaticDownloadTuning {
                meaningful_receive_rate_bytes_per_second: 100,
                activation_qualification_milliseconds: 1_000,
                grace_milliseconds: 0,
            })
            .unwrap();
        let observed_state = Arc::new(Mutex::new(None));
        let network_state = observed_state.clone();
        let log = unavailable_log();
        let wake = WakeCoordinator::new(log.clone());
        let manual = ManualSession::new(wake.clone(), log.clone());
        let mut runtime = ProductRuntime::build_with_services(
            Some(directory.0.clone()),
            log,
            wake,
            manual,
            crate::test_launch_at_login,
            move |_, mut monitor: AutomaticDownloadMonitor| {
                monitor.consume(NetworkActivityEvent::Sample(
                    ReceiveActivitySample::try_new(100, Duration::from_secs(1)).unwrap(),
                ));
                *network_state.lock().unwrap() = Some(monitor.snapshot().state);
                Ok(OwnedResource::new(|| Ok(())))
            },
            unconfigured_updater,
        );

        runtime.start().unwrap();

        assert_eq!(
            *observed_state.lock().unwrap(),
            Some(AutomaticDownloadState::Active)
        );
        runtime.shutdown();
    }
}
