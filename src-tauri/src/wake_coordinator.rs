use crate::{
    local_log::{EventOutcome, LocalLog, ProtectionAction, SafeEvent},
    sleep_inhibitor::{IdleSleepInhibitor, InhibitorState},
};
use std::{
    collections::BTreeSet,
    error::Error,
    sync::{Arc, Mutex},
};

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum WakeReason {
    AutomaticDownload,
    ManualKeepAwake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtectionState {
    Inactive,
    Active,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WakeSnapshot {
    active_reasons: Vec<WakeReason>,
    protection_state: ProtectionState,
}

impl WakeSnapshot {
    pub(crate) fn active_reasons(&self) -> &[WakeReason] {
        &self.active_reasons
    }

    pub(crate) fn protection_state(&self) -> ProtectionState {
        self.protection_state
    }
}

#[derive(Debug)]
pub(crate) struct WakeCoordinationError {
    source: BoxError,
}

impl std::fmt::Display for WakeCoordinationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for WakeCoordinationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(crate) trait SleepInhibitor: Send {
    fn state(&self) -> ProtectionState;
    fn acquire(&mut self) -> Result<(), BoxError>;
    fn release(&mut self) -> Result<(), BoxError>;
}

impl SleepInhibitor for IdleSleepInhibitor {
    fn state(&self) -> ProtectionState {
        match self.state() {
            InhibitorState::Inactive => ProtectionState::Inactive,
            InhibitorState::Active => ProtectionState::Active,
        }
    }

    fn acquire(&mut self) -> Result<(), BoxError> {
        IdleSleepInhibitor::acquire(self).map_err(|error| Box::new(error) as BoxError)
    }

    fn release(&mut self) -> Result<(), BoxError> {
        IdleSleepInhibitor::release(self).map_err(|error| Box::new(error) as BoxError)
    }
}

struct CoordinatorCore {
    active_reasons: BTreeSet<WakeReason>,
    inhibitor: Box<dyn SleepInhibitor>,
}

#[derive(Clone)]
pub(crate) struct WakeCoordinator {
    core: Arc<Mutex<CoordinatorCore>>,
    log: LocalLog,
}

impl WakeCoordinator {
    pub(crate) fn new(log: LocalLog) -> Self {
        Self::from_inhibitor(IdleSleepInhibitor::new(), log)
    }

    fn from_inhibitor(inhibitor: impl SleepInhibitor + 'static, log: LocalLog) -> Self {
        Self {
            core: Arc::new(Mutex::new(CoordinatorCore {
                active_reasons: BTreeSet::new(),
                inhibitor: Box::new(inhibitor),
            })),
            log,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_inhibitor(inhibitor: impl SleepInhibitor + 'static, log: LocalLog) -> Self {
        Self::from_inhibitor(inhibitor, log)
    }

    pub(crate) fn snapshot(&self) -> WakeSnapshot {
        let core = self.core.lock().expect("wake coordinator lock poisoned");
        core.snapshot()
    }

    pub(crate) fn set_reason_active(
        &self,
        reason: WakeReason,
        active: bool,
    ) -> Result<WakeSnapshot, WakeCoordinationError> {
        let mut core = self.core.lock().expect("wake coordinator lock poisoned");
        let prior_reasons = core.active_reasons.clone();
        let prior_protection = core.inhibitor.state();
        if active {
            core.active_reasons.insert(reason);
        } else {
            core.active_reasons.remove(&reason);
        }
        let reconciliation = core.reconcile();
        let snapshot = core.snapshot();
        drop(core);
        if prior_reasons != snapshot.active_reasons.iter().copied().collect() {
            self.record(SafeEvent::WakeReasonsChanged {
                automatic: snapshot
                    .active_reasons
                    .contains(&WakeReason::AutomaticDownload),
                manual: snapshot
                    .active_reasons
                    .contains(&WakeReason::ManualKeepAwake),
            });
        }
        if prior_protection != snapshot.protection_state || reconciliation.is_err() {
            self.record(SafeEvent::NativeProtection {
                action: if snapshot.active_reasons.is_empty() {
                    ProtectionAction::Release
                } else {
                    ProtectionAction::Acquire
                },
                outcome: if reconciliation.is_ok() {
                    EventOutcome::Succeeded
                } else {
                    EventOutcome::Failed
                },
            });
        }
        reconciliation.map_err(|source| WakeCoordinationError { source })?;
        Ok(snapshot)
    }

    pub(crate) fn shutdown(&self) -> Result<(), WakeCoordinationError> {
        let mut core = self.core.lock().expect("wake coordinator lock poisoned");
        let had_reasons = !core.active_reasons.is_empty();
        let prior_protection = core.inhibitor.state();
        core.active_reasons.clear();
        let reconciliation = core.reconcile();
        drop(core);
        if had_reasons {
            self.record(SafeEvent::WakeReasonsChanged {
                automatic: false,
                manual: false,
            });
        }
        if prior_protection == ProtectionState::Active || reconciliation.is_err() {
            self.record(SafeEvent::NativeProtection {
                action: ProtectionAction::Release,
                outcome: if reconciliation.is_ok() {
                    EventOutcome::Succeeded
                } else {
                    EventOutcome::Failed
                },
            });
        }
        reconciliation.map_err(|source| WakeCoordinationError { source })
    }

    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }
}

impl CoordinatorCore {
    fn snapshot(&self) -> WakeSnapshot {
        WakeSnapshot {
            active_reasons: self.active_reasons.iter().copied().collect(),
            protection_state: self.inhibitor.state(),
        }
    }

    fn reconcile(&mut self) -> Result<(), BoxError> {
        match (self.active_reasons.is_empty(), self.inhibitor.state()) {
            (false, ProtectionState::Inactive) => self.inhibitor.acquire(),
            (true, ProtectionState::Active) => self.inhibitor.release(),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn unavailable_log() -> LocalLog {
        LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
    }

    struct RecordingInhibitor {
        acquire_failure: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
        release_failure_state: Option<ProtectionState>,
        state: ProtectionState,
    }

    impl RecordingInhibitor {
        fn new() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            Self::configured(false, None)
        }

        fn failing_next_acquire() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            Self::configured(true, None)
        }

        fn failing_next_release_active() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            Self::configured(false, Some(ProtectionState::Active))
        }

        fn failing_next_release_inactive() -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            Self::configured(false, Some(ProtectionState::Inactive))
        }

        fn configured(
            acquire_failure: bool,
            release_failure_state: Option<ProtectionState>,
        ) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    acquire_failure,
                    calls: calls.clone(),
                    release_failure_state,
                    state: ProtectionState::Inactive,
                },
                calls,
            )
        }
    }

    impl SleepInhibitor for RecordingInhibitor {
        fn state(&self) -> ProtectionState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), BoxError> {
            self.calls.lock().unwrap().push("acquire");
            if std::mem::take(&mut self.acquire_failure) {
                return Err(Box::new(ScriptedError("scripted acquire failure")));
            }
            self.state = ProtectionState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), BoxError> {
            self.calls.lock().unwrap().push("release");
            if let Some(state) = self.release_failure_state.take() {
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

    fn assert_snapshot(
        snapshot: &WakeSnapshot,
        active_reasons: &[WakeReason],
        protection_state: ProtectionState,
    ) {
        assert_eq!(snapshot.active_reasons(), active_reasons);
        assert_eq!(snapshot.protection_state(), protection_state);
    }

    #[test]
    fn a_new_coordinator_has_no_reasons_and_does_not_acquire_protection() {
        let (inhibitor, calls) = RecordingInhibitor::new();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

        assert_snapshot(&coordinator.snapshot(), &[], ProtectionState::Inactive);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn automatic_download_reason_acquires_and_releases_protection() {
        let (inhibitor, calls) = RecordingInhibitor::new();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

        let active = coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("activation should succeed");
        assert_snapshot(
            &active,
            &[WakeReason::AutomaticDownload],
            ProtectionState::Active,
        );

        let inactive = coordinator
            .set_reason_active(WakeReason::AutomaticDownload, false)
            .expect("deactivation should succeed");
        assert_snapshot(&inactive, &[], ProtectionState::Inactive);
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }

    #[test]
    fn manual_keep_awake_reason_acquires_and_releases_protection() {
        let (inhibitor, calls) = RecordingInhibitor::new();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

        let active = coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .expect("activation should succeed");
        assert_snapshot(
            &active,
            &[WakeReason::ManualKeepAwake],
            ProtectionState::Active,
        );

        let inactive = coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
            .expect("deactivation should succeed");
        assert_snapshot(&inactive, &[], ProtectionState::Inactive);
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }

    #[test]
    fn duplicate_reason_commands_do_not_duplicate_native_work() {
        let (inhibitor, calls) = RecordingInhibitor::new();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

        coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
            .expect("deactivating an absent reason should succeed");
        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("activation should succeed");
        let duplicate = coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("duplicate activation should succeed");
        coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
            .expect("deactivating another absent reason should succeed");
        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, false)
            .expect("deactivation should succeed");
        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, false)
            .expect("duplicate deactivation should succeed");

        assert_snapshot(
            &duplicate,
            &[WakeReason::AutomaticDownload],
            ProtectionState::Active,
        );
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }

    #[test]
    fn every_two_reason_transition_order_holds_until_the_final_reason_is_removed() {
        let orders = [
            (
                [WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
                [WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
            ),
            (
                [WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
                [WakeReason::ManualKeepAwake, WakeReason::AutomaticDownload],
            ),
            (
                [WakeReason::ManualKeepAwake, WakeReason::AutomaticDownload],
                [WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
            ),
            (
                [WakeReason::ManualKeepAwake, WakeReason::AutomaticDownload],
                [WakeReason::ManualKeepAwake, WakeReason::AutomaticDownload],
            ),
        ];

        for (activation_order, removal_order) in orders {
            let (inhibitor, calls) = RecordingInhibitor::new();
            let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

            coordinator
                .set_reason_active(activation_order[0], true)
                .expect("first activation should succeed");
            let overlapping = coordinator
                .set_reason_active(activation_order[1], true)
                .expect("second activation should succeed");
            assert_snapshot(
                &overlapping,
                &[WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
                ProtectionState::Active,
            );

            let one_remaining = coordinator
                .set_reason_active(removal_order[0], false)
                .expect("first removal should succeed");
            assert_snapshot(&one_remaining, &[removal_order[1]], ProtectionState::Active);

            let inactive = coordinator
                .set_reason_active(removal_order[1], false)
                .expect("final removal should succeed");
            assert_snapshot(&inactive, &[], ProtectionState::Inactive);
            assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
        }
    }

    #[test]
    fn failed_acquisition_preserves_intent_and_duplicate_activation_retries() {
        let (inhibitor, calls) = RecordingInhibitor::failing_next_acquire();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());

        let error = coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect_err("first acquisition should fail");
        assert_eq!(error.to_string(), "scripted acquire failure");
        assert!(
            error
                .source()
                .expect("source should be preserved")
                .downcast_ref::<ScriptedError>()
                .is_some(),
            "source should preserve its concrete type"
        );
        assert_snapshot(
            &coordinator.snapshot(),
            &[WakeReason::AutomaticDownload],
            ProtectionState::Inactive,
        );

        let recovered = coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("duplicate activation should retry");
        assert_snapshot(
            &recovered,
            &[WakeReason::AutomaticDownload],
            ProtectionState::Active,
        );
        assert_eq!(*calls.lock().unwrap(), ["acquire", "acquire"]);
    }

    #[test]
    fn failed_release_keeps_intent_absent_and_duplicate_deactivation_retries() {
        let (inhibitor, calls) = RecordingInhibitor::failing_next_release_active();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());
        coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .expect("activation should succeed");

        let error = coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
            .expect_err("first release should fail");
        assert_eq!(error.to_string(), "scripted release failure");
        assert_snapshot(&coordinator.snapshot(), &[], ProtectionState::Active);

        let recovered = coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
            .expect("duplicate deactivation should retry");
        assert_snapshot(&recovered, &[], ProtectionState::Inactive);
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release", "release"]);
    }

    #[test]
    fn release_error_that_reports_inactive_never_projects_active_protection() {
        let (inhibitor, calls) = RecordingInhibitor::failing_next_release_inactive();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());
        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("activation should succeed");

        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, false)
            .expect_err("release should surface cleanup debt");
        assert_snapshot(&coordinator.snapshot(), &[], ProtectionState::Inactive);

        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, false)
            .expect("duplicate deactivation should see reconciled state");
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }

    #[test]
    fn shutdown_releases_active_protection_and_is_a_no_op_when_inactive() {
        let (active_inhibitor, active_calls) = RecordingInhibitor::new();
        let active = WakeCoordinator::with_inhibitor(active_inhibitor, unavailable_log());
        active
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .expect("activation should succeed");

        active.shutdown().expect("shutdown should release");
        assert_snapshot(&active.snapshot(), &[], ProtectionState::Inactive);
        assert_eq!(*active_calls.lock().unwrap(), ["acquire", "release"]);

        let (inactive_inhibitor, inactive_calls) = RecordingInhibitor::new();
        let inactive = WakeCoordinator::with_inhibitor(inactive_inhibitor, unavailable_log());
        inactive
            .shutdown()
            .expect("inactive shutdown should succeed");
        assert!(inactive_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn logging_write_failure_cannot_change_wake_reason_or_native_protection_truth() {
        let (inhibitor, _) = RecordingInhibitor::new();
        let log = LocalLog::failing_for_tests();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, log.clone());

        let snapshot = coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .expect("logging failure must not fail wake coordination");

        assert_eq!(snapshot.active_reasons(), &[WakeReason::ManualKeepAwake]);
        assert_eq!(snapshot.protection_state(), ProtectionState::Active);
        assert_eq!(
            log.health(),
            crate::local_log::LoggingHealth::Unavailable(
                crate::local_log::LoggingFailureStage::Write
            )
        );
    }

    #[test]
    fn lifecycle_cleanup_owns_the_same_coordinator_future_callers_receive() {
        let (inhibitor, calls) = RecordingInhibitor::new();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());
        let mut lifecycle = crate::test_lifecycle(
            coordinator.clone(),
            crate::manual_session::ManualSession::new(coordinator.clone(), unavailable_log()),
        );
        lifecycle.start().expect("startup should succeed");
        coordinator
            .set_reason_active(WakeReason::AutomaticDownload, true)
            .expect("activation through the shared handle should succeed");

        let report = lifecycle.shutdown();

        assert!(report.failures().is_empty());
        assert_snapshot(&coordinator.snapshot(), &[], ProtectionState::Inactive);
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }

    #[test]
    fn lifecycle_cleanup_reports_wake_coordination_release_failure() {
        let (inhibitor, calls) = RecordingInhibitor::failing_next_release_active();
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());
        let mut lifecycle = crate::test_lifecycle(
            coordinator.clone(),
            crate::manual_session::ManualSession::new(coordinator.clone(), unavailable_log()),
        );
        lifecycle.start().expect("startup should succeed");
        coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .expect("activation should succeed");

        let report = lifecycle.shutdown();

        assert_eq!(report.failures().len(), 1);
        assert_eq!(
            report.failures()[0].capability(),
            crate::application::CapabilityId::new("wake-coordination")
        );
        assert_eq!(report.failures()[0].message(), "scripted release failure");
        assert_snapshot(&coordinator.snapshot(), &[], ProtectionState::Active);
        assert_eq!(*calls.lock().unwrap(), ["acquire", "release"]);
    }
}
