use crate::{
    automatic_protection::{AutomaticIntentConsumer, AutomaticIntentError},
    local_log::{DetectorStateEvent, LocalLog, SafeEvent},
    network_activity::{NetworkActivityConsumer, NetworkActivityEvent, ReceiveActivitySample},
    settings::{SettingsService, SettingsUpdate, SettingsUpdateError},
};
use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

const PROVISIONAL_MEANINGFUL_RECEIVE_RATE_BYTES_PER_SECOND: u64 = 1024 * 1024;
const PROVISIONAL_ACTIVATION_QUALIFICATION: Duration = Duration::from_secs(5);
const PROVISIONAL_GRACE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticDownloadSettings {
    meaningful_receive_rate_bytes_per_second: NonZeroU64,
    activation_qualification: Duration,
    grace: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticDownloadSettingsError {
    ZeroMeaningfulReceiveRate,
    ZeroActivationQualification,
}

impl AutomaticDownloadSettings {
    pub(crate) fn try_new(
        meaningful_receive_rate_bytes_per_second: u64,
        activation_qualification: Duration,
        grace: Duration,
    ) -> Result<Self, AutomaticDownloadSettingsError> {
        let meaningful_receive_rate_bytes_per_second =
            NonZeroU64::new(meaningful_receive_rate_bytes_per_second)
                .ok_or(AutomaticDownloadSettingsError::ZeroMeaningfulReceiveRate)?;
        if activation_qualification.is_zero() {
            return Err(AutomaticDownloadSettingsError::ZeroActivationQualification);
        }
        Ok(Self {
            meaningful_receive_rate_bytes_per_second,
            activation_qualification,
            grace,
        })
    }

    pub(crate) fn meaningful_receive_rate_bytes_per_second(self) -> u64 {
        self.meaningful_receive_rate_bytes_per_second.get()
    }

    pub(crate) fn activation_qualification(self) -> Duration {
        self.activation_qualification
    }

    pub(crate) fn grace(self) -> Duration {
        self.grace
    }
}

impl std::fmt::Display for AutomaticDownloadSettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMeaningfulReceiveRate => {
                formatter.write_str("meaningful receive-rate threshold must be non-zero")
            }
            Self::ZeroActivationQualification => {
                formatter.write_str("activation qualification duration must be non-zero")
            }
        }
    }
}

impl std::error::Error for AutomaticDownloadSettingsError {}

impl Default for AutomaticDownloadSettings {
    fn default() -> Self {
        Self::try_new(
            PROVISIONAL_MEANINGFUL_RECEIVE_RATE_BYTES_PER_SECOND,
            PROVISIONAL_ACTIVATION_QUALIFICATION,
            PROVISIONAL_GRACE,
        )
        .expect("provisional detector settings are valid")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticDownloadState {
    Idle,
    Active,
    Hold { remaining_grace: Duration },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticDownloadSnapshot {
    pub(crate) state: AutomaticDownloadState,
    pub(crate) latest_receive_rate_bytes_per_second: Option<u128>,
    pub(crate) settings: AutomaticDownloadSettings,
}

struct MonitorCore {
    phase: DetectorPhase,
    latest_receive_rate_bytes_per_second: Option<u128>,
    settings: AutomaticDownloadSettings,
}

#[derive(Clone, Copy)]
enum DetectorPhase {
    Idle { activation_elapsed: Duration },
    Active,
    Hold { quiet_elapsed: Duration },
}

#[derive(Clone)]
pub(crate) struct AutomaticDownloadMonitor {
    core: Arc<Mutex<MonitorCore>>,
    intent_consumer: Arc<dyn AutomaticIntentConsumer>,
    log: LocalLog,
}

#[derive(Clone)]
pub(crate) struct AutomaticDownloadTuningService {
    settings: SettingsService,
    monitor: AutomaticDownloadMonitor,
    operation: Arc<Mutex<()>>,
}

impl AutomaticDownloadTuningService {
    pub(crate) fn new(settings: SettingsService, monitor: AutomaticDownloadMonitor) -> Self {
        Self {
            settings,
            monitor,
            operation: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn set_threshold(
        &self,
        meaningful_receive_rate_bytes_per_second: u64,
    ) -> Result<AutomaticDownloadSnapshot, SettingsUpdateError> {
        self.update(|current| {
            AutomaticDownloadSettings::try_new(
                meaningful_receive_rate_bytes_per_second,
                current.activation_qualification(),
                current.grace(),
            )
            .map_err(SettingsUpdateError::InvalidAutomaticDownload)
        })
    }

    pub(crate) fn set_grace(
        &self,
        grace: Duration,
    ) -> Result<AutomaticDownloadSnapshot, SettingsUpdateError> {
        self.update(|current| {
            AutomaticDownloadSettings::try_new(
                current.meaningful_receive_rate_bytes_per_second(),
                current.activation_qualification(),
                grace,
            )
            .map_err(SettingsUpdateError::InvalidAutomaticDownload)
        })
    }

    fn update(
        &self,
        change: impl FnOnce(
            AutomaticDownloadSettings,
        ) -> Result<AutomaticDownloadSettings, SettingsUpdateError>,
    ) -> Result<AutomaticDownloadSnapshot, SettingsUpdateError> {
        let _operation = self
            .operation
            .lock()
            .expect("automatic download tuning operation lock poisoned");
        let current = self.monitor.snapshot().settings;
        let updated = change(current)?;
        if updated == current {
            return Ok(self.monitor.snapshot());
        }
        self.settings
            .update(SettingsUpdate::AutomaticDownloadTuning {
                meaningful_receive_rate_bytes_per_second: updated
                    .meaningful_receive_rate_bytes_per_second(),
                activation_qualification_milliseconds: duration_milliseconds(
                    updated.activation_qualification(),
                ),
                grace_milliseconds: duration_milliseconds(updated.grace()),
            })?;
        Ok(self.monitor.apply_settings(updated))
    }
}

fn duration_milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .expect("persisted detector durations fit in u64 milliseconds")
}

impl AutomaticDownloadMonitor {
    pub(crate) fn new(
        settings: AutomaticDownloadSettings,
        intent_consumer: impl AutomaticIntentConsumer + 'static,
        log: LocalLog,
    ) -> Self {
        Self {
            core: Arc::new(Mutex::new(MonitorCore {
                phase: DetectorPhase::Idle {
                    activation_elapsed: Duration::ZERO,
                },
                latest_receive_rate_bytes_per_second: None,
                settings,
            })),
            intent_consumer: Arc::new(intent_consumer),
            log,
        }
    }

    pub(crate) fn snapshot(&self) -> AutomaticDownloadSnapshot {
        let core = self.core.lock().expect("automatic download lock poisoned");
        Self::snapshot_from(&core)
    }

    pub(crate) fn apply_settings(
        &self,
        settings: AutomaticDownloadSettings,
    ) -> AutomaticDownloadSnapshot {
        let mut core = self.core.lock().expect("automatic download lock poisoned");
        core.settings = settings;
        core.reset();
        let _ = self.intent_consumer.set_automatic_intent(false);
        Self::snapshot_from(&core)
    }

    fn snapshot_from(core: &MonitorCore) -> AutomaticDownloadSnapshot {
        AutomaticDownloadSnapshot {
            state: match core.phase {
                DetectorPhase::Idle { .. } => AutomaticDownloadState::Idle,
                DetectorPhase::Active => AutomaticDownloadState::Active,
                DetectorPhase::Hold { quiet_elapsed } => AutomaticDownloadState::Hold {
                    remaining_grace: core.settings.grace.saturating_sub(quiet_elapsed),
                },
            },
            latest_receive_rate_bytes_per_second: core.latest_receive_rate_bytes_per_second,
            settings: core.settings,
        }
    }

    pub(crate) fn shutdown(&self) -> Result<(), AutomaticIntentError> {
        let mut core = self.core.lock().expect("automatic download lock poisoned");
        core.reset();
        self.intent_consumer.set_automatic_intent(false)
    }
}

impl NetworkActivityConsumer for AutomaticDownloadMonitor {
    fn consume(&mut self, event: NetworkActivityEvent) {
        let mut core = self.core.lock().expect("automatic download lock poisoned");
        let before = detector_event(&core.phase);
        let continuity_lost = matches!(event, NetworkActivityEvent::ContinuityLost(_));
        match event {
            NetworkActivityEvent::Sample(sample) => {
                self.apply_sample(&mut core, sample);
            }
            NetworkActivityEvent::ContinuityLost(_) => core.reset(),
            NetworkActivityEvent::AvailabilityChanged(_) => {}
        }
        let after = detector_event(&core.phase);
        let desired = !matches!(core.phase, DetectorPhase::Idle { .. });
        let _ = self.intent_consumer.set_automatic_intent(desired);
        drop(core);
        if continuity_lost {
            self.record(SafeEvent::DetectorContinuityReset);
        }
        if before != after {
            self.record(SafeEvent::DetectorTransition {
                from: before,
                to: after,
            });
        }
    }
}

impl AutomaticDownloadMonitor {
    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }

    fn apply_sample(&self, core: &mut MonitorCore, sample: ReceiveActivitySample) {
        let observed_over = sample.observed_over();
        let observed_nanos = observed_over.as_nanos();
        let receive_rate = u128::from(sample.received_bytes()) * 1_000_000_000 / observed_nanos;
        core.latest_receive_rate_bytes_per_second = Some(receive_rate);
        let meaningful = receive_rate
            >= u128::from(core.settings.meaningful_receive_rate_bytes_per_second.get());

        match core.phase {
            DetectorPhase::Idle { activation_elapsed } => {
                if meaningful {
                    let activation_elapsed = activation_elapsed
                        .saturating_add(observed_over)
                        .min(core.settings.activation_qualification);
                    core.phase = if activation_elapsed >= core.settings.activation_qualification {
                        DetectorPhase::Active
                    } else {
                        DetectorPhase::Idle { activation_elapsed }
                    };
                } else {
                    core.phase = DetectorPhase::Idle {
                        activation_elapsed: Duration::ZERO,
                    };
                }
            }
            DetectorPhase::Active => {
                if !meaningful {
                    core.phase = if observed_over >= core.settings.grace {
                        DetectorPhase::Idle {
                            activation_elapsed: Duration::ZERO,
                        }
                    } else {
                        DetectorPhase::Hold {
                            quiet_elapsed: observed_over,
                        }
                    };
                }
            }
            DetectorPhase::Hold { .. } if meaningful => {
                core.phase = DetectorPhase::Active;
            }
            DetectorPhase::Hold { quiet_elapsed } => {
                let quiet_elapsed = quiet_elapsed
                    .saturating_add(observed_over)
                    .min(core.settings.grace);
                core.phase = if quiet_elapsed >= core.settings.grace {
                    DetectorPhase::Idle {
                        activation_elapsed: Duration::ZERO,
                    }
                } else {
                    DetectorPhase::Hold { quiet_elapsed }
                };
            }
        }
    }
}

fn detector_event(phase: &DetectorPhase) -> DetectorStateEvent {
    match phase {
        DetectorPhase::Idle { .. } => DetectorStateEvent::Idle,
        DetectorPhase::Active => DetectorStateEvent::Active,
        DetectorPhase::Hold { .. } => DetectorStateEvent::Hold,
    }
}

impl MonitorCore {
    fn reset(&mut self) {
        self.phase = DetectorPhase::Idle {
            activation_elapsed: Duration::ZERO,
        };
        self.latest_receive_rate_bytes_per_second = None;
    }
}

#[cfg(test)]
mod tests;
