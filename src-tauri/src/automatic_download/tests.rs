use super::*;
use crate::automatic_protection::{
    AutomaticProtectionController, AutomaticProtectionSettings, PowerSource,
};
use crate::local_log::{LocalLog, LoggingFailureStage};
use crate::network_activity::{
    ContinuityLossReason, NetworkActivityAvailability, NetworkActivityConsumer,
    NetworkActivityEvent, ReceiveActivitySample,
};
use crate::wake_coordinator::{ProtectionState, SleepInhibitor, WakeCoordinator, WakeReason};
use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "chiu-automatic-download-{name}-{}",
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

struct RecordingInhibitor {
    fail_acquire: bool,
    fail_release_state: Option<ProtectionState>,
    state: ProtectionState,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingInhibitor {
    fn new() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                fail_acquire: false,
                fail_release_state: None,
                state: ProtectionState::Inactive,
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn failing_next_acquire() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let (mut inhibitor, calls) = Self::new();
        inhibitor.fail_acquire = true;
        (inhibitor, calls)
    }

    fn failing_next_release_active() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let (mut inhibitor, calls) = Self::new();
        inhibitor.fail_release_state = Some(ProtectionState::Active);
        (inhibitor, calls)
    }
}

impl SleepInhibitor for RecordingInhibitor {
    fn state(&self) -> ProtectionState {
        self.state
    }

    fn acquire(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.calls.lock().unwrap().push("acquire");
        if std::mem::take(&mut self.fail_acquire) {
            return Err(Box::new(ScriptedError("scripted acquire failure")));
        }
        self.state = ProtectionState::Active;
        Ok(())
    }

    fn release(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.calls.lock().unwrap().push("release");
        if let Some(state) = self.fail_release_state.take() {
            self.state = state;
            return Err(Box::new(ScriptedError("scripted release failure")));
        }
        self.state = ProtectionState::Inactive;
        Ok(())
    }
}

#[derive(Debug)]
struct ScriptedError(&'static str);

impl std::fmt::Display for ScriptedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for ScriptedError {}

fn sample(received_bytes: u64, observed_over: Duration) -> NetworkActivityEvent {
    NetworkActivityEvent::Sample(
        ReceiveActivitySample::try_new(received_bytes, observed_over)
            .expect("test samples must use a trusted interval"),
    )
}

fn unavailable_log() -> LocalLog {
    LocalLog::unavailable(LoggingFailureStage::Open)
}

fn wake_coordinator() -> WakeCoordinator {
    WakeCoordinator::new(unavailable_log())
}

fn wake_coordinator_with_inhibitor(inhibitor: impl SleepInhibitor + 'static) -> WakeCoordinator {
    WakeCoordinator::with_inhibitor(inhibitor, unavailable_log())
}

fn test_monitor(
    settings: AutomaticDownloadSettings,
    coordinator: WakeCoordinator,
) -> AutomaticDownloadMonitor {
    test_monitor_with_controller(settings, coordinator).0
}

fn test_monitor_with_controller(
    settings: AutomaticDownloadSettings,
    coordinator: WakeCoordinator,
) -> (AutomaticDownloadMonitor, AutomaticProtectionController) {
    test_monitor_with_controller_and_log(settings, coordinator, unavailable_log())
}

fn test_monitor_with_controller_and_log(
    settings: AutomaticDownloadSettings,
    coordinator: WakeCoordinator,
    log: LocalLog,
) -> (AutomaticDownloadMonitor, AutomaticProtectionController) {
    let controller = AutomaticProtectionController::new(
        AutomaticProtectionSettings::new(true, false),
        coordinator,
        log.clone(),
    );
    controller.set_power_source(PowerSource::External);
    (
        AutomaticDownloadMonitor::new(settings, controller.clone(), log),
        controller,
    )
}

#[test]
fn applying_new_tuning_resets_only_automatic_intent() {
    let coordinator = wake_coordinator();
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, true)
        .expect("manual protection should start");
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));
    let updated =
        AutomaticDownloadSettings::try_new(200, Duration::from_secs(1), Duration::from_secs(10))
            .expect("updated settings should be valid");

    let snapshot = monitor.apply_settings(updated);

    assert_eq!(snapshot.state, AutomaticDownloadState::Idle);
    assert_eq!(snapshot.settings, updated);
    assert_eq!(snapshot.latest_receive_rate_bytes_per_second, None);
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::ManualKeepAwake]
    );
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );
}

#[test]
fn detector_logs_transitions_and_continuity_resets_but_not_steady_samples() {
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .unwrap();
    let (log, lines) = LocalLog::recording();
    let (mut monitor, _) = test_monitor_with_controller_and_log(settings, wake_coordinator(), log);

    monitor.consume(sample(100, Duration::from_secs(1)));
    for _ in 0..10 {
        monitor.consume(sample(150, Duration::from_secs(1)));
    }
    monitor.consume(NetworkActivityEvent::ContinuityLost(
        ContinuityLossReason::ExplicitReset,
    ));

    let persisted = lines.lock().unwrap().join("");
    assert_eq!(persisted.matches("detector.transition").count(), 2);
    assert_eq!(persisted.matches("detector.continuity-reset").count(), 1);
    assert!(!persisted.contains("receive-rate"));
}

#[test]
fn steady_download_samples_do_not_repeat_successful_automatic_reconciliation_logs() {
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .unwrap();
    let (log, lines) = LocalLog::recording();
    let coordinator = WakeCoordinator::new(log.clone());
    let (mut monitor, _) = test_monitor_with_controller_and_log(settings, coordinator, log);

    monitor.consume(sample(100, Duration::from_secs(1)));
    for _ in 0..10 {
        monitor.consume(sample(150, Duration::from_secs(1)));
    }
    monitor.consume(NetworkActivityEvent::ContinuityLost(
        ContinuityLossReason::ExplicitReset,
    ));
    monitor.consume(sample(100, Duration::from_secs(1)));

    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted
            .matches("automatic.reconciliation active=true outcome=succeeded")
            .count(),
        2
    );
    assert_eq!(
        persisted
            .matches("automatic.reconciliation active=false outcome=succeeded")
            .count(),
        1
    );
    assert_eq!(persisted.matches("wake.reasons-changed").count(), 3);
    assert_eq!(persisted.matches("wake.native-protection").count(), 3);
}

#[test]
fn automatic_reconciliation_logs_one_failure_and_recovery_then_stays_quiet() {
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .unwrap();
    let (inhibitor, _) = RecordingInhibitor::failing_next_acquire();
    let (log, lines) = LocalLog::recording();
    let coordinator = WakeCoordinator::with_inhibitor(inhibitor, log.clone());
    let (mut monitor, _) = test_monitor_with_controller_and_log(settings, coordinator, log);

    monitor.consume(sample(100, Duration::from_secs(1)));
    monitor.consume(sample(150, Duration::from_secs(1)));
    for _ in 0..10 {
        monitor.consume(sample(150, Duration::from_secs(1)));
    }

    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted
            .matches("automatic.reconciliation active=true outcome=failed")
            .count(),
        1
    );
    assert_eq!(
        persisted
            .matches("automatic.reconciliation active=true outcome=succeeded")
            .count(),
        1
    );
    assert_eq!(persisted.matches("wake.reasons-changed").count(), 1);
    assert_eq!(persisted.matches("wake.native-protection").count(), 2);
}

#[test]
fn failed_tuning_persistence_leaves_live_tuning_and_history_unchanged() {
    let coordinator = wake_coordinator();
    let settings = AutomaticDownloadSettings::default();
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(
        settings.meaningful_receive_rate_bytes_per_second(),
        Duration::from_secs(1),
    ));
    let before = monitor.snapshot();
    let tuning = AutomaticDownloadTuningService::new(
        SettingsService::unavailable(unavailable_log()),
        monitor.clone(),
    );

    assert!(tuning.set_threshold(262_144).is_err());

    assert_eq!(monitor.snapshot(), before);
}

#[test]
fn persisted_grace_change_preserves_threshold_and_qualification_then_resets() {
    let directory = TestDirectory::new("grace");
    let persisted = SettingsService::load(directory.0.clone(), unavailable_log());
    let initial =
        AutomaticDownloadSettings::try_new(256, Duration::from_secs(3), Duration::from_secs(30))
            .unwrap();
    persisted
        .update(SettingsUpdate::AutomaticDownloadTuning {
            meaningful_receive_rate_bytes_per_second: 256,
            activation_qualification_milliseconds: 3_000,
            grace_milliseconds: 30_000,
        })
        .unwrap();
    let coordinator = wake_coordinator();
    let mut monitor = test_monitor(initial, coordinator.clone());
    monitor.consume(sample(768, Duration::from_secs(3)));
    let tuning = AutomaticDownloadTuningService::new(persisted.clone(), monitor.clone());

    let snapshot = tuning.set_grace(Duration::from_secs(300)).unwrap();

    assert_eq!(snapshot.state, AutomaticDownloadState::Idle);
    assert_eq!(
        snapshot.settings.meaningful_receive_rate_bytes_per_second(),
        256
    );
    assert_eq!(
        snapshot.settings.activation_qualification(),
        Duration::from_secs(3)
    );
    assert_eq!(snapshot.settings.grace(), Duration::from_secs(300));
    assert_eq!(persisted.snapshot().automatic_download(), snapshot.settings);
    assert!(coordinator.snapshot().active_reasons().is_empty());
}

#[test]
fn post_persistence_release_failure_keeps_tuning_and_truthful_coordination_failure() {
    let directory = TestDirectory::new("release-failure");
    let persisted = SettingsService::load(directory.0.clone(), unavailable_log());
    let (inhibitor, _) = RecordingInhibitor::failing_next_release_active();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(30))
            .unwrap();
    let (mut monitor, controller) = test_monitor_with_controller(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));
    let tuning = AutomaticDownloadTuningService::new(persisted.clone(), monitor.clone());

    let snapshot = tuning.set_threshold(200).unwrap();

    assert_eq!(
        snapshot.settings.meaningful_receive_rate_bytes_per_second(),
        200
    );
    assert_eq!(persisted.snapshot().automatic_download(), snapshot.settings);
    assert!(
        controller
            .snapshot()
            .latest_coordination_failure()
            .is_some()
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );
}

#[test]
fn a_new_monitor_is_idle_without_automatic_wake_intent() {
    let coordinator = wake_coordinator();
    let monitor = test_monitor(AutomaticDownloadSettings::default(), coordinator.clone());

    assert_eq!(
        monitor.snapshot(),
        AutomaticDownloadSnapshot {
            state: AutomaticDownloadState::Idle,
            latest_receive_rate_bytes_per_second: None,
            settings: AutomaticDownloadSettings::default(),
        }
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
}

#[test]
fn settings_reject_a_zero_meaningful_receive_rate_threshold() {
    let error =
        AutomaticDownloadSettings::try_new(0, Duration::from_secs(5), Duration::from_secs(120))
            .expect_err("zero threshold must be rejected");

    assert_eq!(
        error,
        AutomaticDownloadSettingsError::ZeroMeaningfulReceiveRate
    );
}

#[test]
fn settings_reject_a_zero_activation_qualification() {
    let error = AutomaticDownloadSettings::try_new(1024, Duration::ZERO, Duration::from_secs(120))
        .expect_err("zero activation qualification must be rejected");

    assert_eq!(
        error,
        AutomaticDownloadSettingsError::ZeroActivationQualification
    );
}

#[test]
fn threshold_equality_projects_the_actual_rate_without_activating_a_short_burst() {
    let coordinator = wake_coordinator();
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(3), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());

    monitor.consume(sample(200, Duration::from_secs(2)));

    assert_eq!(
        monitor.snapshot(),
        AutomaticDownloadSnapshot {
            state: AutomaticDownloadState::Idle,
            latest_receive_rate_bytes_per_second: Some(100),
            settings,
        }
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
}

#[test]
fn unequal_meaningful_intervals_activate_at_the_exact_qualification_duration() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(3), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());

    monitor.consume(sample(150, Duration::from_secs(1)));
    monitor.consume(sample(200, Duration::from_secs(2)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
    assert_eq!(
        monitor.snapshot().latest_receive_rate_bytes_per_second,
        Some(100)
    );
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::AutomaticDownload]
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn first_quiet_interval_enters_hold_without_toggling_automatic_intent() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(200, Duration::from_secs(2)));

    monitor.consume(sample(99, Duration::from_secs(1)));

    assert_eq!(
        monitor.snapshot().state,
        AutomaticDownloadState::Hold {
            remaining_grace: Duration::from_secs(9),
        }
    );
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::AutomaticDownload]
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn meaningful_activity_recovers_hold_directly_to_active() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(3), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(300, Duration::from_secs(3)));
    monitor.consume(sample(50, Duration::from_secs(1)));

    monitor.consume(sample(100, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn repeated_quiet_intervals_consume_actual_grace_and_release_at_expiry() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(200, Duration::from_secs(2)));
    monitor.consume(sample(200, Duration::from_secs(4)));

    monitor.consume(sample(400, Duration::from_secs(5)));
    monitor.consume(sample(0, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn a_quiet_idle_sample_clears_partial_activation_history() {
    let (inhibitor, _) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(3), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(200, Duration::from_secs(2)));
    monitor.consume(sample(99, Duration::from_secs(1)));

    monitor.consume(sample(200, Duration::from_secs(2)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
}

#[test]
fn one_quiet_interval_can_exhaust_grace_directly_from_active() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(3))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(200, Duration::from_secs(2)));

    monitor.consume(sample(399, Duration::from_secs(4)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn zero_grace_disables_hold() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings = AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::ZERO)
        .expect("zero grace should be supported");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor.consume(sample(0, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn continuity_loss_clears_activity_rate_intent_and_requires_fresh_qualification() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(200, Duration::from_secs(2)));

    monitor.consume(NetworkActivityEvent::ContinuityLost(
        ContinuityLossReason::ExplicitReset,
    ));

    assert_eq!(
        monitor.snapshot(),
        AutomaticDownloadSnapshot {
            state: AutomaticDownloadState::Idle,
            latest_receive_rate_bytes_per_second: None,
            settings,
        }
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    monitor.consume(sample(100, Duration::from_secs(1)));
    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn availability_events_neither_count_as_activity_nor_clear_partial_history() {
    let (inhibitor, _) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Unavailable,
    ));
    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Available,
    ));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    monitor.consume(sample(100, Duration::from_secs(1)));
    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
}

#[test]
fn idle_events_do_not_retry_or_report_manual_only_acquisition_failure() {
    let (inhibitor, calls) = RecordingInhibitor::failing_next_acquire();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, true)
        .expect_err("manual acquisition should fail");
    let (mut monitor, controller) =
        test_monitor_with_controller(AutomaticDownloadSettings::default(), coordinator.clone());

    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Available,
    ));
    monitor.consume(sample(0, Duration::from_secs(1)));

    assert_eq!(controller.snapshot().latest_coordination_failure(), None);
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::ManualKeepAwake]
    );
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Inactive
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn idle_events_and_shutdown_do_not_retry_or_report_manual_only_release_failure() {
    let (inhibitor, calls) = RecordingInhibitor::failing_next_release_active();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, true)
        .expect("manual acquisition should succeed");
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, false)
        .expect_err("manual release should fail");
    let (mut monitor, controller) =
        test_monitor_with_controller(AutomaticDownloadSettings::default(), coordinator.clone());

    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Unavailable,
    ));
    monitor
        .shutdown()
        .expect("idle automatic shutdown should have no work");

    assert_eq!(controller.snapshot().latest_coordination_failure(), None);
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn extreme_byte_rate_uses_u128_without_overflow_or_early_activation() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(u64::MAX, Duration::from_nanos(2), Duration::ZERO)
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);

    monitor.consume(sample(u64::MAX, Duration::from_nanos(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(
        monitor.snapshot().latest_receive_rate_bytes_per_second,
        Some(u128::from(u64::MAX) * 1_000_000_000)
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn failed_automatic_acquisition_preserves_intent_and_retries_on_the_next_event() {
    let (inhibitor, calls) = RecordingInhibitor::failing_next_acquire();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let (mut monitor, controller) = test_monitor_with_controller(settings, coordinator.clone());

    monitor.consume(sample(100, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
    assert_eq!(
        controller.snapshot().latest_coordination_failure(),
        Some("scripted acquire failure")
    );
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::AutomaticDownload]
    );
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Inactive
    );

    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Available,
    ));

    assert_eq!(controller.snapshot().latest_coordination_failure(), None);
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire", "acquire"]);
}

#[test]
fn failed_automatic_release_keeps_idle_intent_and_retries_on_the_next_event() {
    let (inhibitor, calls) = RecordingInhibitor::failing_next_release_active();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings = AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::ZERO)
        .expect("settings should be valid");
    let (mut monitor, controller) = test_monitor_with_controller(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor.consume(sample(0, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(
        controller.snapshot().latest_coordination_failure(),
        Some("scripted release failure")
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );

    monitor.consume(NetworkActivityEvent::AvailabilityChanged(
        NetworkActivityAvailability::Unavailable,
    ));

    assert_eq!(controller.snapshot().latest_coordination_failure(), None);
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Inactive
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release", "release"]);
}

#[test]
fn automatic_expiry_does_not_release_native_protection_while_manual_intent_remains() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, true)
        .expect("manual activation should succeed");
    let settings = AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::ZERO)
        .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor.consume(sample(0, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(
        coordinator.snapshot().active_reasons(),
        &[WakeReason::ManualKeepAwake]
    );
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn shutdown_is_idempotent_and_clears_detector_history_before_releasing_intent() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor.shutdown().expect("first shutdown should succeed");
    monitor
        .shutdown()
        .expect("duplicate shutdown should remain safe");

    assert_eq!(
        monitor.snapshot(),
        AutomaticDownloadSnapshot {
            state: AutomaticDownloadState::Idle,
            latest_receive_rate_bytes_per_second: None,
            settings,
        }
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn provisional_defaults_are_centralized_in_the_typed_settings_contract() {
    let settings = AutomaticDownloadSettings::default();

    assert_eq!(
        settings.meaningful_receive_rate_bytes_per_second.get(),
        1024 * 1024
    );
    assert_eq!(settings.activation_qualification, Duration::from_secs(5));
    assert_eq!(settings.grace, Duration::from_secs(120));
}

#[test]
fn continued_meaningful_activity_does_not_duplicate_native_acquisition() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);

    monitor.consume(sample(100, Duration::from_secs(1)));
    monitor.consume(sample(500, Duration::from_secs(2)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Active);
    assert_eq!(*calls.lock().unwrap(), ["acquire"]);
}

#[test]
fn continuity_loss_while_idle_clears_partial_qualification() {
    let (inhibitor, _) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(2), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator);
    monitor.consume(sample(100, Duration::from_secs(1)));
    monitor.consume(NetworkActivityEvent::ContinuityLost(
        ContinuityLossReason::UntrustedInterval,
    ));

    monitor.consume(sample(100, Duration::from_secs(1)));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
}

#[test]
fn continuity_loss_while_hold_clears_grace_and_automatic_intent() {
    let (inhibitor, calls) = RecordingInhibitor::new();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let mut monitor = test_monitor(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));
    monitor.consume(sample(0, Duration::from_secs(1)));

    monitor.consume(NetworkActivityEvent::ContinuityLost(
        ContinuityLossReason::ProviderUnavailable,
    ));

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
}

#[test]
fn failed_shutdown_stays_idle_and_a_later_shutdown_retries_release() {
    let (inhibitor, calls) = RecordingInhibitor::failing_next_release_active();
    let coordinator = wake_coordinator_with_inhibitor(inhibitor);
    let settings =
        AutomaticDownloadSettings::try_new(100, Duration::from_secs(1), Duration::from_secs(10))
            .expect("settings should be valid");
    let (mut monitor, controller) = test_monitor_with_controller(settings, coordinator.clone());
    monitor.consume(sample(100, Duration::from_secs(1)));

    monitor
        .shutdown()
        .expect_err("first shutdown should surface release failure");

    assert_eq!(monitor.snapshot().state, AutomaticDownloadState::Idle);
    assert_eq!(
        controller.snapshot().latest_coordination_failure(),
        Some("scripted release failure")
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(
        coordinator.snapshot().protection_state(),
        ProtectionState::Active
    );

    monitor
        .shutdown()
        .expect("later shutdown should retry release");

    assert_eq!(controller.snapshot().latest_coordination_failure(), None);
    assert_eq!(*calls.lock().unwrap(), ["acquire", "release", "release"]);
}
