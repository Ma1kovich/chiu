use super::*;
use crate::{
    application::LifecyclePlan,
    local_log::{LoggingFailureStage, LoggingHealth},
    tray::{TrayApplication, TrayCommandOutcome},
};
use std::{path::PathBuf, sync::Mutex};

struct FixedClock;

impl DiagnosticClock for FixedClock {
    fn generated_at(&self) -> String {
        "2026-08-10T12:00:00Z".to_owned()
    }
}

struct FixedSource(Result<DiagnosticInput, ()>);

impl DiagnosticSnapshotSource for FixedSource {
    fn capture(&self) -> Result<DiagnosticInput, ()> {
        self.0.clone()
    }
}

#[derive(Default)]
struct RecordingClipboard(Mutex<Vec<String>>);

impl ClipboardWriter for RecordingClipboard {
    fn write_text(&self, text: &str) -> Result<(), ()> {
        self.0.lock().unwrap().push(text.to_owned());
        Ok(())
    }
}

struct FailingClipboard;

impl ClipboardWriter for FailingClipboard {
    fn write_text(&self, _: &str) -> Result<(), ()> {
        Err(())
    }
}

#[derive(Default)]
struct RecordingOpener(Mutex<Vec<PathBuf>>);

impl DirectoryOpener for RecordingOpener {
    fn open(&self, directory: &Path) -> Result<(), ()> {
        self.0.lock().unwrap().push(directory.to_owned());
        Ok(())
    }
}

fn service(
    input: DiagnosticInput,
    clipboard: Option<Arc<RecordingClipboard>>,
    opener: Arc<RecordingOpener>,
    log: LocalLog,
) -> DiagnosticsService {
    DiagnosticsService::new(
        Arc::new(FixedSource(Ok(input))),
        clipboard.map(|clipboard| clipboard as Arc<dyn ClipboardWriter>),
        opener,
        log,
        Arc::new(FixedClock),
        PlatformInfo {
            family: "macos".to_owned(),
            version: "15.6.1".to_owned(),
            architecture: "aarch64".to_owned(),
        },
    )
}

fn representative_input() -> DiagnosticInput {
    DiagnosticInput {
        application: ApplicationFacts {
            status: ApplicationStatus::Degraded,
            unavailable_capabilities: vec![CapabilityId::new("power-source")],
            startup_failure: None,
            cleanup_failures: vec![CapabilityId::new("network-activity")],
        },
        settings: Some(SettingsFacts {
            automatic_download_protection: true,
            protect_on_battery: false,
            launch_at_login: true,
            automatic_update_checks: false,
            threshold_bytes_per_second: 1_048_576,
            qualification: std::time::Duration::from_secs(3),
            grace: std::time::Duration::from_secs(120),
            load_condition: LoadCondition::PartialRecovery,
            persistence: PersistenceCondition::Writable,
            field_recoveries: vec![FieldRecovery::new(
                crate::settings::SettingPath::GraceMilliseconds,
                crate::settings::RecoveryReason::InvalidValue,
            )],
            artifact_warnings: vec![ArtifactWarning::StaleTemporary],
            last_save_failure: Some(SettingsPersistenceFailure::WritePrimary),
        }),
        manual: ManualFacts {
            state: ManualSessionState::Finite {
                remaining: std::time::Duration::from_secs(900),
            },
            background_failure: false,
        },
        detector: Some(DetectorFacts {
            state: AutomaticDownloadState::Hold {
                remaining_grace: std::time::Duration::from_millis(1_200),
            },
            latest_receive_rate_bytes_per_second: Some(2_048),
        }),
        automatic: Some(AutomaticFacts {
            preference_enabled: true,
            protect_on_battery: false,
            detector_intent: true,
            desired_automatic_reason: true,
            suppression: None,
            coordination_failure: false,
        }),
        power: Some(PowerFacts {
            source: PowerSource::External,
            health: PowerSourceObserverHealth::Degraded,
            failure: Some(PowerFailureFacts {
                operation: PowerSourceOperation::Sample,
                code: Some(17),
            }),
        }),
        wake: WakeFacts {
            active_reasons: vec![WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
            protection: ProtectionState::Active,
        },
        network: NetworkFacts {
            status: NetworkActivitySourceStatus::Available,
            failure: Some(NetworkActivityFailure::ProviderRead),
        },
        interfaces: vec![
            InterfaceFacts {
                id: 9,
                received_bytes: 90,
                recent_delta: None,
                included: false,
                continuity: NetworkInterfaceContinuity::CounterReset,
            },
            InterfaceFacts {
                id: 2,
                received_bytes: 20,
                recent_delta: Some(5),
                included: true,
                continuity: NetworkInterfaceContinuity::Continuous,
            },
        ],
        launch_at_login: Some(LaunchAtLoginFacts {
            available: true,
            desired: true,
            observed: Registration::Unknown,
            failure_stage: Some(LaunchAtLoginFailureStage::Verify),
        }),
        updater: Some(UpdaterFacts {
            availability: UpdateAvailability::Available,
            automatic_checks: false,
            status: UpdateStatus::UpdateAvailable {
                version: Some("0.2.0".to_owned()),
            },
        }),
    }
}

#[test]
fn representative_report_has_exact_stable_bytes() {
    let retention = LocalLog::retention_policy();
    let expected = format!(
        "Chiù diagnostics\n\
generated_at_utc=2026-08-10T12:00:00Z\n\
version={}\n\
os_family=macos\n\
os_version=15.6.1\n\
architecture=aarch64\n\
[application]\n\
status=degraded\n\
unavailable_capabilities=power-source\n\
startup_failure=none\n\
cleanup_failures=network-activity\n\
[settings]\n\
automatic_download_protection=true\n\
protect_on_battery=false\n\
launch_at_login=true\n\
automatic_update_checks=false\n\
threshold_bytes_per_second=1048576\n\
qualification_milliseconds=3000\n\
grace_milliseconds=120000\n\
load_condition=partial-recovery\n\
persistence=writable\n\
field_recovery=grace reason=invalid-value\n\
artifact_warning=stale-temporary\n\
last_save_failure=write-primary\n\
[manual_keep_awake]\n\
state=finite remaining_seconds=900\n\
background_failure=none\n\
[detector]\n\
state=hold remaining_milliseconds=1200\n\
aggregate_receive_bytes_per_second=2048\n\
[automatic_protection]\n\
preference_enabled=true\n\
protect_on_battery=false\n\
detector_intent=true\n\
desired_automatic_reason=true\n\
suppression=none\n\
coordination_failure=none\n\
[power_source]\n\
source=external\n\
health=degraded\n\
failure_operation=sample\n\
failure_code=17\n\
[wake_coordination]\n\
active_reasons=automatic-download,manual-keep-awake\n\
native_protection=active\n\
[network]\n\
status=available\n\
failure=provider-read\n\
interface_count=2\n\
interface id=2 included=true continuity=continuous received_bytes=20 recent_delta=5\n\
interface id=9 included=false continuity=counter-reset received_bytes=90 recent_delta=unavailable\n\
[launch_at_login]\n\
available=true\n\
desired=true\n\
observed=unknown\n\
failure_stage=verify\n\
[updater]\n\
availability=available\n\
automatic_checks=false\n\
status=update-available\n\
target_version=0.2.0\n\
[logging]\n\
health=available\n\
maximum_files={}\n\
maximum_file_bytes={}\n",
        env!("CARGO_PKG_VERSION"),
        retention.maximum_files,
        retention.maximum_file_bytes,
    );

    assert_eq!(
        report::render(
            &representative_input(),
            "2026-08-10T12:00:00Z",
            &PlatformInfo {
                family: "macos".to_owned(),
                version: "15.6.1".to_owned(),
                architecture: "aarch64".to_owned(),
            },
            LoggingHealth::Available,
        ),
        expected
    );
}

#[test]
fn minimal_report_has_exact_stable_bytes() {
    let retention = LocalLog::retention_policy();
    let expected = format!(
        "Chiù diagnostics\n\
generated_at_utc=2026-08-10T12:00:00Z\n\
version={}\n\
os_family=macos\n\
os_version=15.6.1\n\
architecture=aarch64\n\
[application]\n\
status=failed\n\
unavailable_capabilities=none\n\
startup_failure=none\n\
cleanup_failures=none\n\
[settings]\n\
status=not-initialized\n\
[manual_keep_awake]\n\
state=inactive\n\
background_failure=none\n\
[detector]\n\
status=not-initialized\n\
[automatic_protection]\n\
status=not-initialized\n\
[power_source]\n\
status=not-initialized\n\
[wake_coordination]\n\
active_reasons=none\n\
native_protection=inactive\n\
[network]\n\
status=not-started\n\
failure=none\n\
interface_count=0\n\
[launch_at_login]\n\
status=not-initialized\n\
[updater]\n\
status=not-initialized\n\
[logging]\n\
health=unavailable:open\n\
maximum_files={}\n\
maximum_file_bytes={}\n",
        env!("CARGO_PKG_VERSION"),
        retention.maximum_files,
        retention.maximum_file_bytes,
    );

    assert_eq!(
        report::render(
            &DiagnosticInput::minimal(ApplicationStatus::Failed),
            "2026-08-10T12:00:00Z",
            &PlatformInfo {
                family: "macos".to_owned(),
                version: "15.6.1".to_owned(),
                architecture: "aarch64".to_owned(),
            },
            LoggingHealth::Unavailable(LoggingFailureStage::Open),
        ),
        expected
    );
}

#[test]
fn partial_snapshot_is_deterministic_bounded_and_explicit_about_missing_capabilities() {
    let service = service(
        DiagnosticInput::minimal(ApplicationStatus::Failed),
        Some(Arc::new(RecordingClipboard::default())),
        Arc::new(RecordingOpener::default()),
        LocalLog::unavailable(LoggingFailureStage::Open),
    );

    let first = service.report().unwrap();
    let second = service.report().unwrap();

    assert_eq!(first, second);
    assert!(first.len() <= report::MAX_DIAGNOSTIC_BYTES);
    assert!(first.contains("generated_at_utc=2026-08-10T12:00:00Z"));
    assert!(first.contains("[settings]\nstatus=not-initialized"));
    assert!(first.contains("health=unavailable:open"));
    assert!(!first.contains("/Users/"));
}

#[test]
fn copy_writes_exactly_the_fresh_snapshot_and_never_needs_a_clipboard_read() {
    let clipboard = Arc::new(RecordingClipboard::default());
    let service = service(
        DiagnosticInput::minimal(ApplicationStatus::Ready),
        Some(clipboard.clone()),
        Arc::new(RecordingOpener::default()),
        LocalLog::unavailable(LoggingFailureStage::Open),
    );

    let copied = service.copy().unwrap();

    assert_eq!(clipboard.0.lock().unwrap().as_slice(), &[copied]);
    assert!(service.availability().copy);
    assert!(!service.availability().open_logs);
}

#[test]
fn tray_surfaces_operational_copy_failures_but_ignores_stale_shutdown_actions() {
    let snapshot_failure = DiagnosticsService::new(
        Arc::new(FixedSource(Err(()))),
        Some(Arc::new(RecordingClipboard::default())),
        Arc::new(RecordingOpener::default()),
        LocalLog::unavailable(LoggingFailureStage::Open),
        Arc::new(FixedClock),
        PlatformInfo {
            family: "macos".to_owned(),
            version: "15.6.1".to_owned(),
            architecture: "aarch64".to_owned(),
        },
    );
    let lifecycle =
        LifecyclePlan::new(|| Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())).build();
    let application = TrayApplication::new(lifecycle.state(), snapshot_failure);

    assert_eq!(
        application.execute(crate::tray::TrayCommand::CopyDiagnostics),
        TrayCommandOutcome::CopyDiagnosticsFailed
    );

    let clipboard_failure = DiagnosticsService::new(
        Arc::new(FixedSource(Ok(DiagnosticInput::minimal(
            ApplicationStatus::Ready,
        )))),
        Some(Arc::new(FailingClipboard)),
        Arc::new(RecordingOpener::default()),
        LocalLog::unavailable(LoggingFailureStage::Open),
        Arc::new(FixedClock),
        PlatformInfo {
            family: "macos".to_owned(),
            version: "15.6.1".to_owned(),
            architecture: "aarch64".to_owned(),
        },
    );
    let lifecycle =
        LifecyclePlan::new(|| Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())).build();
    let application = TrayApplication::new(lifecycle.state(), clipboard_failure.clone());

    assert_eq!(
        application.execute(crate::tray::TrayCommand::CopyDiagnostics),
        TrayCommandOutcome::CopyDiagnosticsFailed
    );
    clipboard_failure.begin_shutdown();
    assert_eq!(
        application.execute(crate::tray::TrayCommand::CopyDiagnostics),
        TrayCommandOutcome::Ignored
    );
}

#[test]
fn open_logs_delegates_the_resolved_directory_without_url_conversion() {
    let opener = Arc::new(RecordingOpener::default());
    let (log, _) = LocalLog::recording();
    let service = service(
        DiagnosticInput::minimal(ApplicationStatus::Failed),
        Some(Arc::new(RecordingClipboard::default())),
        opener.clone(),
        log,
    );

    service.open_logs().unwrap();

    assert_eq!(
        opener.0.lock().unwrap().as_slice(),
        &[PathBuf::from("/test/chiu/logs")]
    );
    assert!(service.availability().copy);
    assert!(service.availability().open_logs);
}

#[test]
fn shutdown_rejects_stale_diagnostic_actions_before_the_view_refreshes() {
    let clipboard = Arc::new(RecordingClipboard::default());
    let service = service(
        DiagnosticInput::minimal(ApplicationStatus::Ready),
        Some(clipboard.clone()),
        Arc::new(RecordingOpener::default()),
        LocalLog::unavailable(LoggingFailureStage::Open),
    );

    service.begin_shutdown();

    assert_eq!(service.copy(), Err(DiagnosticActionFailure::Unavailable));
    assert!(!service.availability().copy);
    assert!(clipboard.0.lock().unwrap().is_empty());
}

#[test]
fn render_redacts_raw_platform_and_updater_values() {
    let mut input = representative_input();
    input.updater.as_mut().unwrap().status = UpdateStatus::UpdateAvailable {
        version: Some("https://private.example/release".to_owned()),
    };

    let text = report::render(
        &input,
        "2026-08-10T12:00:00Z",
        &PlatformInfo {
            family: "/Users/alice/file.zip".to_owned(),
            version: "10.0.0.1".to_owned(),
            architecture: "line\nbreak".to_owned(),
        },
        LoggingHealth::Available,
    );

    assert!(text.contains("os_family=unavailable"));
    assert!(text.contains("os_version=unavailable"));
    assert!(text.contains("architecture=unavailable"));
    assert!(text.contains("target_version=unavailable"));
    assert!(!text.contains("private.example"));
    assert!(!text.contains("/Users/alice"));
}

#[test]
fn capture_assembles_typed_core_facts_without_formatting() {
    let lifecycle =
        LifecyclePlan::new(|| Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())).build();
    let log = LocalLog::unavailable(LoggingFailureStage::Open);
    let wake = WakeCoordinator::new(log.clone());
    let manual = ManualSession::new(wake.clone(), log.clone());
    let network = NetworkActivitySource::new(log);

    let input = DiagnosticInput::capture(
        &lifecycle.state(),
        &manual,
        &network,
        &wake,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    assert_eq!(input.application.status, ApplicationStatus::Starting);
    assert_eq!(input.manual.state, ManualSessionState::Inactive);
    assert_eq!(input.wake.protection, ProtectionState::Inactive);
    assert_eq!(
        input.network.status,
        NetworkActivitySourceStatus::NotStarted
    );
    assert!(input.settings.is_none());
    assert!(input.interfaces.is_empty());
}

#[test]
fn more_than_sixty_four_interfaces_are_reported_as_omitted_in_stable_id_order() {
    let mut input = DiagnosticInput::minimal(ApplicationStatus::Ready);
    input.interfaces = (1..=100)
        .rev()
        .map(|id| InterfaceFacts {
            id,
            received_bytes: id,
            recent_delta: Some(id),
            included: true,
            continuity: NetworkInterfaceContinuity::Continuous,
        })
        .collect();
    let text = report::render(
        &input,
        "2026-08-10T12:00:00Z",
        &PlatformInfo {
            family: "macos".to_owned(),
            version: "15.6.1".to_owned(),
            architecture: "aarch64".to_owned(),
        },
        LoggingHealth::Available,
    );

    assert!(text.find("interface id=1 ").unwrap() < text.find("interface id=2 ").unwrap());
    assert!(text.contains("interface_count=100"));
    assert!(text.contains("interfaces_omitted=36"));
    assert!(!text.contains("interface id=65 "));
    assert!(text.len() <= report::MAX_DIAGNOSTIC_BYTES);
}
