use crate::{
    local_log::{EventOutcome, LocalLog, ManualCommandEvent, SafeEvent},
    wake_coordinator::{WakeCoordinationError, WakeCoordinator, WakeReason},
};
use std::{
    error::Error,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManualSessionState {
    Inactive,
    Finite { remaining: Duration },
    UntilDisabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualSessionSnapshot {
    pub(crate) state: ManualSessionState,
    pub(crate) background_failure: Option<String>,
}

struct ManualSessionCore {
    intent: ManualSessionIntent,
    background_failure: Option<String>,
    accepting_commands: bool,
    scheduler_started: bool,
}

#[derive(Clone, Copy)]
enum ManualSessionIntent {
    Inactive,
    Finite { deadline: Duration },
    UntilDisabled,
}

struct SharedState {
    core: Mutex<ManualSessionCore>,
    changed: Condvar,
}

trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualSessionStartPreset {
    FifteenMinutes,
    ThirtyMinutes,
    OneHour,
    TwoHours,
    FourHours,
    EightHours,
    UntilDisabled,
}

impl ManualSessionStartPreset {
    fn duration(self) -> Option<Duration> {
        match self {
            Self::FifteenMinutes => Some(Duration::from_secs(15 * 60)),
            Self::ThirtyMinutes => Some(Duration::from_secs(30 * 60)),
            Self::OneHour => Some(Duration::from_secs(60 * 60)),
            Self::TwoHours => Some(Duration::from_secs(2 * 60 * 60)),
            Self::FourHours => Some(Duration::from_secs(4 * 60 * 60)),
            Self::EightHours => Some(Duration::from_secs(8 * 60 * 60)),
            Self::UntilDisabled => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualSessionAddPreset {
    FifteenMinutes,
    ThirtyMinutes,
    OneHour,
    TwoHours,
    FourHours,
}

impl ManualSessionAddPreset {
    fn duration(self) -> Duration {
        match self {
            Self::FifteenMinutes => Duration::from_secs(15 * 60),
            Self::ThirtyMinutes => Duration::from_secs(30 * 60),
            Self::OneHour => Duration::from_secs(60 * 60),
            Self::TwoHours => Duration::from_secs(2 * 60 * 60),
            Self::FourHours => Duration::from_secs(4 * 60 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualSessionCommand {
    Start(ManualSessionStartPreset),
    Add(ManualSessionAddPreset),
    ConvertToUntilDisabled,
    Stop,
}

#[derive(Debug)]
pub(crate) enum ManualSessionError {
    InvalidTransition,
    DeadlineOverflow,
    Unavailable,
    WakeCoordination(WakeCoordinationError),
}

impl std::fmt::Display for ManualSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTransition => formatter.write_str("invalid manual session transition"),
            Self::DeadlineOverflow => formatter.write_str("manual session deadline overflow"),
            Self::Unavailable => formatter.write_str("manual session is shutting down"),
            Self::WakeCoordination(error) => error.fmt(formatter),
        }
    }
}

impl Error for ManualSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WakeCoordination(error) => Some(error),
            Self::InvalidTransition | Self::DeadlineOverflow | Self::Unavailable => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ManualSessionRuntimeError {
    AlreadyStarted,
    Spawn(std::io::Error),
    WorkerPanicked,
}

impl std::fmt::Display for ManualSessionRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("manual session scheduler already started"),
            Self::Spawn(error) => write!(
                formatter,
                "failed to start manual session scheduler: {error}"
            ),
            Self::WorkerPanicked => formatter.write_str("manual session scheduler panicked"),
        }
    }
}

impl Error for ManualSessionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::AlreadyStarted | Self::WorkerPanicked => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ManualSession {
    shared: Arc<SharedState>,
    coordinator: WakeCoordinator,
    clock: Arc<dyn Clock>,
    log: LocalLog,
}

impl ManualSession {
    pub(crate) fn new(coordinator: WakeCoordinator, log: LocalLog) -> Self {
        Self::from_clock(coordinator, Arc::new(MonotonicClock::new()), log)
    }

    fn from_clock(coordinator: WakeCoordinator, clock: Arc<dyn Clock>, log: LocalLog) -> Self {
        Self {
            shared: Arc::new(SharedState {
                core: Mutex::new(ManualSessionCore {
                    intent: ManualSessionIntent::Inactive,
                    background_failure: None,
                    accepting_commands: true,
                    scheduler_started: false,
                }),
                changed: Condvar::new(),
            }),
            coordinator,
            clock,
            log,
        }
    }

    #[cfg(test)]
    fn with_clock(coordinator: WakeCoordinator, clock: Arc<dyn Clock>, log: LocalLog) -> Self {
        Self::from_clock(coordinator, clock, log)
    }

    pub(crate) fn snapshot(&self) -> ManualSessionSnapshot {
        let core = self
            .shared
            .core
            .lock()
            .expect("manual session lock poisoned");
        self.snapshot_from(&core)
    }

    pub(crate) fn command(
        &self,
        command: ManualSessionCommand,
    ) -> Result<ManualSessionSnapshot, ManualSessionError> {
        let event = match command {
            ManualSessionCommand::Start(_) => ManualCommandEvent::Start,
            ManualSessionCommand::Add(_) => ManualCommandEvent::Add,
            ManualSessionCommand::ConvertToUntilDisabled => ManualCommandEvent::Convert,
            ManualSessionCommand::Stop => ManualCommandEvent::Stop,
        };
        let result = self.command_inner(command);
        self.record(SafeEvent::ManualCommand {
            command: event,
            outcome: match &result {
                Ok(_) => EventOutcome::Succeeded,
                Err(ManualSessionError::WakeCoordination(_)) => EventOutcome::Failed,
                Err(
                    ManualSessionError::Unavailable
                    | ManualSessionError::InvalidTransition
                    | ManualSessionError::DeadlineOverflow,
                ) => EventOutcome::Rejected,
            },
        });
        result
    }

    fn command_inner(
        &self,
        command: ManualSessionCommand,
    ) -> Result<ManualSessionSnapshot, ManualSessionError> {
        let mut core = self
            .shared
            .core
            .lock()
            .expect("manual session lock poisoned");
        if !core.accepting_commands {
            return Err(ManualSessionError::Unavailable);
        }
        let reason_active = match command {
            ManualSessionCommand::Start(preset) => {
                if !matches!(core.intent, ManualSessionIntent::Inactive) {
                    return Err(ManualSessionError::InvalidTransition);
                }
                core.intent = match preset.duration() {
                    Some(duration) => ManualSessionIntent::Finite {
                        deadline: self
                            .clock
                            .now()
                            .checked_add(duration)
                            .ok_or(ManualSessionError::DeadlineOverflow)?,
                    },
                    None => ManualSessionIntent::UntilDisabled,
                };
                true
            }
            ManualSessionCommand::Add(preset) => {
                let ManualSessionIntent::Finite { deadline } = core.intent else {
                    return Err(ManualSessionError::InvalidTransition);
                };
                core.intent = ManualSessionIntent::Finite {
                    deadline: deadline
                        .checked_add(preset.duration())
                        .ok_or(ManualSessionError::DeadlineOverflow)?,
                };
                true
            }
            ManualSessionCommand::ConvertToUntilDisabled => {
                if !matches!(core.intent, ManualSessionIntent::Finite { .. }) {
                    return Err(ManualSessionError::InvalidTransition);
                }
                core.intent = ManualSessionIntent::UntilDisabled;
                true
            }
            ManualSessionCommand::Stop => {
                core.intent = ManualSessionIntent::Inactive;
                false
            }
        };
        let coordination = self
            .coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, reason_active);
        self.shared.changed.notify_all();
        if let Err(error) = coordination {
            return Err(ManualSessionError::WakeCoordination(error));
        }
        core.background_failure = None;
        Ok(self.snapshot_from(&core))
    }

    pub(crate) fn start_scheduler(
        &self,
    ) -> Result<ManualSessionScheduler, ManualSessionRuntimeError> {
        {
            let mut core = self
                .shared
                .core
                .lock()
                .expect("manual session lock poisoned");
            if core.scheduler_started || !core.accepting_commands {
                return Err(ManualSessionRuntimeError::AlreadyStarted);
            }
            core.scheduler_started = true;
        }

        let session = self.clone();
        let worker = match thread::Builder::new()
            .name("chiu-manual-session".to_owned())
            .spawn(move || session.run_scheduler())
        {
            Ok(worker) => worker,
            Err(error) => {
                self.shared
                    .core
                    .lock()
                    .expect("manual session lock poisoned")
                    .scheduler_started = false;
                return Err(ManualSessionRuntimeError::Spawn(error));
            }
        };

        Ok(ManualSessionScheduler {
            session: self.clone(),
            worker: Some(worker),
        })
    }

    fn run_scheduler(&self) {
        let mut core = self
            .shared
            .core
            .lock()
            .expect("manual session lock poisoned");
        while core.accepting_commands {
            match core.intent {
                ManualSessionIntent::Finite { deadline } => {
                    let now = self.clock.now();
                    if deadline <= now {
                        drop(core);
                        self.expire_due();
                        core = self
                            .shared
                            .core
                            .lock()
                            .expect("manual session lock poisoned");
                        continue;
                    }
                    let wait = deadline.saturating_sub(now);
                    let (next, _) = self
                        .shared
                        .changed
                        .wait_timeout(core, wait)
                        .expect("manual session lock poisoned");
                    core = next;
                }
                ManualSessionIntent::Inactive | ManualSessionIntent::UntilDisabled => {
                    core = self
                        .shared
                        .changed
                        .wait(core)
                        .expect("manual session lock poisoned");
                }
            }
        }
    }

    fn request_scheduler_shutdown(&self) {
        let mut core = self
            .shared
            .core
            .lock()
            .expect("manual session lock poisoned");
        core.accepting_commands = false;
        self.shared.changed.notify_all();
    }

    fn expire_due(&self) {
        let mut core = self
            .shared
            .core
            .lock()
            .expect("manual session lock poisoned");
        let ManualSessionIntent::Finite { deadline } = core.intent else {
            return;
        };
        if deadline > self.clock.now() {
            return;
        }

        core.intent = ManualSessionIntent::Inactive;
        let outcome = match self
            .coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, false)
        {
            Ok(_) => {
                core.background_failure = None;
                EventOutcome::Succeeded
            }
            Err(error) => {
                core.background_failure = Some(error.to_string());
                EventOutcome::Failed
            }
        };
        self.shared.changed.notify_all();
        drop(core);
        self.record(SafeEvent::ManualExpired { outcome });
    }

    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }

    fn snapshot_from(&self, core: &ManualSessionCore) -> ManualSessionSnapshot {
        let state = match core.intent {
            ManualSessionIntent::Inactive => ManualSessionState::Inactive,
            ManualSessionIntent::Finite { deadline } => ManualSessionState::Finite {
                remaining: deadline.saturating_sub(self.clock.now()),
            },
            ManualSessionIntent::UntilDisabled => ManualSessionState::UntilDisabled,
        };
        ManualSessionSnapshot {
            state,
            background_failure: core.background_failure.clone(),
        }
    }
}

pub(crate) struct ManualSessionScheduler {
    session: ManualSession,
    worker: Option<JoinHandle<()>>,
}

impl ManualSessionScheduler {
    pub(crate) fn shutdown(mut self) -> Result<(), ManualSessionRuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), ManualSessionRuntimeError> {
        self.session.request_scheduler_shutdown();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| ManualSessionRuntimeError::WorkerPanicked),
            None => Ok(()),
        }
    }
}

impl Drop for ManualSessionScheduler {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[cfg(test)]
include!("manual_session/tests.rs");
