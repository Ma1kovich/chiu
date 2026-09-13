use crate::{
    application::{ApplicationState, ApplicationStatus, CapabilityId, StartupFailurePoint},
    automatic_download::{AutomaticDownloadMonitor, AutomaticDownloadState},
    automatic_protection::{
        AutomaticProtectionController, AutomaticProtectionSuppression, PowerSource,
    },
    launch_at_login::{LaunchAtLoginFailureStage, LaunchAtLoginService, Registration},
    local_log::{EventOutcome, LocalLog, SafeEvent},
    manual_session::{ManualSession, ManualSessionState},
    network_activity::{
        NetworkActivityFailure, NetworkActivitySource, NetworkActivitySourceStatus,
        NetworkInterfaceContinuity,
    },
    power_source::{PowerSourceObserver, PowerSourceObserverHealth, PowerSourceOperation},
    settings::{
        ArtifactWarning, FieldRecovery, LoadCondition, PersistenceCondition,
        SettingsPersistenceFailure, SettingsService,
    },
    updater::{UpdateAvailability, UpdateStatus, UpdaterService},
    wake_coordinator::{ProtectionState, WakeCoordinator, WakeReason},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

mod report;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticsAvailability {
    pub(crate) copy: bool,
    pub(crate) open_logs: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticActionFailure {
    Unavailable,
    Snapshot,
    Clipboard,
    OpenLogs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicationFacts {
    status: ApplicationStatus,
    unavailable_capabilities: Vec<CapabilityId>,
    startup_failure: Option<StartupFailurePoint>,
    cleanup_failures: Vec<CapabilityId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SettingsFacts {
    automatic_download_protection: bool,
    protect_on_battery: bool,
    launch_at_login: bool,
    automatic_update_checks: bool,
    threshold_bytes_per_second: u64,
    qualification: std::time::Duration,
    grace: std::time::Duration,
    load_condition: LoadCondition,
    persistence: PersistenceCondition,
    field_recoveries: Vec<FieldRecovery>,
    artifact_warnings: Vec<ArtifactWarning>,
    last_save_failure: Option<SettingsPersistenceFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManualFacts {
    state: ManualSessionState,
    background_failure: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DetectorFacts {
    state: AutomaticDownloadState,
    latest_receive_rate_bytes_per_second: Option<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AutomaticFacts {
    preference_enabled: bool,
    protect_on_battery: bool,
    detector_intent: bool,
    desired_automatic_reason: bool,
    suppression: Option<AutomaticProtectionSuppression>,
    coordination_failure: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PowerFailureFacts {
    operation: PowerSourceOperation,
    code: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PowerFacts {
    source: PowerSource,
    health: PowerSourceObserverHealth,
    failure: Option<PowerFailureFacts>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WakeFacts {
    active_reasons: Vec<WakeReason>,
    protection: ProtectionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NetworkFacts {
    status: NetworkActivitySourceStatus,
    failure: Option<NetworkActivityFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceFacts {
    id: u64,
    received_bytes: u64,
    recent_delta: Option<u64>,
    included: bool,
    continuity: NetworkInterfaceContinuity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LaunchAtLoginFacts {
    available: bool,
    desired: bool,
    observed: Registration,
    failure_stage: Option<LaunchAtLoginFailureStage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UpdaterFacts {
    availability: UpdateAvailability,
    automatic_checks: bool,
    status: UpdateStatus,
}

#[derive(Clone)]
pub(crate) struct DiagnosticInput {
    application: ApplicationFacts,
    settings: Option<SettingsFacts>,
    manual: ManualFacts,
    detector: Option<DetectorFacts>,
    automatic: Option<AutomaticFacts>,
    power: Option<PowerFacts>,
    wake: WakeFacts,
    network: NetworkFacts,
    interfaces: Vec<InterfaceFacts>,
    launch_at_login: Option<LaunchAtLoginFacts>,
    updater: Option<UpdaterFacts>,
}

impl DiagnosticInput {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        application: &ApplicationState,
        manual: &ManualSession,
        network: &NetworkActivitySource,
        wake: &WakeCoordinator,
        settings: Option<&SettingsService>,
        detector: Option<&AutomaticDownloadMonitor>,
        automatic: Option<&AutomaticProtectionController>,
        power: Option<&PowerSourceObserver>,
        launch_at_login: Option<&LaunchAtLoginService>,
        updater: Option<&UpdaterService>,
    ) -> Self {
        let application = application.snapshot();
        let manual = manual.snapshot();
        let network = network.snapshot();
        let settings = settings.map(|settings| {
            let snapshot = settings.snapshot();
            let health = settings.health();
            let tuning = snapshot.automatic_download();
            SettingsFacts {
                automatic_download_protection: snapshot.automatic_download_protection_enabled(),
                protect_on_battery: snapshot.protect_on_battery(),
                launch_at_login: snapshot.launch_at_login(),
                automatic_update_checks: snapshot.automatic_update_checks_enabled(),
                threshold_bytes_per_second: tuning.meaningful_receive_rate_bytes_per_second(),
                qualification: tuning.activation_qualification(),
                grace: tuning.grace(),
                load_condition: health.load_condition(),
                persistence: health.persistence(),
                field_recoveries: health.field_recoveries().to_vec(),
                artifact_warnings: health.artifact_warnings().to_vec(),
                last_save_failure: health.last_save_failure(),
            }
        });
        let detector = detector.map(|detector| {
            let snapshot = detector.snapshot();
            DetectorFacts {
                state: snapshot.state,
                latest_receive_rate_bytes_per_second: snapshot.latest_receive_rate_bytes_per_second,
            }
        });
        let automatic = automatic.map(|automatic| {
            let snapshot = automatic.snapshot();
            AutomaticFacts {
                preference_enabled: snapshot.settings().automatic_download_protection_enabled(),
                protect_on_battery: snapshot.settings().protect_on_battery(),
                detector_intent: snapshot.detector_intent(),
                desired_automatic_reason: snapshot.desired_automatic_reason(),
                suppression: snapshot.suppression_reason(),
                coordination_failure: snapshot.latest_coordination_failure().is_some(),
            }
        });
        let power = power.map(|power| {
            let snapshot = power.snapshot();
            PowerFacts {
                source: snapshot.source(),
                health: snapshot.health(),
                failure: snapshot.latest_failure().map(|failure| PowerFailureFacts {
                    operation: failure.operation(),
                    code: failure.code(),
                }),
            }
        });
        let launch_at_login = launch_at_login.map(|launch| {
            let snapshot = launch.snapshot();
            LaunchAtLoginFacts {
                available: snapshot.available(),
                desired: snapshot.desired(),
                observed: snapshot.observed(),
                failure_stage: snapshot.latest_failure().map(|failure| failure.stage()),
            }
        });
        let updater = updater.map(|updater| {
            let snapshot = updater.snapshot();
            UpdaterFacts {
                availability: snapshot.availability,
                automatic_checks: snapshot.automatic_checks_enabled,
                status: snapshot.status,
            }
        });
        let interfaces = network
            .interfaces()
            .iter()
            .map(|interface| InterfaceFacts {
                id: interface.id().value(),
                received_bytes: interface.last_observed_receive_bytes(),
                recent_delta: interface.recent_delta(),
                included: interface.included_in_activity(),
                continuity: interface.continuity(),
            })
            .collect::<Vec<_>>();
        let wake = wake.snapshot();
        Self {
            application: ApplicationFacts {
                status: application.status(),
                unavailable_capabilities: application.unavailable_capabilities().to_vec(),
                startup_failure: application.startup_failure(),
                cleanup_failures: application.cleanup_failures().to_vec(),
            },
            settings,
            manual: ManualFacts {
                state: manual.state,
                background_failure: manual.background_failure.is_some(),
            },
            detector,
            automatic,
            power,
            wake: WakeFacts {
                active_reasons: wake.active_reasons().to_vec(),
                protection: wake.protection_state(),
            },
            network: NetworkFacts {
                status: network.status(),
                failure: network.latest_failure(),
            },
            interfaces,
            launch_at_login,
            updater,
        }
    }

    #[cfg(test)]
    fn minimal(status: ApplicationStatus) -> Self {
        Self {
            application: ApplicationFacts {
                status,
                unavailable_capabilities: Vec::new(),
                startup_failure: None,
                cleanup_failures: Vec::new(),
            },
            settings: None,
            manual: ManualFacts {
                state: ManualSessionState::Inactive,
                background_failure: false,
            },
            detector: None,
            automatic: None,
            power: None,
            wake: WakeFacts {
                active_reasons: Vec::new(),
                protection: ProtectionState::Inactive,
            },
            network: NetworkFacts {
                status: NetworkActivitySourceStatus::NotStarted,
                failure: None,
            },
            interfaces: Vec::new(),
            launch_at_login: None,
            updater: None,
        }
    }
}

pub(crate) trait DiagnosticSnapshotSource: Send + Sync {
    fn capture(&self) -> Result<DiagnosticInput, ()>;
}

trait DiagnosticClock: Send + Sync {
    fn generated_at(&self) -> String;
}

struct UtcDiagnosticClock;

impl DiagnosticClock for UtcDiagnosticClock {
    fn generated_at(&self) -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
    }
}

trait ClipboardWriter: Send + Sync {
    fn write_text(&self, text: &str) -> Result<(), ()>;
}

trait DirectoryOpener: Send + Sync {
    fn open(&self, directory: &Path) -> Result<(), ()>;
}

struct PlatformClipboard;

impl ClipboardWriter for PlatformClipboard {
    fn write_text(&self, text: &str) -> Result<(), ()> {
        crate::clipboard::write_text(text)
    }
}

struct TauriDirectoryOpener;

impl DirectoryOpener for TauriDirectoryOpener {
    fn open(&self, directory: &Path) -> Result<(), ()> {
        tauri_plugin_opener::open_path(directory, None::<&str>).map_err(|_| ())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    family: String,
    version: String,
    architecture: String,
}

impl PlatformInfo {
    fn current() -> Self {
        Self {
            family: tauri_plugin_os::platform().to_owned(),
            version: tauri_plugin_os::version().to_string(),
            architecture: tauri_plugin_os::arch().to_owned(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticsService {
    source: Arc<dyn DiagnosticSnapshotSource>,
    clipboard: Option<Arc<dyn ClipboardWriter>>,
    opener: Arc<dyn DirectoryOpener>,
    log: LocalLog,
    clock: Arc<dyn DiagnosticClock>,
    platform: PlatformInfo,
    accepting: Arc<AtomicBool>,
}

impl DiagnosticsService {
    pub(crate) fn register(source: Arc<dyn DiagnosticSnapshotSource>, log: LocalLog) -> Self {
        Self::new(
            source,
            Some(Arc::new(PlatformClipboard)),
            Arc::new(TauriDirectoryOpener),
            log,
            Arc::new(UtcDiagnosticClock),
            PlatformInfo::current(),
        )
    }

    fn new(
        source: Arc<dyn DiagnosticSnapshotSource>,
        clipboard: Option<Arc<dyn ClipboardWriter>>,
        opener: Arc<dyn DirectoryOpener>,
        log: LocalLog,
        clock: Arc<dyn DiagnosticClock>,
        platform: PlatformInfo,
    ) -> Self {
        Self {
            source,
            clipboard,
            opener,
            log,
            clock,
            platform,
            accepting: Arc::new(AtomicBool::new(true)),
        }
    }

    #[cfg(test)]
    pub(crate) fn unavailable_for_tests() -> Self {
        Self::new(
            Arc::new(UnavailableSource),
            None,
            Arc::new(TauriDirectoryOpener),
            LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open),
            Arc::new(UtcDiagnosticClock),
            PlatformInfo {
                family: "test".to_owned(),
                version: "test".to_owned(),
                architecture: "test".to_owned(),
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn available_for_tests(log: LocalLog) -> Self {
        Self::new(
            Arc::new(TestSource),
            Some(Arc::new(TestClipboard)),
            Arc::new(TestOpener),
            log,
            Arc::new(UtcDiagnosticClock),
            PlatformInfo {
                family: "test".to_owned(),
                version: "test".to_owned(),
                architecture: "test".to_owned(),
            },
        )
    }

    pub(crate) fn availability(&self) -> DiagnosticsAvailability {
        let accepting = self.accepting.load(Ordering::Acquire);
        DiagnosticsAvailability {
            copy: accepting && self.clipboard.is_some(),
            open_logs: accepting && self.log.directory().is_some(),
        }
    }

    pub(crate) fn copy(&self) -> Result<String, DiagnosticActionFailure> {
        if !self.availability().copy {
            self.log.record(SafeEvent::DiagnosticsAction {
                action: "copy",
                outcome: EventOutcome::Rejected,
            });
            return Err(DiagnosticActionFailure::Unavailable);
        }
        let result = self.report().and_then(|text| {
            self.clipboard
                .as_ref()
                .ok_or(DiagnosticActionFailure::Unavailable)?
                .write_text(&text)
                .map_err(|_| DiagnosticActionFailure::Clipboard)?;
            Ok(text)
        });
        self.log.record(SafeEvent::DiagnosticsAction {
            action: "copy",
            outcome: if result.is_ok() {
                EventOutcome::Succeeded
            } else {
                EventOutcome::Failed
            },
        });
        result
    }

    pub(crate) fn open_logs(&self) -> Result<(), DiagnosticActionFailure> {
        if !self.availability().open_logs {
            self.log.record(SafeEvent::DiagnosticsAction {
                action: "open-logs",
                outcome: EventOutcome::Rejected,
            });
            return Err(DiagnosticActionFailure::Unavailable);
        }
        let directory = self
            .log
            .directory()
            .ok_or(DiagnosticActionFailure::Unavailable)?;
        let result = self
            .opener
            .open(directory)
            .map_err(|_| DiagnosticActionFailure::OpenLogs);
        self.log.record(SafeEvent::DiagnosticsAction {
            action: "open-logs",
            outcome: if result.is_ok() {
                EventOutcome::Succeeded
            } else {
                EventOutcome::Failed
            },
        });
        result
    }

    pub(crate) fn begin_shutdown(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    pub(crate) fn report(&self) -> Result<String, DiagnosticActionFailure> {
        let input = self
            .source
            .capture()
            .map_err(|_| DiagnosticActionFailure::Snapshot)?;
        Ok(report::render(
            &input,
            &self.clock.generated_at(),
            &self.platform,
            self.log.health(),
        ))
    }
}

#[cfg(test)]
struct UnavailableSource;

#[cfg(test)]
struct TestSource;

#[cfg(test)]
impl DiagnosticSnapshotSource for TestSource {
    fn capture(&self) -> Result<DiagnosticInput, ()> {
        Ok(DiagnosticInput::minimal(ApplicationStatus::Failed))
    }
}

#[cfg(test)]
struct TestClipboard;

#[cfg(test)]
impl ClipboardWriter for TestClipboard {
    fn write_text(&self, _: &str) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(test)]
struct TestOpener;

#[cfg(test)]
impl DirectoryOpener for TestOpener {
    fn open(&self, _: &Path) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(test)]
impl DiagnosticSnapshotSource for UnavailableSource {
    fn capture(&self) -> Result<DiagnosticInput, ()> {
        Err(())
    }
}

#[cfg(test)]
mod tests;
