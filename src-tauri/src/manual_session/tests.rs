#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake_coordinator::{ProtectionState, SleepInhibitor, WakeCoordinator, WakeReason};
    use std::{
        error::Error,
        sync::{Barrier, Mutex},
        time::Duration,
    };

    type BoxError = Box<dyn Error + Send + Sync>;

    fn unavailable_log() -> LocalLog {
        LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
    }

    struct TestInhibitor {
        state: ProtectionState,
    }

    impl SleepInhibitor for TestInhibitor {
        fn state(&self) -> ProtectionState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Inactive;
            Ok(())
        }
    }

    struct FailingFirstAcquireInhibitor {
        fail_next: bool,
        state: ProtectionState,
    }

    impl SleepInhibitor for FailingFirstAcquireInhibitor {
        fn state(&self) -> ProtectionState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), BoxError> {
            if std::mem::take(&mut self.fail_next) {
                return Err(Box::new(ScriptedError("scripted acquire failure")));
            }
            self.state = ProtectionState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Inactive;
            Ok(())
        }
    }

    struct FailingFirstReleaseInhibitor {
        fail_next: bool,
        state: ProtectionState,
    }

    impl SleepInhibitor for FailingFirstReleaseInhibitor {
        fn state(&self) -> ProtectionState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), BoxError> {
            if std::mem::take(&mut self.fail_next) {
                return Err(Box::new(ScriptedError("scripted release failure")));
            }
            self.state = ProtectionState::Inactive;
            Ok(())
        }
    }

    struct NotifyingReleaseInhibitor {
        released: std::sync::mpsc::Sender<()>,
        state: ProtectionState,
    }

    impl SleepInhibitor for NotifyingReleaseInhibitor {
        fn state(&self) -> ProtectionState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), BoxError> {
            self.state = ProtectionState::Inactive;
            self.released
                .send(())
                .expect("release observer should remain connected");
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

    struct TestClock {
        now: Mutex<Duration>,
    }

    impl Clock for TestClock {
        fn now(&self) -> Duration {
            *self.now.lock().unwrap()
        }
    }

    impl TestClock {
        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now = now.checked_add(duration).expect("test clock overflow");
        }
    }

    fn test_session(now: Duration) -> (ManualSession, WakeCoordinator, Arc<TestClock>) {
        test_session_with_log(now, unavailable_log())
    }

    fn test_session_with_log(
        now: Duration,
        log: LocalLog,
    ) -> (ManualSession, WakeCoordinator, Arc<TestClock>) {
        let coordinator = WakeCoordinator::with_inhibitor(
            TestInhibitor {
                state: ProtectionState::Inactive,
            },
            unavailable_log(),
        );
        let clock = Arc::new(TestClock {
            now: Mutex::new(now),
        });
        let session = ManualSession::with_clock(coordinator.clone(), clock.clone(), log);
        (session, coordinator, clock)
    }

    fn session_with_inhibitor(
        now: Duration,
        inhibitor: impl SleepInhibitor + 'static,
    ) -> (ManualSession, WakeCoordinator, Arc<TestClock>) {
        let coordinator = WakeCoordinator::with_inhibitor(inhibitor, unavailable_log());
        let clock = Arc::new(TestClock {
            now: Mutex::new(now),
        });
        let session =
            ManualSession::with_clock(coordinator.clone(), clock.clone(), unavailable_log());
        (session, coordinator, clock)
    }

    #[test]
    fn a_new_manual_session_is_inactive_without_requesting_wake_protection() {
        let (session, coordinator, _) = test_session(Duration::ZERO);

        assert_eq!(
            session.snapshot(),
            ManualSessionSnapshot {
                state: ManualSessionState::Inactive,
                background_failure: None,
            }
        );
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn starting_a_fifteen_minute_session_sets_a_deadline_and_manual_wake_intent() {
        let (session, coordinator, _) = test_session(Duration::from_secs(5 * 60));

        let snapshot = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");

        assert_eq!(
            snapshot.state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(15 * 60),
            }
        );
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
    fn adding_time_extends_the_existing_deadline_after_time_has_elapsed() {
        let (session, _, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::OneHour,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(40 * 60));

        let first = session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::ThirtyMinutes,
            ))
            .expect("first add should succeed");
        clock.advance(Duration::from_secs(5 * 60));
        let second = session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            ))
            .expect("second add should succeed");

        assert_eq!(
            first.state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(50 * 60),
            }
        );
        assert_eq!(
            second.state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(60 * 60),
            }
        );
    }

    #[test]
    fn stopping_a_finite_session_removes_manual_intent_and_protection() {
        let (session, coordinator, _) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::ThirtyMinutes,
            ))
            .expect("start should succeed");

        let snapshot = session
            .command(ManualSessionCommand::Stop)
            .expect("stop should succeed");

        assert_eq!(snapshot.state, ManualSessionState::Inactive);
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn converting_a_finite_session_to_until_disabled_preserves_manual_wake_intent() {
        let (session, coordinator, _) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::ThirtyMinutes,
            ))
            .expect("start should succeed");

        let snapshot = session
            .command(ManualSessionCommand::ConvertToUntilDisabled)
            .expect("conversion should succeed");

        assert_eq!(snapshot.state, ManualSessionState::UntilDisabled);
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
    fn a_due_finite_session_expires_and_removes_manual_wake_intent() {
        let (session, coordinator, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(15 * 60));

        session.expire_due();

        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn every_finite_start_preset_projects_its_exact_remaining_duration() {
        let cases = [
            (
                ManualSessionStartPreset::FifteenMinutes,
                Duration::from_secs(15 * 60),
            ),
            (
                ManualSessionStartPreset::ThirtyMinutes,
                Duration::from_secs(30 * 60),
            ),
            (
                ManualSessionStartPreset::OneHour,
                Duration::from_secs(60 * 60),
            ),
            (
                ManualSessionStartPreset::TwoHours,
                Duration::from_secs(2 * 60 * 60),
            ),
            (
                ManualSessionStartPreset::FourHours,
                Duration::from_secs(4 * 60 * 60),
            ),
            (
                ManualSessionStartPreset::EightHours,
                Duration::from_secs(8 * 60 * 60),
            ),
        ];

        for (preset, remaining) in cases {
            let (session, _, _) = test_session(Duration::from_secs(123));

            let snapshot = session
                .command(ManualSessionCommand::Start(preset))
                .expect("finite start should succeed");

            assert_eq!(
                snapshot.state,
                ManualSessionState::Finite { remaining },
                "unexpected projection for {preset:?}"
            );
        }
    }

    #[test]
    fn until_disabled_has_no_deadline_and_can_be_stopped() {
        let (session, coordinator, clock) = test_session(Duration::ZERO);

        let active = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::UntilDisabled,
            ))
            .expect("until-disabled start should succeed");
        clock.advance(Duration::from_secs(24 * 60 * 60));
        session.expire_due();

        assert_eq!(active.state, ManualSessionState::UntilDisabled);
        assert_eq!(session.snapshot().state, ManualSessionState::UntilDisabled);
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::ManualKeepAwake]
        );

        session
            .command(ManualSessionCommand::Stop)
            .expect("stop should succeed");
        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
    }

    #[test]
    fn extending_a_session_neutralizes_its_original_deadline() {
        let (session, _, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(14 * 60));
        session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            ))
            .expect("add should succeed");
        clock.advance(Duration::from_secs(60));

        session.expire_due();

        assert_eq!(
            session.snapshot().state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(15 * 60),
            }
        );
    }

    #[test]
    fn conversion_neutralizes_the_old_finite_deadline() {
        let (session, _, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        session
            .command(ManualSessionCommand::ConvertToUntilDisabled)
            .expect("conversion should succeed");
        clock.advance(Duration::from_secs(60 * 60));

        session.expire_due();

        assert_eq!(session.snapshot().state, ManualSessionState::UntilDisabled);
    }

    #[test]
    fn stopping_then_starting_again_is_not_affected_by_the_old_deadline() {
        let (session, _, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("first start should succeed");
        session
            .command(ManualSessionCommand::Stop)
            .expect("stop should succeed");
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::OneHour,
            ))
            .expect("second start should succeed");
        clock.advance(Duration::from_secs(15 * 60));

        session.expire_due();

        assert_eq!(
            session.snapshot().state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(45 * 60),
            }
        );
    }

    #[test]
    fn add_after_expiry_is_rejected_without_resurrecting_the_session() {
        let (session, _, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(15 * 60));
        session.expire_due();

        let error = session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            ))
            .expect_err("add after expiry should fail");

        assert!(matches!(error, ManualSessionError::InvalidTransition));
        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
    }

    #[test]
    fn invalid_commands_are_logged_as_rejected() {
        let (log, lines) = LocalLog::recording();
        let (session, _, _) = test_session_with_log(Duration::ZERO, log);

        let error = session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            ))
            .expect_err("adding to an inactive session should fail");

        assert!(matches!(error, ManualSessionError::InvalidTransition));
        let persisted = lines.lock().unwrap().join("");
        assert!(persisted.contains("manual.command command=add outcome=rejected"));
        assert!(!persisted.contains("manual.command command=add outcome=failed"));
    }

    #[test]
    fn active_start_and_non_finite_extensions_are_rejected_without_mutation() {
        let (session, _, _) = test_session(Duration::ZERO);
        let finite = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::ThirtyMinutes,
            ))
            .expect("start should succeed");

        assert!(matches!(
            session.command(ManualSessionCommand::Start(
                ManualSessionStartPreset::OneHour
            )),
            Err(ManualSessionError::InvalidTransition)
        ));
        assert_eq!(session.snapshot(), finite);

        session
            .command(ManualSessionCommand::ConvertToUntilDisabled)
            .expect("conversion should succeed");
        assert!(matches!(
            session.command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes
            )),
            Err(ManualSessionError::InvalidTransition)
        ));
        assert!(matches!(
            session.command(ManualSessionCommand::ConvertToUntilDisabled),
            Err(ManualSessionError::InvalidTransition)
        ));
        assert_eq!(session.snapshot().state, ManualSessionState::UntilDisabled);
    }

    #[test]
    fn deadline_overflow_is_reported_without_mutating_the_finite_session() {
        let start_time = Duration::MAX
            .checked_sub(Duration::from_secs(30 * 60))
            .expect("test start time should fit");
        let (session, _, _) = test_session(start_time);
        let before = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should fit");

        let error = session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::ThirtyMinutes,
            ))
            .expect_err("overflow should fail");

        assert!(matches!(error, ManualSessionError::DeadlineOverflow));
        assert_eq!(session.snapshot(), before);
    }

    #[test]
    fn failed_acquire_preserves_intent_and_a_later_add_retries_coordination() {
        let (session, coordinator, _) = session_with_inhibitor(
            Duration::ZERO,
            FailingFirstAcquireInhibitor {
                fail_next: true,
                state: ProtectionState::Inactive,
            },
        );

        let error = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect_err("first acquire should fail");
        assert_eq!(error.to_string(), "scripted acquire failure");
        assert_eq!(
            session.snapshot().state,
            ManualSessionState::Finite {
                remaining: Duration::from_secs(15 * 60),
            }
        );
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::ManualKeepAwake]
        );
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );

        session
            .command(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            ))
            .expect("add should retry acquisition");
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Active
        );
    }

    #[test]
    fn failed_stop_release_keeps_intent_inactive_and_can_be_retried() {
        let (session, coordinator, _) = session_with_inhibitor(
            Duration::ZERO,
            FailingFirstReleaseInhibitor {
                fail_next: true,
                state: ProtectionState::Inactive,
            },
        );
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");

        let error = session
            .command(ManualSessionCommand::Stop)
            .expect_err("first release should fail");
        assert_eq!(error.to_string(), "scripted release failure");
        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Active
        );

        session
            .command(ManualSessionCommand::Stop)
            .expect("inactive stop should retry release");
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn failed_expiry_release_is_projected_until_successful_reconciliation() {
        let (session, coordinator, clock) = session_with_inhibitor(
            Duration::ZERO,
            FailingFirstReleaseInhibitor {
                fail_next: true,
                state: ProtectionState::Inactive,
            },
        );
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(15 * 60));

        session.expire_due();

        assert_eq!(
            session.snapshot(),
            ManualSessionSnapshot {
                state: ManualSessionState::Inactive,
                background_failure: Some("scripted release failure".to_owned()),
            }
        );
        assert!(coordinator.snapshot().active_reasons().is_empty());

        let recovered = session
            .command(ManualSessionCommand::Stop)
            .expect("stop should retry release");
        assert_eq!(recovered.background_failure, None);
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn stopping_or_expiring_manual_intent_preserves_automatic_protection() {
        for expire in [false, true] {
            let (session, coordinator, clock) = test_session(Duration::ZERO);
            coordinator
                .set_reason_active(WakeReason::AutomaticDownload, true)
                .expect("automatic reason should activate");
            session
                .command(ManualSessionCommand::Start(
                    ManualSessionStartPreset::FifteenMinutes,
                ))
                .expect("manual start should succeed");

            if expire {
                clock.advance(Duration::from_secs(15 * 60));
                session.expire_due();
            } else {
                session
                    .command(ManualSessionCommand::Stop)
                    .expect("manual stop should succeed");
            }

            assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
            assert_eq!(
                coordinator.snapshot().active_reasons(),
                &[WakeReason::AutomaticDownload]
            );
            assert_eq!(
                coordinator.snapshot().protection_state(),
                ProtectionState::Active
            );
        }
    }

    #[test]
    fn scheduler_shutdown_does_not_wait_for_a_pending_finite_deadline() {
        let coordinator = WakeCoordinator::with_inhibitor(
            TestInhibitor {
                state: ProtectionState::Inactive,
            },
            unavailable_log(),
        );
        let session = ManualSession::new(coordinator, unavailable_log());
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::EightHours,
            ))
            .expect("start should succeed");
        let scheduler = session.start_scheduler().expect("scheduler should start");

        scheduler.shutdown().expect("scheduler should stop");
    }

    #[test]
    fn the_scheduler_expires_a_session_due_at_worker_start() {
        let (released, release_observed) = std::sync::mpsc::channel();
        let (session, _, clock) = session_with_inhibitor(
            Duration::ZERO,
            NotifyingReleaseInhibitor {
                released,
                state: ProtectionState::Inactive,
            },
        );
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(15 * 60));
        let scheduler = session.start_scheduler().expect("scheduler should start");

        release_observed
            .recv()
            .expect("scheduler should release protection");
        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
        scheduler.shutdown().expect("scheduler should stop");
    }

    #[test]
    fn commands_are_rejected_after_scheduler_shutdown_begins() {
        let (session, _, _) = test_session(Duration::ZERO);
        let scheduler = session.start_scheduler().expect("scheduler should start");
        scheduler.shutdown().expect("scheduler should stop");

        let error = session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect_err("commands should stop with the scheduler");

        assert!(matches!(error, ManualSessionError::Unavailable));
        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
    }

    #[test]
    fn stop_and_expiry_racing_converge_on_one_inactive_session() {
        let (session, coordinator, clock) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        clock.advance(Duration::from_secs(15 * 60));
        let barrier = Arc::new(Barrier::new(3));
        let stop_session = session.clone();
        let stop_barrier = barrier.clone();
        let stop = std::thread::spawn(move || {
            stop_barrier.wait();
            stop_session.command(ManualSessionCommand::Stop)
        });
        let expiry_session = session.clone();
        let expiry_barrier = barrier.clone();
        let expiry = std::thread::spawn(move || {
            expiry_barrier.wait();
            expiry_session.expire_due();
        });

        barrier.wait();
        stop.join()
            .expect("stop thread should not panic")
            .expect("stop should converge successfully");
        expiry.join().expect("expiry thread should not panic");

        assert_eq!(session.snapshot().state, ManualSessionState::Inactive);
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
    }

    #[test]
    fn every_add_preset_accumulates_on_the_current_deadline() {
        let cases = [
            (
                ManualSessionAddPreset::FifteenMinutes,
                Duration::from_secs(15 * 60),
            ),
            (
                ManualSessionAddPreset::ThirtyMinutes,
                Duration::from_secs(30 * 60),
            ),
            (
                ManualSessionAddPreset::OneHour,
                Duration::from_secs(60 * 60),
            ),
            (
                ManualSessionAddPreset::TwoHours,
                Duration::from_secs(2 * 60 * 60),
            ),
            (
                ManualSessionAddPreset::FourHours,
                Duration::from_secs(4 * 60 * 60),
            ),
        ];
        let (session, _, _) = test_session(Duration::ZERO);
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            ))
            .expect("start should succeed");
        let mut remaining = Duration::from_secs(15 * 60);

        for (preset, added) in cases {
            remaining = remaining
                .checked_add(added)
                .expect("test duration overflow");
            let snapshot = session
                .command(ManualSessionCommand::Add(preset))
                .expect("add should succeed");
            assert_eq!(
                snapshot.state,
                ManualSessionState::Finite { remaining },
                "unexpected projection after {preset:?}"
            );
        }
    }

    #[test]
    fn application_lifecycle_stops_the_scheduler_before_wake_cleanup() {
        let (session, coordinator, _) = test_session(Duration::ZERO);
        let mut lifecycle = crate::test_lifecycle(coordinator.clone(), session.clone());
        lifecycle.start().expect("startup should succeed");
        session
            .command(ManualSessionCommand::Start(
                ManualSessionStartPreset::EightHours,
            ))
            .expect("start should succeed");

        let report = lifecycle.shutdown();

        assert!(report.failures().is_empty());
        assert!(coordinator.snapshot().active_reasons().is_empty());
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Inactive
        );
        assert!(matches!(
            session.command(ManualSessionCommand::Stop),
            Err(ManualSessionError::Unavailable)
        ));
    }
}
