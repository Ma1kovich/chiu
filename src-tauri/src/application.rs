use std::{
    error::Error,
    sync::{Arc, Mutex},
};

type BoxError = Box<dyn Error + Send + Sync>;
type StartParticipant<S> = Box<dyn FnOnce(&S) -> Result<OwnedResource, BoxError> + Send>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CapabilityId(&'static str);

impl CapabilityId {
    pub(crate) const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupFailurePoint {
    Settings,
    Required(CapabilityId),
}

pub(crate) struct OwnedResource {
    stop: Box<dyn FnOnce() -> Result<(), BoxError> + Send>,
}

impl OwnedResource {
    pub(crate) fn new<F>(stop: F) -> Self
    where
        F: FnOnce() -> Result<(), BoxError> + Send + 'static,
    {
        Self {
            stop: Box::new(stop),
        }
    }

    fn stop(self) -> Result<(), BoxError> {
        (self.stop)()
    }
}

struct Participant<S> {
    capability: CapabilityId,
    required: bool,
    // A failed start must release anything it acquired; ownership transfers only through this token.
    start: StartParticipant<S>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApplicationStatus {
    Starting,
    Ready,
    Degraded,
    Failed,
    ShuttingDown,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApplicationSnapshot {
    status: ApplicationStatus,
    unavailable_capabilities: Vec<CapabilityId>,
    startup_failure: Option<StartupFailurePoint>,
    cleanup_failures: Vec<CapabilityId>,
}

impl ApplicationSnapshot {
    pub(crate) fn status(&self) -> ApplicationStatus {
        self.status
    }

    pub(crate) fn unavailable_capabilities(&self) -> &[CapabilityId] {
        &self.unavailable_capabilities
    }

    pub(crate) fn startup_failure(&self) -> Option<StartupFailurePoint> {
        self.startup_failure
    }

    pub(crate) fn cleanup_failures(&self) -> &[CapabilityId] {
        &self.cleanup_failures
    }
}

#[derive(Clone)]
pub(crate) struct ApplicationState {
    snapshot: Arc<Mutex<ApplicationSnapshot>>,
}

impl ApplicationState {
    fn new() -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(ApplicationSnapshot {
                status: ApplicationStatus::Starting,
                unavailable_capabilities: Vec::new(),
                startup_failure: None,
                cleanup_failures: Vec::new(),
            })),
        }
    }

    pub(crate) fn snapshot(&self) -> ApplicationSnapshot {
        self.snapshot
            .lock()
            .expect("application state lock poisoned")
            .clone()
    }

    fn set_status(&self, status: ApplicationStatus) {
        self.snapshot
            .lock()
            .expect("application state lock poisoned")
            .status = status;
    }

    fn record_unavailable(&self, capability: CapabilityId) {
        self.snapshot
            .lock()
            .expect("application state lock poisoned")
            .unavailable_capabilities
            .push(capability);
    }
}

pub(crate) struct LifecyclePlan<S> {
    restore_settings: Box<dyn FnOnce() -> Result<S, BoxError> + Send>,
    participants: Vec<Participant<S>>,
}

impl<S> LifecyclePlan<S> {
    pub(crate) fn new<F>(restore_settings: F) -> Self
    where
        F: FnOnce() -> Result<S, BoxError> + Send + 'static,
    {
        Self {
            restore_settings: Box::new(restore_settings),
            participants: Vec::new(),
        }
    }

    pub(crate) fn required<F>(mut self, capability: CapabilityId, start: F) -> Self
    where
        F: FnOnce(&S) -> Result<OwnedResource, BoxError> + Send + 'static,
    {
        self.participants.push(Participant {
            capability,
            required: true,
            start: Box::new(start),
        });
        self
    }

    pub(crate) fn optional<F>(mut self, capability: CapabilityId, start: F) -> Self
    where
        F: FnOnce(&S) -> Result<OwnedResource, BoxError> + Send + 'static,
    {
        self.participants.push(Participant {
            capability,
            required: false,
            start: Box::new(start),
        });
        self
    }

    pub(crate) fn build(self) -> ApplicationLifecycle<S> {
        ApplicationLifecycle {
            restore_settings: Some(self.restore_settings),
            participants: self.participants,
            resources: Vec::new(),
            state: ApplicationState::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct OperationFailure {
    capability: CapabilityId,
    message: String,
}

impl OperationFailure {
    pub(crate) fn capability(&self) -> CapabilityId {
        self.capability
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug)]
pub(crate) struct StartupReport {
    optional_failures: Vec<OperationFailure>,
}

impl StartupReport {
    pub(crate) fn optional_failures(&self) -> &[OperationFailure] {
        &self.optional_failures
    }
}

#[derive(Debug, Default)]
pub(crate) struct ShutdownReport {
    failures: Vec<OperationFailure>,
}

impl ShutdownReport {
    pub(crate) fn failures(&self) -> &[OperationFailure] {
        &self.failures
    }
}

#[derive(Debug)]
pub(crate) struct StartError {
    failure_point: Option<StartupFailurePoint>,
    message: String,
    cleanup_failures: Vec<OperationFailure>,
}

impl StartError {
    fn startup(
        failure_point: StartupFailurePoint,
        error: BoxError,
        cleanup_failures: Vec<OperationFailure>,
    ) -> Self {
        Self {
            failure_point: Some(failure_point),
            message: error.to_string(),
            cleanup_failures,
        }
    }

    fn invalid(status: ApplicationStatus) -> Self {
        Self {
            failure_point: None,
            message: format!("cannot start application lifecycle from {status:?}"),
            cleanup_failures: Vec::new(),
        }
    }

    pub(crate) fn failure_point(&self) -> Option<StartupFailurePoint> {
        self.failure_point
    }

    pub(crate) fn cleanup_failures(&self) -> &[OperationFailure] {
        &self.cleanup_failures
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for StartError {}

pub(crate) struct ApplicationLifecycle<S> {
    restore_settings: Option<Box<dyn FnOnce() -> Result<S, BoxError> + Send>>,
    participants: Vec<Participant<S>>,
    resources: Vec<(CapabilityId, OwnedResource)>,
    state: ApplicationState,
}

impl<S> ApplicationLifecycle<S> {
    pub(crate) fn start(&mut self) -> Result<StartupReport, StartError> {
        let status = self.state.snapshot().status();
        if status != ApplicationStatus::Starting {
            return Err(StartError::invalid(status));
        }

        let restore_settings = self
            .restore_settings
            .take()
            .ok_or_else(|| StartError::invalid(status))?;

        let settings = match restore_settings() {
            Ok(settings) => settings,
            Err(error) => {
                let failure_point = StartupFailurePoint::Settings;
                self.mark_startup_failed(failure_point);
                return Err(StartError::startup(failure_point, error, Vec::new()));
            }
        };

        let mut optional_failures = Vec::new();
        let participants = std::mem::take(&mut self.participants);
        for participant in participants {
            match (participant.start)(&settings) {
                Ok(resource) => self.resources.push((participant.capability, resource)),
                Err(error) if !participant.required => {
                    self.state.record_unavailable(participant.capability);
                    optional_failures.push(OperationFailure {
                        capability: participant.capability,
                        message: error.to_string(),
                    });
                }
                Err(error) => {
                    let failure_point = StartupFailurePoint::Required(participant.capability);
                    self.mark_startup_failed(failure_point);
                    let cleanup_failures = self.cleanup_owned_resources();
                    self.record_cleanup_failures(&cleanup_failures);
                    return Err(StartError::startup(failure_point, error, cleanup_failures));
                }
            }
        }
        if optional_failures.is_empty() {
            self.state.set_status(ApplicationStatus::Ready);
        } else {
            self.state.set_status(ApplicationStatus::Degraded);
        }

        Ok(StartupReport { optional_failures })
    }

    pub(crate) fn state(&self) -> ApplicationState {
        self.state.clone()
    }

    pub(crate) fn request_shutdown(&mut self) -> bool {
        let status = self.state.snapshot().status();
        if matches!(
            status,
            ApplicationStatus::ShuttingDown | ApplicationStatus::Stopped
        ) {
            return false;
        }

        self.state.set_status(ApplicationStatus::ShuttingDown);
        true
    }

    pub(crate) fn shutdown(&mut self) -> ShutdownReport {
        if self.state.snapshot().status() == ApplicationStatus::Stopped {
            return ShutdownReport::default();
        }

        self.request_shutdown();
        let failures = self.cleanup_owned_resources();
        self.record_cleanup_failures(&failures);
        self.state.set_status(ApplicationStatus::Stopped);

        ShutdownReport { failures }
    }

    fn mark_startup_failed(&self, failure_point: StartupFailurePoint) {
        let mut snapshot = self
            .state
            .snapshot
            .lock()
            .expect("application state lock poisoned");
        snapshot.status = ApplicationStatus::Failed;
        snapshot.startup_failure = Some(failure_point);
    }

    fn cleanup_owned_resources(&mut self) -> Vec<OperationFailure> {
        let mut failures = Vec::new();
        while let Some((capability, resource)) = self.resources.pop() {
            if let Err(error) = resource.stop() {
                failures.push(OperationFailure {
                    capability,
                    message: error.to_string(),
                });
            }
        }
        failures
    }

    fn record_cleanup_failures(&self, failures: &[OperationFailure]) {
        self.state
            .snapshot
            .lock()
            .expect("application state lock poisoned")
            .cleanup_failures
            .extend(failures.iter().map(OperationFailure::capability));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn startup_without_participants_becomes_ready() {
        let plan = LifecyclePlan::new(|| Ok(()));
        let mut lifecycle = plan.build();

        let report = lifecycle.start().expect("startup should succeed");

        assert!(report.optional_failures().is_empty());
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Ready
        );
    }

    #[test]
    fn settings_are_restored_before_ordered_participants_start() {
        struct TestSettings;

        let events = Arc::new(Mutex::new(Vec::new()));
        let settings_events = events.clone();
        let participant_events = events.clone();
        let plan = LifecyclePlan::new(move || {
            settings_events.lock().unwrap().push("settings");
            Ok(TestSettings)
        })
        .required(CapabilityId::new("core"), move |_: &TestSettings| {
            participant_events.lock().unwrap().push("core:start");
            Ok(OwnedResource::new(|| Ok(())))
        });
        let mut lifecycle = plan.build();

        lifecycle.start().expect("startup should succeed");

        assert_eq!(*events.lock().unwrap(), ["settings", "core:start"]);
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Ready
        );
    }

    #[test]
    fn optional_failure_degrades_state_and_later_required_startup_continues() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let optional_events = events.clone();
        let required_events = events.clone();
        let plan = LifecyclePlan::new(|| Ok(()))
            .optional(CapabilityId::new("updates"), move |_: &()| {
                optional_events.lock().unwrap().push("updates:start");
                Err(std::io::Error::other("offline").into())
            })
            .required(CapabilityId::new("core"), move |_: &()| {
                required_events.lock().unwrap().push("core:start");
                Ok(OwnedResource::new(|| Ok(())))
            });
        let mut lifecycle = plan.build();

        let report = lifecycle.start().expect("core startup should succeed");
        let snapshot = lifecycle.state().snapshot();

        assert_eq!(*events.lock().unwrap(), ["updates:start", "core:start"]);
        assert_eq!(
            report.optional_failures()[0].capability(),
            CapabilityId::new("updates")
        );
        assert_eq!(snapshot.status(), ApplicationStatus::Degraded);
        assert_eq!(
            snapshot.unavailable_capabilities(),
            &[CapabilityId::new("updates")]
        );
    }

    #[test]
    fn required_failure_preserves_prior_optional_unavailability() {
        let plan = LifecyclePlan::new(|| Ok(()))
            .optional(CapabilityId::new("updates"), |_: &()| {
                Err(std::io::Error::other("offline").into())
            })
            .required(CapabilityId::new("core"), |_: &()| {
                Err(std::io::Error::other("core failed").into())
            });
        let mut lifecycle = plan.build();

        lifecycle.start().expect_err("startup should fail");
        let snapshot = lifecycle.state().snapshot();

        assert_eq!(snapshot.status(), ApplicationStatus::Failed);
        assert_eq!(
            snapshot.startup_failure(),
            Some(StartupFailurePoint::Required(CapabilityId::new("core")))
        );
        assert_eq!(
            snapshot.unavailable_capabilities(),
            &[CapabilityId::new("updates")]
        );
    }

    #[test]
    fn required_failure_stops_later_startup_and_rolls_back_owned_resources() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let first_start_events = events.clone();
        let first_stop_events = events.clone();
        let failing_events = events.clone();
        let skipped_events = events.clone();
        let plan = LifecyclePlan::new(|| Ok(()))
            .required(CapabilityId::new("first"), move |_: &()| {
                first_start_events.lock().unwrap().push("first:start");
                Ok(OwnedResource::new(move || {
                    first_stop_events.lock().unwrap().push("first:stop");
                    Ok(())
                }))
            })
            .required(CapabilityId::new("failing"), move |_: &()| {
                failing_events.lock().unwrap().push("failing:start");
                Err(std::io::Error::other("required startup failed").into())
            })
            .required(CapabilityId::new("skipped"), move |_: &()| {
                skipped_events.lock().unwrap().push("skipped:start");
                Ok(OwnedResource::new(|| Ok(())))
            });
        let mut lifecycle = plan.build();

        let error = lifecycle.start().expect_err("startup should fail");
        let snapshot = lifecycle.state().snapshot();

        assert_eq!(
            *events.lock().unwrap(),
            ["first:start", "failing:start", "first:stop"]
        );
        assert_eq!(
            error.failure_point(),
            Some(StartupFailurePoint::Required(CapabilityId::new("failing")))
        );
        assert!(error.cleanup_failures().is_empty());
        assert_eq!(snapshot.status(), ApplicationStatus::Failed);
        assert_eq!(
            snapshot.startup_failure(),
            Some(StartupFailurePoint::Required(CapabilityId::new("failing")))
        );
    }

    #[test]
    fn shutdown_publishes_shutting_down_and_releases_resources_in_reverse_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut plan = LifecyclePlan::new(|| Ok(()));
        for name in ["first", "second", "third"] {
            let start_events = events.clone();
            let stop_events = events.clone();
            plan = plan.required(CapabilityId::new(name), move |_: &()| {
                start_events.lock().unwrap().push(format!("{name}:start"));
                Ok(OwnedResource::new(move || {
                    stop_events.lock().unwrap().push(format!("{name}:stop"));
                    Ok(())
                }))
            });
        }
        let mut lifecycle = plan.build();
        lifecycle.start().expect("startup should succeed");

        lifecycle.request_shutdown();
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::ShuttingDown
        );
        let report = lifecycle.shutdown();

        assert!(report.failures().is_empty());
        assert_eq!(
            *events.lock().unwrap(),
            [
                "first:start",
                "second:start",
                "third:start",
                "third:stop",
                "second:stop",
                "first:stop"
            ]
        );
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Stopped
        );
    }

    #[test]
    fn shutdown_reports_failures_without_skipping_remaining_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut plan = LifecyclePlan::new(|| Ok(()));
        for (name, should_fail) in [("first", false), ("second", true), ("third", true)] {
            let stop_events = events.clone();
            plan = plan.required(CapabilityId::new(name), move |_: &()| {
                Ok(OwnedResource::new(move || {
                    stop_events.lock().unwrap().push(name);
                    if should_fail {
                        Err(std::io::Error::other(format!("{name} failed")).into())
                    } else {
                        Ok(())
                    }
                }))
            });
        }
        let mut lifecycle = plan.build();
        lifecycle.start().expect("startup should succeed");

        let report = lifecycle.shutdown();
        let snapshot = lifecycle.state().snapshot();

        assert_eq!(*events.lock().unwrap(), ["third", "second", "first"]);
        assert_eq!(
            report
                .failures()
                .iter()
                .map(OperationFailure::capability)
                .collect::<Vec<_>>(),
            [CapabilityId::new("third"), CapabilityId::new("second")]
        );
        assert_eq!(report.failures()[0].message(), "third failed");
        assert_eq!(
            snapshot.cleanup_failures(),
            &[CapabilityId::new("third"), CapabilityId::new("second")]
        );
        assert_eq!(snapshot.status(), ApplicationStatus::Stopped);
    }

    #[test]
    fn duplicate_start_is_rejected_without_repeating_startup_work() {
        let settings_count = Arc::new(Mutex::new(0));
        let participant_count = Arc::new(Mutex::new(0));
        let settings_calls = settings_count.clone();
        let participant_calls = participant_count.clone();
        let plan = LifecyclePlan::new(move || {
            *settings_calls.lock().unwrap() += 1;
            Ok(())
        })
        .required(CapabilityId::new("core"), move |_: &()| {
            *participant_calls.lock().unwrap() += 1;
            Ok(OwnedResource::new(|| Ok(())))
        });
        let mut lifecycle = plan.build();
        lifecycle.start().expect("first startup should succeed");

        let error = lifecycle.start().expect_err("second startup should fail");

        assert_eq!(
            error.to_string(),
            "cannot start application lifecycle from Ready"
        );
        assert_eq!(*settings_count.lock().unwrap(), 1);
        assert_eq!(*participant_count.lock().unwrap(), 1);
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Ready
        );
    }

    #[test]
    fn repeated_shutdown_requests_and_cleanup_are_idempotent() {
        let stop_count = Arc::new(Mutex::new(0));
        let stop_calls = stop_count.clone();
        let plan =
            LifecyclePlan::new(|| Ok(())).required(CapabilityId::new("core"), move |_: &()| {
                Ok(OwnedResource::new(move || {
                    *stop_calls.lock().unwrap() += 1;
                    Ok(())
                }))
            });
        let mut lifecycle = plan.build();
        lifecycle.start().expect("startup should succeed");

        assert!(lifecycle.request_shutdown());
        assert!(!lifecycle.request_shutdown());
        assert!(lifecycle.shutdown().failures().is_empty());
        assert!(lifecycle.shutdown().failures().is_empty());

        assert_eq!(*stop_count.lock().unwrap(), 1);
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Stopped
        );
    }

    #[test]
    fn settings_failure_prevents_all_participant_startup() {
        let participant_started = Arc::new(Mutex::new(false));
        let participant_flag = participant_started.clone();
        let plan = LifecyclePlan::new(|| {
            Err::<(), BoxError>(std::io::Error::other("settings unavailable").into())
        })
        .required(CapabilityId::new("core"), move |_: &()| {
            *participant_flag.lock().unwrap() = true;
            Ok(OwnedResource::new(|| Ok(())))
        });
        let mut lifecycle = plan.build();

        let error = lifecycle.start().expect_err("startup should fail");

        assert_eq!(error.failure_point(), Some(StartupFailurePoint::Settings));
        assert!(!*participant_started.lock().unwrap());
        assert_eq!(
            lifecycle.state().snapshot().status(),
            ApplicationStatus::Failed
        );
    }

    #[test]
    fn startup_rollback_continues_and_reports_cleanup_failures() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let first_events = events.clone();
        let second_events = events.clone();
        let plan = LifecyclePlan::new(|| Ok(()))
            .required(CapabilityId::new("first"), move |_: &()| {
                Ok(OwnedResource::new(move || {
                    first_events.lock().unwrap().push("first");
                    Ok(())
                }))
            })
            .required(CapabilityId::new("second"), move |_: &()| {
                Ok(OwnedResource::new(move || {
                    second_events.lock().unwrap().push("second");
                    Err(std::io::Error::other("second cleanup failed").into())
                }))
            })
            .required(CapabilityId::new("failing"), move |_: &()| {
                Err(std::io::Error::other("startup failed").into())
            });
        let mut lifecycle = plan.build();

        let error = lifecycle.start().expect_err("startup should fail");
        let snapshot = lifecycle.state().snapshot();

        assert_eq!(*events.lock().unwrap(), ["second", "first"]);
        assert_eq!(
            error
                .cleanup_failures()
                .iter()
                .map(OperationFailure::capability)
                .collect::<Vec<_>>(),
            [CapabilityId::new("second")]
        );
        assert_eq!(snapshot.cleanup_failures(), &[CapabilityId::new("second")]);
        assert_eq!(snapshot.status(), ApplicationStatus::Failed);
    }

    #[test]
    fn state_is_coherent_through_degraded_startup_and_shutdown() {
        let plan = LifecyclePlan::new(|| Ok(()))
            .optional(CapabilityId::new("updates"), |_: &()| {
                Err(std::io::Error::other("offline").into())
            });
        let mut lifecycle = plan.build();
        let state = lifecycle.state();

        assert_eq!(state.snapshot().status(), ApplicationStatus::Starting);
        lifecycle
            .start()
            .expect("core startup should remain usable");
        assert_eq!(state.snapshot().status(), ApplicationStatus::Degraded);
        lifecycle.request_shutdown();
        assert_eq!(state.snapshot().status(), ApplicationStatus::ShuttingDown);
        lifecycle.shutdown();
        assert_eq!(state.snapshot().status(), ApplicationStatus::Stopped);
    }
}
