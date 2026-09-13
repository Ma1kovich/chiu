use crate::{
    application::{ApplicationState, ApplicationStatus},
    automatic_download::{
        AutomaticDownloadMonitor, AutomaticDownloadSettings, AutomaticDownloadState,
        AutomaticDownloadTuningService,
    },
    automatic_protection::{
        AutomaticProtectionController, AutomaticProtectionService, AutomaticProtectionSettings,
        AutomaticProtectionSuppression, PowerSource,
    },
    diagnostics::{DiagnosticsAvailability, DiagnosticsService},
    launch_at_login::{LaunchAtLoginFailureStage, LaunchAtLoginService, Registration},
    manual_session::{ManualSession, ManualSessionState},
    network_activity::{NetworkActivitySource, NetworkActivitySourceStatus},
    power_source::{PowerSourceObserver, PowerSourceObserverHealth},
    settings::{LoadCondition, PersistenceCondition, SettingsService},
    updater::{
        UpdateAvailability, UpdateFailureStage, UpdateStatus, UpdaterService, UpdaterSnapshot,
    },
    wake_coordinator::{ProtectionState, WakeCoordinator, WakeReason},
};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

mod command;

pub(crate) use command::{GracePreset, ThresholdPreset, TrayCommand};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsAlert {
    None,
    Recovered,
    SaveFailed,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrayInput {
    pub(crate) application_status: ApplicationStatus,
    pub(crate) manual_state: ManualSessionState,
    pub(crate) manual_failure: bool,
    pub(crate) detector_state: AutomaticDownloadState,
    pub(crate) receive_rate: Option<u128>,
    pub(crate) tuning: AutomaticDownloadSettings,
    pub(crate) automatic_settings: AutomaticProtectionSettings,
    pub(crate) detector_intent: bool,
    pub(crate) desired_automatic_reason: bool,
    pub(crate) suppression: Option<AutomaticProtectionSuppression>,
    pub(crate) automatic_failure: bool,
    pub(crate) network_status: NetworkActivitySourceStatus,
    pub(crate) network_failure: bool,
    pub(crate) power_source: PowerSource,
    pub(crate) power_health: PowerSourceObserverHealth,
    pub(crate) power_failure: bool,
    pub(crate) settings_alert: SettingsAlert,
    pub(crate) launch_at_login_available: bool,
    pub(crate) launch_at_login_desired: bool,
    pub(crate) launch_at_login_observed: Registration,
    pub(crate) launch_at_login_failure: Option<LaunchAtLoginFailureStage>,
    pub(crate) updater: UpdaterSnapshot,
    pub(crate) wake_reasons: Vec<WakeReason>,
    pub(crate) protection: ProtectionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KeepAwakeView {
    Inactive,
    Finite { remaining: String },
    UntilDisabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChoiceView {
    pub(crate) checked: [bool; 3],
    pub(crate) custom: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DetectionView {
    pub(crate) state: String,
    pub(crate) receive_rate: Option<String>,
    pub(crate) power_source: String,
    pub(crate) threshold: ChoiceView,
    pub(crate) grace: ChoiceView,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchAtLoginView {
    pub(crate) label: String,
    pub(crate) enabled: bool,
    pub(crate) checked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdaterView {
    pub(crate) automatic_label: String,
    pub(crate) automatic_enabled: bool,
    pub(crate) automatic_checked: bool,
    pub(crate) check_label: String,
    pub(crate) check_enabled: bool,
    pub(crate) result: Option<String>,
    pub(crate) install: Option<(String, bool)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticsView {
    pub(crate) copy_label: String,
    pub(crate) copy_enabled: bool,
    pub(crate) open_logs_label: String,
    pub(crate) open_logs_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrayView {
    pub(crate) primary: String,
    pub(crate) detail: Option<String>,
    pub(crate) warning: Option<String>,
    pub(crate) commands_enabled: bool,
    pub(crate) automatic_checked: bool,
    pub(crate) protect_on_battery_checked: bool,
    pub(crate) keep_awake: KeepAwakeView,
    pub(crate) detection: DetectionView,
    pub(crate) launch_at_login: LaunchAtLoginView,
    pub(crate) updater: UpdaterView,
    pub(crate) diagnostics: DiagnosticsView,
}

#[derive(Clone)]
pub(crate) struct TrayApplication {
    application: ApplicationState,
    diagnostics: DiagnosticsService,
    product: Arc<OnceLock<TrayProduct>>,
}

#[derive(Clone)]
pub(crate) struct TrayProduct {
    manual: ManualSession,
    detector: AutomaticDownloadMonitor,
    tuning: AutomaticDownloadTuningService,
    automatic: AutomaticProtectionController,
    automatic_settings: AutomaticProtectionService,
    network: NetworkActivitySource,
    power: PowerSourceObserver,
    launch_at_login: LaunchAtLoginService,
    updater: UpdaterService,
    settings: SettingsService,
    wake: WakeCoordinator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayCommandOutcome {
    RefreshRequested,
    QuitRequested,
    CopyDiagnosticsFailed,
    Ignored,
}

impl TrayApplication {
    pub(crate) fn new(application: ApplicationState, diagnostics: DiagnosticsService) -> Self {
        Self {
            application,
            diagnostics,
            product: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn activate(&self, product: TrayProduct) {
        self.product
            .set(product)
            .unwrap_or_else(|_| panic!("tray product facade initialized more than once"));
        self.product
            .get()
            .expect("tray product facade initialized")
            .updater
            .activate();
    }

    pub(crate) fn view(&self) -> TrayView {
        let status = self.application.snapshot().status();
        let mut view = self.product.get().map_or_else(
            || lifecycle_only_view(status),
            |product| product.view(status),
        );
        view.diagnostics = diagnostics_view(self.diagnostics.availability());
        view
    }

    pub(crate) fn execute(&self, command: TrayCommand) -> TrayCommandOutcome {
        if command == TrayCommand::Quit {
            return TrayCommandOutcome::QuitRequested;
        }
        match command {
            TrayCommand::CopyDiagnostics => {
                return match self.diagnostics.copy() {
                    Ok(_) => TrayCommandOutcome::RefreshRequested,
                    Err(crate::diagnostics::DiagnosticActionFailure::Unavailable) => {
                        TrayCommandOutcome::Ignored
                    }
                    Err(_) => TrayCommandOutcome::CopyDiagnosticsFailed,
                };
            }
            TrayCommand::OpenLogs => {
                let _ = self.diagnostics.open_logs();
                return TrayCommandOutcome::RefreshRequested;
            }
            _ => {}
        }
        if !matches!(
            self.application.snapshot().status(),
            ApplicationStatus::Ready | ApplicationStatus::Degraded
        ) {
            return TrayCommandOutcome::Ignored;
        }
        let Some(product) = self.product.get() else {
            return TrayCommandOutcome::Ignored;
        };
        product.execute(command)
    }
}

impl TrayProduct {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        manual: ManualSession,
        detector: AutomaticDownloadMonitor,
        tuning: AutomaticDownloadTuningService,
        automatic: AutomaticProtectionController,
        automatic_settings: AutomaticProtectionService,
        network: NetworkActivitySource,
        power: PowerSourceObserver,
        launch_at_login: LaunchAtLoginService,
        updater: UpdaterService,
        settings: SettingsService,
        wake: WakeCoordinator,
    ) -> Self {
        Self {
            manual,
            detector,
            tuning,
            automatic,
            automatic_settings,
            network,
            power,
            launch_at_login,
            updater,
            settings,
            wake,
        }
    }

    pub(crate) fn settings_health(&self) -> crate::settings::SettingsHealth {
        self.settings.health()
    }

    fn view(&self, application_status: ApplicationStatus) -> TrayView {
        project(&self.input(application_status))
    }

    fn input(&self, application_status: ApplicationStatus) -> TrayInput {
        let manual = self.manual.snapshot();
        let detector = self.detector.snapshot();
        let automatic = self.automatic.snapshot();
        let network = self.network.snapshot();
        let power = self.power.snapshot();
        let launch_at_login = self.launch_at_login.snapshot();
        let updater = self.updater.snapshot();
        let settings = self.settings.health();
        // Coordinator truth is sampled last so transient cross-service states fail safe.
        let wake = self.wake.snapshot();
        TrayInput {
            application_status,
            manual_state: manual.state,
            manual_failure: manual.background_failure.is_some(),
            detector_state: detector.state,
            receive_rate: detector.latest_receive_rate_bytes_per_second,
            tuning: detector.settings,
            automatic_settings: automatic.settings(),
            detector_intent: automatic.detector_intent(),
            desired_automatic_reason: automatic.desired_automatic_reason(),
            suppression: automatic.suppression_reason(),
            automatic_failure: automatic.latest_coordination_failure().is_some(),
            network_status: network.status(),
            network_failure: network.latest_failure().is_some(),
            power_source: automatic.power_source(),
            power_health: power.health(),
            power_failure: power.latest_failure().is_some(),
            settings_alert: settings_alert(&settings),
            launch_at_login_available: launch_at_login.available(),
            launch_at_login_desired: launch_at_login.desired(),
            launch_at_login_observed: launch_at_login.observed(),
            launch_at_login_failure: launch_at_login
                .latest_failure()
                .map(|failure| failure.stage()),
            updater,
            wake_reasons: wake.active_reasons().to_vec(),
            protection: wake.protection_state(),
        }
    }

    fn execute(&self, command: TrayCommand) -> TrayCommandOutcome {
        match command {
            TrayCommand::ToggleAutomaticProtection => {
                let _ = self
                    .automatic_settings
                    .toggle_automatic_download_protection();
            }
            TrayCommand::ToggleProtectOnBattery => {
                let _ = self.automatic_settings.toggle_protect_on_battery();
            }
            TrayCommand::ToggleLaunchAtLogin => {
                let _ = self.launch_at_login.toggle();
            }
            TrayCommand::ToggleAutomaticUpdates => self.updater.toggle_automatic_checks(),
            TrayCommand::CheckUpdates => self.updater.check_manually(),
            TrayCommand::InstallUpdate => self.updater.request_install(),
            TrayCommand::Manual(command) => {
                let _ = self.manual.command(command);
            }
            TrayCommand::SetThreshold(threshold) => {
                let _ = self.tuning.set_threshold(threshold.bytes_per_second());
            }
            TrayCommand::SetGrace(grace) => {
                let _ = self.tuning.set_grace(grace.duration());
            }
            TrayCommand::Quit => unreachable!("quit is handled before product command gating"),
            TrayCommand::CopyDiagnostics | TrayCommand::OpenLogs => {
                unreachable!("diagnostics commands are handled before product command gating")
            }
        }
        TrayCommandOutcome::RefreshRequested
    }
}

fn lifecycle_only_view(status: ApplicationStatus) -> TrayView {
    let primary = match status {
        ApplicationStatus::Failed => "Startup failed",
        ApplicationStatus::ShuttingDown | ApplicationStatus::Stopped => "Shutting down…",
        ApplicationStatus::Starting | ApplicationStatus::Ready | ApplicationStatus::Degraded => {
            "Starting…"
        }
    };
    TrayView {
        primary: primary.to_owned(),
        detail: None,
        warning: None,
        commands_enabled: false,
        automatic_checked: false,
        protect_on_battery_checked: false,
        keep_awake: KeepAwakeView::Inactive,
        detection: DetectionView {
            state: "Unavailable".to_owned(),
            receive_rate: None,
            power_source: "Unknown".to_owned(),
            threshold: ChoiceView {
                checked: [false; 3],
                custom: None,
            },
            grace: ChoiceView {
                checked: [false; 3],
                custom: None,
            },
        },
        launch_at_login: LaunchAtLoginView {
            label: "Launch at login — unavailable".to_owned(),
            enabled: false,
            checked: false,
        },
        updater: unavailable_updater_view(false),
        diagnostics: diagnostics_view(DiagnosticsAvailability {
            copy: false,
            open_logs: false,
        }),
    }
}

fn settings_alert(settings: &crate::settings::SettingsHealth) -> SettingsAlert {
    if settings.last_save_failure().is_some() {
        return SettingsAlert::SaveFailed;
    }
    if settings.persistence() != PersistenceCondition::Writable
        || matches!(
            settings.load_condition(),
            LoadCondition::PersistenceUnavailable | LoadCondition::UnsupportedFutureSchema { .. }
        )
    {
        return SettingsAlert::Unavailable;
    }
    if !matches!(
        settings.load_condition(),
        LoadCondition::FirstRun | LoadCondition::CurrentVersion
    ) || !settings.field_recoveries().is_empty()
        || !settings.artifact_warnings().is_empty()
    {
        return SettingsAlert::Recovered;
    }
    SettingsAlert::None
}

pub(crate) fn project(input: &TrayInput) -> TrayView {
    let commands_enabled = matches!(
        input.application_status,
        ApplicationStatus::Ready | ApplicationStatus::Degraded
    );
    let primary = match input.application_status {
        ApplicationStatus::Starting => "Starting…",
        ApplicationStatus::Failed => "Startup failed",
        ApplicationStatus::ShuttingDown | ApplicationStatus::Stopped => "Shutting down…",
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if !input.wake_reasons.is_empty() && input.protection == ProtectionState::Inactive =>
        {
            "Protection failed"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if input.wake_reasons.is_empty() && input.protection == ProtectionState::Active =>
        {
            "Sleep protection could not stop"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if !input.wake_reasons.is_empty() && input.protection == ProtectionState::Active =>
        {
            "Keeping awake"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if input.detector_intent && input.suppression.is_some() =>
        {
            "Network activity detected, not keeping awake"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if input.network_status != NetworkActivitySourceStatus::Available =>
        {
            "Can’t detect network activity"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded
            if !input
                .automatic_settings
                .automatic_download_protection_enabled()
                && !input.wake_reasons.contains(&WakeReason::ManualKeepAwake) =>
        {
            "Keep awake during downloads: Off"
        }
        ApplicationStatus::Ready | ApplicationStatus::Degraded => "Ready to keep awake",
    };
    let launch_at_login = LaunchAtLoginView {
        label: if input.launch_at_login_available {
            "Launch at login"
        } else {
            "Launch at login — unavailable"
        }
        .to_owned(),
        enabled: commands_enabled && input.launch_at_login_available,
        checked: input.launch_at_login_desired,
    };
    let updater = updater_view(&input.updater, commands_enabled);
    TrayView {
        primary: primary.to_owned(),
        detail: reason_detail(input).or_else(|| suppression_detail(input)),
        warning: commands_enabled.then(|| warning(input)).flatten(),
        commands_enabled,
        automatic_checked: input
            .automatic_settings
            .automatic_download_protection_enabled(),
        protect_on_battery_checked: input.automatic_settings.protect_on_battery(),
        keep_awake: keep_awake_view(&input.manual_state),
        detection: detection_view(input),
        launch_at_login,
        updater,
        diagnostics: diagnostics_view(DiagnosticsAvailability {
            copy: false,
            open_logs: false,
        }),
    }
}

fn diagnostics_view(availability: DiagnosticsAvailability) -> DiagnosticsView {
    DiagnosticsView {
        copy_label: if availability.copy {
            "Copy diagnostics"
        } else {
            "Copy diagnostics — unavailable"
        }
        .to_owned(),
        copy_enabled: availability.copy,
        open_logs_label: if availability.open_logs {
            "Open logs"
        } else {
            "Open logs — unavailable"
        }
        .to_owned(),
        open_logs_enabled: availability.open_logs,
    }
}

fn updater_view(snapshot: &UpdaterSnapshot, commands_enabled: bool) -> UpdaterView {
    if snapshot.availability != UpdateAvailability::Available {
        return unavailable_updater_view(snapshot.automatic_checks_enabled);
    }
    let active = matches!(
        snapshot.status,
        UpdateStatus::Checking(_)
            | UpdateStatus::AwaitingConfirmation { .. }
            | UpdateStatus::Downloading { .. }
            | UpdateStatus::Installing { .. }
    );
    let (check_label, result, install) = match &snapshot.status {
        UpdateStatus::Idle => ("Check for updates…".to_owned(), None, None),
        UpdateStatus::Checking(_) => ("Checking for updates…".to_owned(), None, None),
        UpdateStatus::UpToDate => (
            "Check for updates…".to_owned(),
            Some("Up to date".to_owned()),
            None,
        ),
        UpdateStatus::UpdateAvailable { version } => (
            "Check for updates…".to_owned(),
            Some(update_available_label(version.as_deref())),
            Some((install_label(version.as_deref()), commands_enabled)),
        ),
        UpdateStatus::AwaitingConfirmation { .. } => (
            "Check for updates…".to_owned(),
            Some("Waiting for update confirmation…".to_owned()),
            None,
        ),
        UpdateStatus::Downloading { percent, .. } => (
            "Check for updates…".to_owned(),
            Some(percent.map_or_else(
                || "Downloading update…".to_owned(),
                |percent| format!("Downloading update… {percent}%"),
            )),
            None,
        ),
        UpdateStatus::Installing { .. } => (
            "Check for updates…".to_owned(),
            Some("Installing update…".to_owned()),
            None,
        ),
        UpdateStatus::Failed { stage, pending, .. } => (
            "Check for updates…".to_owned(),
            Some(update_failure_label(*stage).to_owned()),
            pending
                .as_ref()
                .map(|pending| (install_label(pending.version()), commands_enabled)),
        ),
    };
    UpdaterView {
        automatic_label: "Automatically check for updates".to_owned(),
        automatic_enabled: commands_enabled,
        automatic_checked: snapshot.automatic_checks_enabled,
        check_label,
        check_enabled: commands_enabled && !active,
        result,
        install,
    }
}

fn unavailable_updater_view(checked: bool) -> UpdaterView {
    UpdaterView {
        automatic_label: "Automatically check for updates — unavailable".to_owned(),
        automatic_enabled: false,
        automatic_checked: checked,
        check_label: "Check for updates… — unavailable".to_owned(),
        check_enabled: false,
        result: None,
        install: None,
    }
}

fn update_available_label(version: Option<&str>) -> String {
    version.map_or_else(
        || "Update available".to_owned(),
        |version| format!("Update available — Chiù {version}"),
    )
}

fn install_label(version: Option<&str>) -> String {
    version.map_or_else(
        || "Install Chiù update…".to_owned(),
        |version| format!("Install Chiù {version}…"),
    )
}

fn update_failure_label(stage: UpdateFailureStage) -> &'static str {
    match stage {
        UpdateFailureStage::Scheduling => "Automatic update scheduling failed",
        UpdateFailureStage::Check => "Update check failed",
        UpdateFailureStage::Download => "Update download failed",
        UpdateFailureStage::Verification => "Update verification failed",
        UpdateFailureStage::Install => "Update installation failed",
        UpdateFailureStage::Restart => "Update restart failed",
    }
}

fn keep_awake_view(state: &ManualSessionState) -> KeepAwakeView {
    match state {
        ManualSessionState::Inactive => KeepAwakeView::Inactive,
        ManualSessionState::Finite { remaining } => KeepAwakeView::Finite {
            remaining: format_remaining(*remaining),
        },
        ManualSessionState::UntilDisabled => KeepAwakeView::UntilDisabled,
    }
}

fn detection_view(input: &TrayInput) -> DetectionView {
    const THRESHOLDS: [ThresholdPreset; 3] = [
        ThresholdPreset::Kibibytes256,
        ThresholdPreset::Mebibyte1,
        ThresholdPreset::Mebibytes5,
    ];
    const GRACE: [GracePreset; 3] = [
        GracePreset::Seconds30,
        GracePreset::Minutes2,
        GracePreset::Minutes5,
    ];
    let threshold = input.tuning.meaningful_receive_rate_bytes_per_second();
    let grace = input.tuning.grace();
    let threshold_checked = THRESHOLDS.map(|preset| threshold == preset.bytes_per_second());
    let grace_checked = GRACE.map(|preset| grace == preset.duration());
    DetectionView {
        state: match input.detector_state {
            AutomaticDownloadState::Idle => "Waiting".to_owned(),
            AutomaticDownloadState::Active => "Active".to_owned(),
            AutomaticDownloadState::Hold { remaining_grace } => {
                format!("Grace — {}", format_remaining(remaining_grace))
            }
        },
        receive_rate: input.receive_rate.map(format_byte_rate),
        power_source: match input.power_source {
            PowerSource::External => "External power",
            PowerSource::Limited => "Battery or limited power",
            PowerSource::Unknown => "Unknown",
        }
        .to_owned(),
        threshold: ChoiceView {
            checked: threshold_checked,
            custom: (!threshold_checked.iter().any(|checked| *checked))
                .then(|| format!("Custom — {}", format_byte_rate(u128::from(threshold)))),
        },
        grace: ChoiceView {
            checked: grace_checked,
            custom: (!grace_checked.iter().any(|checked| *checked))
                .then(|| format!("Custom — {}", format_duration_value(grace))),
        },
    }
}

fn format_byte_rate(bytes_per_second: u128) -> String {
    const KIB: u128 = 1024;
    const MIB: u128 = 1024 * 1024;
    if bytes_per_second >= MIB && bytes_per_second.is_multiple_of(MIB) {
        format!("{} MB/s", bytes_per_second / MIB)
    } else if bytes_per_second >= MIB {
        format!("{:.1} MB/s", bytes_per_second as f64 / MIB as f64)
    } else if bytes_per_second >= KIB && bytes_per_second.is_multiple_of(KIB) {
        format!("{} KB/s", bytes_per_second / KIB)
    } else if bytes_per_second >= KIB {
        format!("{:.1} KB/s", bytes_per_second as f64 / KIB as f64)
    } else {
        format!("{bytes_per_second} B/s")
    }
}

fn format_duration_value(duration: Duration) -> String {
    let milliseconds = duration.as_millis();
    if milliseconds.is_multiple_of(60_000) {
        let minutes = milliseconds / 60_000;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else if milliseconds.is_multiple_of(1_000) {
        let seconds = milliseconds / 1_000;
        format!("{seconds} second{}", if seconds == 1 { "" } else { "s" })
    } else {
        format!("{milliseconds} ms")
    }
}

fn suppression_detail(input: &TrayInput) -> Option<String> {
    if !input.wake_reasons.is_empty() || !input.detector_intent {
        return None;
    }
    input.suppression.map(|reason| {
        match reason {
            AutomaticProtectionSuppression::AutomaticDisabled => "Keep awake during downloads: Off",
            AutomaticProtectionSuppression::LimitedPower => "Paused on battery or limited power",
            AutomaticProtectionSuppression::UnknownPower => "Power source unknown",
        }
        .to_owned()
    })
}

fn warning(input: &TrayInput) -> Option<String> {
    if input.manual_failure || input.automatic_failure || protection_is_inconsistent(input) {
        return Some("Sleep protection needs attention".to_owned());
    }
    match input.settings_alert {
        SettingsAlert::SaveFailed => return Some("Settings could not be saved".to_owned()),
        SettingsAlert::Unavailable => {
            return Some("Settings changes are unavailable".to_owned());
        }
        SettingsAlert::None | SettingsAlert::Recovered => {}
    }
    if let Some(warning) = launch_at_login_warning(input) {
        return Some(warning);
    }
    if let Some(stage) = input.updater.user_failure() {
        return Some(update_failure_label(stage).to_owned());
    }
    if input.network_failure || input.network_status != NetworkActivitySourceStatus::Available {
        return Some("Keep awake during downloads is unavailable".to_owned());
    }
    if input.power_failure || input.power_health != PowerSourceObserverHealth::Observing {
        return Some("Power source monitoring is unavailable".to_owned());
    }
    if let Some(stage) = input.updater.background_failure() {
        return Some(update_failure_label(stage).to_owned());
    }
    (input.settings_alert == SettingsAlert::Recovered)
        .then(|| "Settings were recovered using safe defaults".to_owned())
}

fn launch_at_login_warning(input: &TrayInput) -> Option<String> {
    if !input.launch_at_login_available {
        return Some("Launch at login is unavailable".to_owned());
    }
    match input.launch_at_login_failure {
        Some(LaunchAtLoginFailureStage::Initialize) => {
            Some("Launch at login is unavailable".to_owned())
        }
        Some(LaunchAtLoginFailureStage::Enable) => {
            Some("Launch at login could not be enabled".to_owned())
        }
        Some(LaunchAtLoginFailureStage::Disable) => {
            Some("Launch at login could not be disabled".to_owned())
        }
        Some(LaunchAtLoginFailureStage::Verify)
            if input.launch_at_login_observed == Registration::Unknown =>
        {
            Some("Launch-at-login status could not be verified".to_owned())
        }
        Some(LaunchAtLoginFailureStage::Verify) => Some(
            if input.launch_at_login_desired {
                "Launch at login could not be enabled"
            } else {
                "Launch at login could not be disabled"
            }
            .to_owned(),
        ),
        None if !matches!(
            (
                input.launch_at_login_desired,
                input.launch_at_login_observed
            ),
            (true, Registration::Enabled) | (false, Registration::Disabled)
        ) =>
        {
            Some("Launch-at-login status could not be verified".to_owned())
        }
        None => None,
    }
}

fn protection_is_inconsistent(input: &TrayInput) -> bool {
    let manual_intent = !matches!(input.manual_state, ManualSessionState::Inactive);
    let automatic_intent = input.wake_reasons.contains(&WakeReason::AutomaticDownload);
    let manual_reason = input.wake_reasons.contains(&WakeReason::ManualKeepAwake);
    manual_intent != manual_reason
        || input.desired_automatic_reason != automatic_intent
        || input.detector_intent != !matches!(input.detector_state, AutomaticDownloadState::Idle)
        || (!input.wake_reasons.is_empty() && input.protection == ProtectionState::Inactive)
        || (input.wake_reasons.is_empty() && input.protection == ProtectionState::Active)
}

fn reason_detail(input: &TrayInput) -> Option<String> {
    if input.wake_reasons.is_empty() {
        return (input.protection == ProtectionState::Active)
            .then(|| "No wake reason remains".to_owned());
    }
    let automatic = input
        .wake_reasons
        .contains(&WakeReason::AutomaticDownload)
        .then(|| match input.detector_state {
            AutomaticDownloadState::Hold { remaining_grace } => format!(
                "Recent network activity — {}",
                format_remaining(remaining_grace)
            ),
            AutomaticDownloadState::Active => input.receive_rate.map_or_else(
                || "Network activity".to_owned(),
                |rate| format!("Network activity — {}", format_byte_rate(rate)),
            ),
            AutomaticDownloadState::Idle => "Network activity".to_owned(),
        });
    let manual = input
        .wake_reasons
        .contains(&WakeReason::ManualKeepAwake)
        .then(|| match input.manual_state {
            ManualSessionState::Finite { remaining } => {
                format!("Manual — {}", format_remaining(remaining))
            }
            ManualSessionState::UntilDisabled => "Manual — until disabled".to_owned(),
            ManualSessionState::Inactive => "Manual".to_owned(),
        });
    match (automatic, manual) {
        (Some(automatic), Some(manual)) => Some(format!("Reasons: {automatic}; {manual}")),
        (Some(reason), None) | (None, Some(reason)) => Some(format!("Reason: {reason}")),
        (None, None) => None,
    }
}

fn format_remaining(remaining: Duration) -> String {
    if remaining < Duration::from_secs(60) {
        return "<1 min remaining".to_owned();
    }
    let whole_seconds = remaining.as_secs();
    let minutes = whole_seconds / 60
        + u64::from(!whole_seconds.is_multiple_of(60) || remaining.subsec_nanos() != 0);
    if minutes < 60 {
        return format!("{minutes} min remaining");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    if minutes == 0 {
        format!("{hours} h remaining")
    } else {
        format!("{hours} h {minutes} min remaining")
    }
}

#[cfg(test)]
mod tests;
