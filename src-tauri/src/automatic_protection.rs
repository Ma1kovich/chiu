use crate::{
    local_log::{EventOutcome, LocalLog, SafeEvent},
    settings::{Settings, SettingsService, SettingsUpdate, SettingsUpdateError},
    wake_coordinator::{WakeCoordinator, WakeReason},
};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub(crate) struct AutomaticIntentError(String);

impl std::fmt::Display for AutomaticIntentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AutomaticIntentError {}

pub(crate) trait AutomaticIntentConsumer: Send + Sync {
    fn set_automatic_intent(&self, active: bool) -> Result<(), AutomaticIntentError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PowerSource {
    External,
    Limited,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticProtectionSettings {
    automatic_download_protection_enabled: bool,
    protect_on_battery: bool,
}

impl AutomaticProtectionSettings {
    pub(crate) const fn new(
        automatic_download_protection_enabled: bool,
        protect_on_battery: bool,
    ) -> Self {
        Self {
            automatic_download_protection_enabled,
            protect_on_battery,
        }
    }

    fn from_settings(settings: &Settings) -> Self {
        Self::new(
            settings.automatic_download_protection_enabled(),
            settings.protect_on_battery(),
        )
    }

    pub(crate) fn automatic_download_protection_enabled(self) -> bool {
        self.automatic_download_protection_enabled
    }

    pub(crate) fn protect_on_battery(self) -> bool {
        self.protect_on_battery
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticProtectionSuppression {
    AutomaticDisabled,
    LimitedPower,
    UnknownPower,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticProtectionSnapshot {
    detector_intent: bool,
    settings: AutomaticProtectionSettings,
    power_source: PowerSource,
    desired_automatic_reason: bool,
    suppression_reason: Option<AutomaticProtectionSuppression>,
    latest_coordination_failure: Option<String>,
}

impl AutomaticProtectionSnapshot {
    pub(crate) fn detector_intent(&self) -> bool {
        self.detector_intent
    }

    pub(crate) fn settings(&self) -> AutomaticProtectionSettings {
        self.settings
    }

    pub(crate) fn power_source(&self) -> PowerSource {
        self.power_source
    }

    pub(crate) fn desired_automatic_reason(&self) -> bool {
        self.desired_automatic_reason
    }

    pub(crate) fn suppression_reason(&self) -> Option<AutomaticProtectionSuppression> {
        self.suppression_reason
    }

    pub(crate) fn latest_coordination_failure(&self) -> Option<&str> {
        self.latest_coordination_failure.as_deref()
    }
}

#[derive(Clone)]
pub(crate) struct AutomaticProtectionService {
    settings: SettingsService,
    controller: AutomaticProtectionController,
    operation: Arc<Mutex<()>>,
}

impl AutomaticProtectionService {
    pub(crate) fn new(
        settings: SettingsService,
        controller: AutomaticProtectionController,
    ) -> Self {
        Self {
            settings,
            controller,
            operation: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_automatic_download_protection_enabled(
        &self,
        enabled: bool,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        self.update(SettingsUpdate::AutomaticDownloadProtectionEnabled(enabled))
    }

    #[cfg(test)]
    pub(crate) fn set_protect_on_battery(
        &self,
        enabled: bool,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        self.update(SettingsUpdate::ProtectOnBattery(enabled))
    }

    pub(crate) fn toggle_automatic_download_protection(
        &self,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        let _operation = self
            .operation
            .lock()
            .expect("automatic protection operation lock poisoned");
        let enabled = !self
            .controller
            .snapshot()
            .settings()
            .automatic_download_protection_enabled();
        self.update_locked(SettingsUpdate::AutomaticDownloadProtectionEnabled(enabled))
    }

    pub(crate) fn toggle_protect_on_battery(
        &self,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        let _operation = self
            .operation
            .lock()
            .expect("automatic protection operation lock poisoned");
        let enabled = !self.controller.snapshot().settings().protect_on_battery();
        self.update_locked(SettingsUpdate::ProtectOnBattery(enabled))
    }

    #[cfg(test)]
    fn update(
        &self,
        update: SettingsUpdate,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        let _operation = self
            .operation
            .lock()
            .expect("automatic protection operation lock poisoned");
        self.update_locked(update)
    }

    fn update_locked(
        &self,
        update: SettingsUpdate,
    ) -> Result<AutomaticProtectionSnapshot, SettingsUpdateError> {
        let persisted = self.settings.update(update)?;
        Ok(self
            .controller
            .set_settings(AutomaticProtectionSettings::from_settings(&persisted)))
    }
}

struct AutomaticProtectionState {
    detector_intent: bool,
    settings: AutomaticProtectionSettings,
    power_source: PowerSource,
    last_commanded_desired: bool,
    latest_coordination_failure: Option<String>,
}

#[derive(Clone)]
pub(crate) struct AutomaticProtectionController {
    state: Arc<Mutex<AutomaticProtectionState>>,
    coordinator: WakeCoordinator,
    log: LocalLog,
}

impl AutomaticProtectionController {
    pub(crate) fn new(
        settings: AutomaticProtectionSettings,
        coordinator: WakeCoordinator,
        log: LocalLog,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(AutomaticProtectionState {
                detector_intent: false,
                settings,
                power_source: PowerSource::Unknown,
                last_commanded_desired: false,
                latest_coordination_failure: None,
            })),
            coordinator,
            log,
        }
    }

    pub(crate) fn set_detector_intent(&self, detector_intent: bool) -> AutomaticProtectionSnapshot {
        let mut state = self
            .state
            .lock()
            .expect("automatic protection lock poisoned");
        let previous = state.snapshot();
        state.detector_intent = detector_intent;
        self.reconcile(&mut state);
        let current = state.snapshot();
        drop(state);
        self.record_eligibility(&previous, &current);
        current
    }

    pub(crate) fn snapshot(&self) -> AutomaticProtectionSnapshot {
        self.state
            .lock()
            .expect("automatic protection lock poisoned")
            .snapshot()
    }

    pub(crate) fn set_power_source(
        &self,
        power_source: PowerSource,
    ) -> AutomaticProtectionSnapshot {
        let mut state = self
            .state
            .lock()
            .expect("automatic protection lock poisoned");
        let previous = state.snapshot();
        state.power_source = power_source;
        self.reconcile(&mut state);
        let current = state.snapshot();
        drop(state);
        self.record_eligibility(&previous, &current);
        current
    }

    pub(crate) fn set_settings(
        &self,
        settings: AutomaticProtectionSettings,
    ) -> AutomaticProtectionSnapshot {
        let mut state = self
            .state
            .lock()
            .expect("automatic protection lock poisoned");
        let previous = state.snapshot();
        state.settings = settings;
        self.reconcile(&mut state);
        let current = state.snapshot();
        drop(state);
        self.record_eligibility(&previous, &current);
        current
    }

    fn reconcile(&self, state: &mut AutomaticProtectionState) {
        let desired = state.desired_automatic_reason();
        if desired == state.last_commanded_desired && state.latest_coordination_failure.is_none() {
            return;
        }
        state.last_commanded_desired = desired;
        match self
            .coordinator
            .set_reason_active(WakeReason::AutomaticDownload, desired)
        {
            Ok(_) => {
                state.latest_coordination_failure = None;
                self.record(SafeEvent::AutomaticReconciliation {
                    active: desired,
                    outcome: EventOutcome::Succeeded,
                });
            }
            Err(error) => {
                state.latest_coordination_failure = Some(error.to_string());
                self.record(SafeEvent::AutomaticReconciliation {
                    active: desired,
                    outcome: EventOutcome::Failed,
                });
            }
        }
    }

    fn record_eligibility(
        &self,
        previous: &AutomaticProtectionSnapshot,
        current: &AutomaticProtectionSnapshot,
    ) {
        if previous.desired_automatic_reason != current.desired_automatic_reason
            || previous.suppression_reason != current.suppression_reason
        {
            self.record(SafeEvent::AutomaticEligibilityChanged {
                eligible: current.desired_automatic_reason,
                suppression: suppression_name(current.suppression_reason),
            });
        }
    }

    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }
}

fn suppression_name(suppression: Option<AutomaticProtectionSuppression>) -> &'static str {
    match suppression {
        None => "none",
        Some(AutomaticProtectionSuppression::AutomaticDisabled) => "automatic-disabled",
        Some(AutomaticProtectionSuppression::LimitedPower) => "limited-power",
        Some(AutomaticProtectionSuppression::UnknownPower) => "unknown-power",
    }
}

impl AutomaticIntentConsumer for AutomaticProtectionController {
    fn set_automatic_intent(&self, active: bool) -> Result<(), AutomaticIntentError> {
        let snapshot = self.set_detector_intent(active);
        match snapshot.latest_coordination_failure {
            Some(failure) => Err(AutomaticIntentError(failure)),
            None => Ok(()),
        }
    }
}

impl AutomaticProtectionState {
    fn desired_automatic_reason(&self) -> bool {
        self.detector_intent
            && self.settings.automatic_download_protection_enabled
            && (self.settings.protect_on_battery || self.power_source == PowerSource::External)
    }

    fn suppression_reason(&self) -> Option<AutomaticProtectionSuppression> {
        if !self.detector_intent || self.desired_automatic_reason() {
            return None;
        }
        if !self.settings.automatic_download_protection_enabled {
            Some(AutomaticProtectionSuppression::AutomaticDisabled)
        } else {
            match self.power_source {
                PowerSource::Limited => Some(AutomaticProtectionSuppression::LimitedPower),
                PowerSource::Unknown => Some(AutomaticProtectionSuppression::UnknownPower),
                PowerSource::External => None,
            }
        }
    }

    fn snapshot(&self) -> AutomaticProtectionSnapshot {
        AutomaticProtectionSnapshot {
            detector_intent: self.detector_intent,
            settings: self.settings,
            power_source: self.power_source,
            desired_automatic_reason: self.desired_automatic_reason(),
            suppression_reason: self.suppression_reason(),
            latest_coordination_failure: self.latest_coordination_failure.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_log::LoggingFailureStage;
    use crate::wake_coordinator::{ProtectionState, SleepInhibitor, WakeCoordinator};
    use std::{error::Error, fs, path::PathBuf, time::SystemTime};

    struct FailsFirstAcquire {
        active: bool,
        failed_once: bool,
    }

    impl SleepInhibitor for FailsFirstAcquire {
        fn state(&self) -> ProtectionState {
            if self.active {
                ProtectionState::Active
            } else {
                ProtectionState::Inactive
            }
        }

        fn acquire(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
            if self.failed_once {
                self.active = true;
                Ok(())
            } else {
                self.failed_once = true;
                Err("scripted acquire failure".into())
            }
        }

        fn release(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
            Ok(())
        }
    }

    struct RecordingInhibitor {
        active: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl SleepInhibitor for RecordingInhibitor {
        fn state(&self) -> ProtectionState {
            if self.active {
                ProtectionState::Active
            } else {
                ProtectionState::Inactive
            }
        }

        fn acquire(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
            self.calls.lock().unwrap().push("acquire");
            self.active = true;
            Ok(())
        }

        fn release(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
            self.calls.lock().unwrap().push("release");
            self.active = false;
            Ok(())
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "chiu-automatic-protection-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unavailable_log() -> LocalLog {
        LocalLog::unavailable(LoggingFailureStage::Open)
    }

    fn wake_coordinator() -> WakeCoordinator {
        WakeCoordinator::new(unavailable_log())
    }

    fn automatic_controller(
        settings: AutomaticProtectionSettings,
        coordinator: WakeCoordinator,
    ) -> AutomaticProtectionController {
        AutomaticProtectionController::new(settings, coordinator, unavailable_log())
    }

    #[test]
    fn qualified_intent_is_suppressed_while_default_power_is_unknown() {
        let coordinator = wake_coordinator();
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            coordinator.clone(),
        );

        let snapshot = controller.set_detector_intent(true);

        assert!(snapshot.detector_intent());
        assert!(!snapshot.desired_automatic_reason());
        assert_eq!(
            snapshot.suppression_reason(),
            Some(AutomaticProtectionSuppression::UnknownPower)
        );
        assert!(coordinator.snapshot().active_reasons().is_empty());
    }

    #[test]
    fn external_power_acquires_qualified_automatic_intent_without_requalification() {
        let coordinator = wake_coordinator();
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            coordinator.clone(),
        );
        controller.set_detector_intent(true);

        let snapshot = controller.set_power_source(PowerSource::External);

        assert!(snapshot.desired_automatic_reason());
        assert_eq!(snapshot.suppression_reason(), None);
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::AutomaticDownload]
        );
    }

    #[test]
    fn allowing_battery_reconciles_qualified_unknown_power_immediately() {
        let coordinator = wake_coordinator();
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            coordinator.clone(),
        );
        controller.set_detector_intent(true);

        let snapshot = controller.set_settings(AutomaticProtectionSettings::new(true, true));

        assert!(snapshot.desired_automatic_reason());
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::AutomaticDownload]
        );
    }

    #[test]
    fn failed_acquisition_preserves_desired_intent_and_retries_on_a_later_event() {
        let coordinator = WakeCoordinator::with_inhibitor(
            FailsFirstAcquire {
                active: false,
                failed_once: false,
            },
            unavailable_log(),
        );
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            coordinator.clone(),
        );
        controller.set_power_source(PowerSource::External);

        let failed = controller.set_detector_intent(true);

        assert!(failed.desired_automatic_reason());
        assert_eq!(
            failed.latest_coordination_failure(),
            Some("scripted acquire failure")
        );
        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::AutomaticDownload]
        );

        let recovered = controller.set_detector_intent(true);
        assert_eq!(recovered.latest_coordination_failure(), None);
        assert_eq!(
            coordinator.snapshot().protection_state(),
            ProtectionState::Active
        );
    }

    #[test]
    fn policy_matrix_matches_the_persisted_settings_and_power_contract() {
        for detector_intent in [false, true] {
            for enabled in [false, true] {
                for protect_on_battery in [false, true] {
                    for power_source in [
                        PowerSource::External,
                        PowerSource::Limited,
                        PowerSource::Unknown,
                    ] {
                        let controller = automatic_controller(
                            AutomaticProtectionSettings::new(enabled, protect_on_battery),
                            wake_coordinator(),
                        );
                        controller.set_power_source(power_source);

                        let snapshot = controller.set_detector_intent(detector_intent);

                        assert_eq!(
                            snapshot.desired_automatic_reason(),
                            detector_intent
                                && enabled
                                && (protect_on_battery || power_source == PowerSource::External),
                            "intent={detector_intent}, enabled={enabled}, battery={protect_on_battery}, source={power_source:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn suppression_precedence_is_stable_and_unqualified_intent_is_not_suppressed() {
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(false, false),
            wake_coordinator(),
        );
        controller.set_power_source(PowerSource::Limited);
        assert_eq!(controller.snapshot().suppression_reason(), None);

        let disabled = controller.set_detector_intent(true);
        assert_eq!(
            disabled.suppression_reason(),
            Some(AutomaticProtectionSuppression::AutomaticDisabled)
        );

        let limited = controller.set_settings(AutomaticProtectionSettings::new(true, false));
        assert_eq!(
            limited.suppression_reason(),
            Some(AutomaticProtectionSuppression::LimitedPower)
        );

        let unknown = controller.set_power_source(PowerSource::Unknown);
        assert_eq!(
            unknown.suppression_reason(),
            Some(AutomaticProtectionSuppression::UnknownPower)
        );
    }

    #[test]
    fn relevant_transitions_reconcile_without_duplicate_native_commands() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let coordinator = WakeCoordinator::with_inhibitor(
            RecordingInhibitor {
                active: false,
                calls: calls.clone(),
            },
            unavailable_log(),
        );
        let controller =
            automatic_controller(AutomaticProtectionSettings::new(true, false), coordinator);
        controller.set_detector_intent(true);
        controller.set_power_source(PowerSource::External);
        controller.set_power_source(PowerSource::External);
        controller.set_power_source(PowerSource::Limited);
        controller.set_power_source(PowerSource::Unknown);
        controller.set_settings(AutomaticProtectionSettings::new(true, true));
        controller.set_settings(AutomaticProtectionSettings::new(true, true));
        controller.set_settings(AutomaticProtectionSettings::new(false, true));

        assert_eq!(
            *calls.lock().unwrap(),
            ["acquire", "release", "acquire", "release"]
        );
    }

    #[test]
    fn suppressing_automatic_intent_preserves_overlapping_manual_protection() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let coordinator = WakeCoordinator::with_inhibitor(
            RecordingInhibitor {
                active: false,
                calls: calls.clone(),
            },
            unavailable_log(),
        );
        coordinator
            .set_reason_active(WakeReason::ManualKeepAwake, true)
            .unwrap();
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            coordinator.clone(),
        );
        controller.set_power_source(PowerSource::External);
        controller.set_detector_intent(true);

        controller.set_power_source(PowerSource::Limited);

        assert_eq!(
            coordinator.snapshot().active_reasons(),
            &[WakeReason::ManualKeepAwake]
        );
        assert_eq!(*calls.lock().unwrap(), ["acquire"]);
    }

    #[test]
    fn settings_facade_persists_before_applying_the_live_policy() {
        let directory = TestDirectory::new("persist-first");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        let controller = automatic_controller(
            AutomaticProtectionSettings::from_settings(&settings.snapshot()),
            wake_coordinator(),
        );
        let service = AutomaticProtectionService::new(settings.clone(), controller.clone());

        let snapshot = service.set_protect_on_battery(true).unwrap();

        assert!(settings.snapshot().protect_on_battery());
        assert!(snapshot.settings().protect_on_battery());
        assert!(controller.snapshot().settings().protect_on_battery());
    }

    #[test]
    fn toggle_uses_effective_policy_state_instead_of_menu_state() {
        let directory = TestDirectory::new("toggle-effective");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        let controller = automatic_controller(
            AutomaticProtectionSettings::from_settings(&settings.snapshot()),
            wake_coordinator(),
        );
        let service = AutomaticProtectionService::new(settings.clone(), controller.clone());

        let snapshot = service.toggle_automatic_download_protection().unwrap();

        assert!(!snapshot.settings().automatic_download_protection_enabled());
        assert!(!settings.snapshot().automatic_download_protection_enabled());
    }

    #[test]
    fn failed_toggle_persistence_does_not_change_effective_policy() {
        let settings = SettingsService::unavailable(unavailable_log());
        let controller = automatic_controller(
            AutomaticProtectionSettings::new(true, false),
            wake_coordinator(),
        );
        let service = AutomaticProtectionService::new(settings, controller.clone());

        assert!(service.toggle_automatic_download_protection().is_err());

        assert!(
            controller
                .snapshot()
                .settings()
                .automatic_download_protection_enabled()
        );
    }

    #[test]
    fn persistence_failure_does_not_change_the_live_policy() {
        let settings = SettingsService::unavailable(unavailable_log());
        let controller = automatic_controller(
            AutomaticProtectionSettings::from_settings(&settings.snapshot()),
            wake_coordinator(),
        );
        let service = AutomaticProtectionService::new(settings, controller.clone());

        assert!(
            service
                .set_automatic_download_protection_enabled(false)
                .is_err()
        );

        assert!(
            controller
                .snapshot()
                .settings()
                .automatic_download_protection_enabled()
        );
    }

    #[test]
    fn wake_failure_does_not_roll_back_a_persisted_policy_change() {
        let directory = TestDirectory::new("persisted-wake-failure");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        settings
            .update(SettingsUpdate::AutomaticDownloadProtectionEnabled(false))
            .unwrap();
        let coordinator = WakeCoordinator::with_inhibitor(
            FailsFirstAcquire {
                active: false,
                failed_once: false,
            },
            unavailable_log(),
        );
        let controller = automatic_controller(
            AutomaticProtectionSettings::from_settings(&settings.snapshot()),
            coordinator,
        );
        controller.set_power_source(PowerSource::External);
        controller.set_detector_intent(true);
        let service = AutomaticProtectionService::new(settings.clone(), controller.clone());

        let snapshot = service
            .set_automatic_download_protection_enabled(true)
            .unwrap();

        assert!(settings.snapshot().automatic_download_protection_enabled());
        assert!(snapshot.desired_automatic_reason());
        assert_eq!(
            snapshot.latest_coordination_failure(),
            Some("scripted acquire failure")
        );

        let recovered = service.set_protect_on_battery(false).unwrap();
        assert_eq!(recovered.latest_coordination_failure(), None);
    }

    #[test]
    fn concurrent_setting_operations_leave_persistence_and_policy_aligned() {
        let directory = TestDirectory::new("serialized-updates");
        let settings = SettingsService::load(directory.0.clone(), unavailable_log());
        let controller = automatic_controller(
            AutomaticProtectionSettings::from_settings(&settings.snapshot()),
            wake_coordinator(),
        );
        let service = AutomaticProtectionService::new(settings.clone(), controller.clone());
        let left = service.clone();
        let right = service;

        let left = std::thread::spawn(move || {
            for enabled in [false, true, false, true] {
                left.set_automatic_download_protection_enabled(enabled)
                    .unwrap();
            }
        });
        let right = std::thread::spawn(move || {
            for enabled in [true, false, true, false] {
                right.set_protect_on_battery(enabled).unwrap();
            }
        });
        left.join().unwrap();
        right.join().unwrap();

        assert_eq!(
            controller.snapshot().settings(),
            AutomaticProtectionSettings::from_settings(&settings.snapshot())
        );
    }
}
