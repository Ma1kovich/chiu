use crate::{
    automatic_protection::{AutomaticProtectionController, PowerSource},
    local_log::{LocalLog, SafeEvent},
};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PowerSourceObserverHealth {
    Starting,
    Observing,
    Degraded,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PowerSourceOperation {
    Registration,
    Sample,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unregistration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PowerSourceFailure {
    operation: PowerSourceOperation,
    code: Option<u32>,
}

impl PowerSourceFailure {
    pub(crate) const fn new(operation: PowerSourceOperation, code: Option<u32>) -> Self {
        Self { operation, code }
    }

    pub(crate) fn operation(&self) -> PowerSourceOperation {
        self.operation
    }

    pub(crate) fn code(&self) -> Option<u32> {
        self.code
    }
}

impl std::fmt::Display for PowerSourceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "power source {:?} failed", self.operation)?;
        if let Some(code) = self.code {
            write!(formatter, " with platform code {code}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PowerSourceFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PowerSourceObserverSnapshot {
    source: PowerSource,
    health: PowerSourceObserverHealth,
    latest_failure: Option<PowerSourceFailure>,
}

impl PowerSourceObserverSnapshot {
    pub(crate) fn source(&self) -> PowerSource {
        self.source
    }

    pub(crate) fn health(&self) -> PowerSourceObserverHealth {
        self.health
    }

    pub(crate) fn latest_failure(&self) -> Option<&PowerSourceFailure> {
        self.latest_failure.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct PowerSourceObserver {
    state: Arc<Mutex<PowerSourceObserverSnapshot>>,
    controller: AutomaticProtectionController,
    log: LocalLog,
}

impl PowerSourceObserver {
    pub(crate) fn new(controller: AutomaticProtectionController, log: LocalLog) -> Self {
        Self {
            state: Arc::new(Mutex::new(PowerSourceObserverSnapshot {
                source: PowerSource::Unknown,
                health: PowerSourceObserverHealth::Starting,
                latest_failure: None,
            })),
            controller,
            log,
        }
    }

    pub(crate) fn start(&self) -> Result<PowerSourceRuntime, PowerSourceFailure> {
        platform_start(self.clone())
    }

    pub(crate) fn snapshot(&self) -> PowerSourceObserverSnapshot {
        self.state
            .lock()
            .expect("power source observer lock poisoned")
            .clone()
    }

    fn publish(&self, result: Result<PowerSource, PowerSourceFailure>) {
        let previous = self.snapshot();
        let source = match result {
            Ok(source) => {
                let mut state = self
                    .state
                    .lock()
                    .expect("power source observer lock poisoned");
                state.source = source;
                state.health = PowerSourceObserverHealth::Observing;
                state.latest_failure = None;
                source
            }
            Err(failure) => {
                let mut state = self
                    .state
                    .lock()
                    .expect("power source observer lock poisoned");
                state.source = PowerSource::Unknown;
                state.health = PowerSourceObserverHealth::Degraded;
                state.latest_failure = Some(failure);
                PowerSource::Unknown
            }
        };
        self.controller.set_power_source(source);
        self.record_state_if_changed(&previous);
    }

    fn stopped(&self, failure: Option<PowerSourceFailure>) {
        let previous = self.snapshot();
        {
            let mut state = self
                .state
                .lock()
                .expect("power source observer lock poisoned");
            state.source = PowerSource::Unknown;
            state.health = PowerSourceObserverHealth::Stopped;
            if failure.is_some() {
                state.latest_failure = failure;
            }
        }
        self.controller.set_power_source(PowerSource::Unknown);
        self.record_state_if_changed(&previous);
    }

    fn record_state_if_changed(&self, previous: &PowerSourceObserverSnapshot) {
        let snapshot = self.snapshot();
        if snapshot.source == previous.source && snapshot.health == previous.health {
            return;
        }
        self.log.record(SafeEvent::PowerChanged {
            source: power_source_name(snapshot.source),
            health: power_health_name(snapshot.health),
        });
    }
}

fn power_source_name(source: PowerSource) -> &'static str {
    match source {
        PowerSource::External => "external",
        PowerSource::Limited => "limited",
        PowerSource::Unknown => "unknown",
    }
}

fn power_health_name(health: PowerSourceObserverHealth) -> &'static str {
    match health {
        PowerSourceObserverHealth::Starting => "starting",
        PowerSourceObserverHealth::Observing => "observing",
        PowerSourceObserverHealth::Degraded => "degraded",
        PowerSourceObserverHealth::Stopped => "stopped",
    }
}

#[cfg(target_os = "macos")]
type PlatformRuntime = macos::MacPowerSourceRuntime;
#[cfg(target_os = "windows")]
type PlatformRuntime = windows::WindowsPowerSourceRuntime;

pub(crate) struct PowerSourceRuntime {
    platform: Option<PlatformRuntime>,
    observer: PowerSourceObserver,
}

impl PowerSourceRuntime {
    fn new(platform: PlatformRuntime, observer: PowerSourceObserver) -> Self {
        Self {
            platform: Some(platform),
            observer,
        }
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), PowerSourceFailure> {
        let result = match self.platform.take() {
            Some(platform) => platform.shutdown(),
            None => Ok(()),
        };
        self.observer.stopped(result.as_ref().err().cloned());
        result
    }
}

impl Drop for PowerSourceRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(target_os = "macos")]
fn platform_start(observer: PowerSourceObserver) -> Result<PowerSourceRuntime, PowerSourceFailure> {
    macos::start(observer.clone()).map(|runtime| PowerSourceRuntime::new(runtime, observer))
}

#[cfg(target_os = "windows")]
fn platform_start(observer: PowerSourceObserver) -> Result<PowerSourceRuntime, PowerSourceFailure> {
    windows::start(observer.clone()).map(|runtime| PowerSourceRuntime::new(runtime, observer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        automatic_protection::AutomaticProtectionSettings,
        local_log::LoggingFailureStage,
        wake_coordinator::{WakeCoordinator, WakeReason},
    };

    fn unavailable_log() -> LocalLog {
        LocalLog::unavailable(LoggingFailureStage::Open)
    }

    fn controller(coordinator: WakeCoordinator) -> AutomaticProtectionController {
        AutomaticProtectionController::new(
            AutomaticProtectionSettings::new(true, false),
            coordinator,
            unavailable_log(),
        )
    }

    fn observer(controller: AutomaticProtectionController) -> PowerSourceObserver {
        PowerSourceObserver::new(controller, unavailable_log())
    }

    #[test]
    fn sample_failure_projects_unknown_power_and_degraded_health() {
        let coordinator = WakeCoordinator::new(unavailable_log());
        let controller = controller(coordinator.clone());
        controller.set_detector_intent(true);
        controller.set_power_source(PowerSource::External);
        let observer = observer(controller);

        observer.publish(Err(PowerSourceFailure::new(
            PowerSourceOperation::Sample,
            Some(7),
        )));

        assert_eq!(observer.snapshot().source(), PowerSource::Unknown);
        assert_eq!(
            observer.snapshot().health(),
            PowerSourceObserverHealth::Degraded
        );
        assert_eq!(
            observer.snapshot().latest_failure().unwrap().code(),
            Some(7)
        );
        assert!(coordinator.snapshot().active_reasons().is_empty());
    }

    #[test]
    fn a_valid_sample_recovers_observer_health_and_policy() {
        let coordinator = WakeCoordinator::new(unavailable_log());
        let controller = controller(coordinator.clone());
        controller.set_detector_intent(true);
        let observer = observer(controller);
        observer.publish(Err(PowerSourceFailure::new(
            PowerSourceOperation::Sample,
            None,
        )));

        observer.publish(Ok(PowerSource::External));

        assert_eq!(
            observer.snapshot().health(),
            PowerSourceObserverHealth::Observing
        );
        assert_eq!(observer.snapshot().latest_failure(), None);
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::AutomaticDownload]
        );
    }

    #[test]
    fn stopping_observation_invalidates_known_external_power() {
        let coordinator = WakeCoordinator::new(unavailable_log());
        let controller = controller(coordinator.clone());
        controller.set_detector_intent(true);
        let observer = observer(controller);
        observer.publish(Ok(PowerSource::External));

        observer.stopped(Some(PowerSourceFailure::new(
            PowerSourceOperation::Unregistration,
            Some(9),
        )));

        assert_eq!(observer.snapshot().source(), PowerSource::Unknown);
        assert_eq!(
            observer.snapshot().health(),
            PowerSourceObserverHealth::Stopped
        );
        assert_eq!(
            observer.snapshot().latest_failure().unwrap().code(),
            Some(9)
        );
        assert!(coordinator.snapshot().active_reasons().is_empty());
    }
}
