use super::{DiagnosticInput, PlatformInfo};
use crate::{
    application::{ApplicationStatus, StartupFailurePoint},
    automatic_download::AutomaticDownloadState,
    automatic_protection::{AutomaticProtectionSuppression, PowerSource},
    launch_at_login::{LaunchAtLoginFailureStage, Registration},
    local_log::{LocalLog, LoggingFailureStage, LoggingHealth},
    manual_session::ManualSessionState,
    network_activity::{
        NetworkActivityFailure, NetworkActivitySourceStatus, NetworkInterfaceContinuity,
    },
    power_source::{PowerSourceObserverHealth, PowerSourceOperation},
    settings::{
        ArtifactWarning, LoadCondition, PersistenceCondition, RecoveryReason, SettingPath,
        SettingsPersistenceFailure,
    },
    updater::{
        PendingUpdateView, UpdateAvailability, UpdateFailureStage, UpdateStatus, UpdateTrigger,
    },
    wake_coordinator::{ProtectionState, WakeReason},
};

pub(super) const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_INTERFACES: usize = 64;
const OMITTED_MARKER: &str = "output_omitted=true\n";

pub(super) fn render(
    input: &DiagnosticInput,
    generated_at: &str,
    platform: &PlatformInfo,
    logging_health: LoggingHealth,
) -> String {
    let mut output = BoundedText::new();
    output.line("Chiù diagnostics");
    output.line(&format!(
        "generated_at_utc={}",
        safe_timestamp(generated_at)
    ));
    output.line(&format!("version={}", env!("CARGO_PKG_VERSION")));
    output.line(&format!(
        "os_family={}",
        safe_external(&platform.family, 32)
    ));
    output.line(&format!(
        "os_version={}",
        safe_external(&platform.version, 128)
    ));
    output.line(&format!(
        "architecture={}",
        safe_external(&platform.architecture, 32)
    ));
    let application = application_lines(&input.application);
    let settings = input.settings.as_ref().map(settings_lines);
    let manual = manual_lines(&input.manual);
    let detector = input.detector.as_ref().map(detector_lines);
    let automatic = input.automatic.as_ref().map(automatic_lines);
    let power = input.power.as_ref().map(power_lines);
    let wake = wake_lines(&input.wake);
    let network = network_lines(&input.network, input.interfaces.len());
    let launch_at_login = input.launch_at_login.as_ref().map(launch_at_login_lines);
    let updater = input.updater.as_ref().map(updater_lines);
    section(&mut output, "application", Some(&application));
    section(&mut output, "settings", settings.as_deref());
    section(&mut output, "manual_keep_awake", Some(&manual));
    section(&mut output, "detector", detector.as_deref());
    section(&mut output, "automatic_protection", automatic.as_deref());
    section(&mut output, "power_source", power.as_deref());
    section(&mut output, "wake_coordination", Some(&wake));
    section(&mut output, "network", Some(&network));
    let mut interfaces = input.interfaces.clone();
    interfaces.sort_by_key(|interface| interface.id);
    for interface in interfaces.iter().take(MAX_INTERFACES) {
        output.line(&format!(
            "interface id={} included={} continuity={} received_bytes={} recent_delta={}",
            interface.id,
            interface.included,
            continuity(interface.continuity),
            interface.received_bytes,
            interface
                .recent_delta
                .map_or_else(|| "unavailable".to_owned(), |delta| delta.to_string())
        ));
    }
    if interfaces.len() > MAX_INTERFACES {
        output.line(&format!(
            "interfaces_omitted={}",
            interfaces.len() - MAX_INTERFACES
        ));
    }
    section(&mut output, "launch_at_login", launch_at_login.as_deref());
    section(&mut output, "updater", updater.as_deref());
    output.line("[logging]");
    output.line(&format!("health={}", logging_health_name(logging_health)));
    let retention = LocalLog::retention_policy();
    output.line(&format!("maximum_files={}", retention.maximum_files));
    output.line(&format!(
        "maximum_file_bytes={}",
        retention.maximum_file_bytes
    ));
    output.finish()
}

fn application_lines(facts: &super::ApplicationFacts) -> Vec<String> {
    vec![
        format!("status={}", application_status(facts.status)),
        format!(
            "unavailable_capabilities={}",
            joined_capabilities(&facts.unavailable_capabilities)
        ),
        format!(
            "startup_failure={}",
            facts
                .startup_failure
                .map(startup_failure)
                .unwrap_or_else(|| "none".to_owned())
        ),
        format!(
            "cleanup_failures={}",
            joined_capabilities(&facts.cleanup_failures)
        ),
    ]
}

fn settings_lines(facts: &super::SettingsFacts) -> Vec<String> {
    let mut lines = vec![
        format!(
            "automatic_download_protection={}",
            facts.automatic_download_protection
        ),
        format!("protect_on_battery={}", facts.protect_on_battery),
        format!("launch_at_login={}", facts.launch_at_login),
        format!("automatic_update_checks={}", facts.automatic_update_checks),
        format!(
            "threshold_bytes_per_second={}",
            facts.threshold_bytes_per_second
        ),
        format!(
            "qualification_milliseconds={}",
            facts.qualification.as_millis()
        ),
        format!("grace_milliseconds={}", facts.grace.as_millis()),
        format!("load_condition={}", load_condition(facts.load_condition)),
        format!("persistence={}", persistence(facts.persistence)),
    ];
    lines.extend(facts.field_recoveries.iter().map(|recovery| {
        format!(
            "field_recovery={} reason={}",
            setting_path(recovery.path()),
            recovery_reason(recovery.reason())
        )
    }));
    lines.extend(
        facts
            .artifact_warnings
            .iter()
            .map(|warning| format!("artifact_warning={}", artifact_warning(*warning))),
    );
    if let Some(failure) = facts.last_save_failure {
        lines.push(format!("last_save_failure={}", settings_failure(failure)));
    }
    lines
}

fn manual_lines(facts: &super::ManualFacts) -> Vec<String> {
    vec![
        format!("state={}", manual_state(&facts.state)),
        format!(
            "background_failure={}",
            if facts.background_failure {
                "wake-coordination"
            } else {
                "none"
            }
        ),
    ]
}

fn detector_lines(facts: &super::DetectorFacts) -> Vec<String> {
    vec![
        format!("state={}", detector_state(&facts.state)),
        facts.latest_receive_rate_bytes_per_second.map_or_else(
            || "aggregate_receive_bytes_per_second=unavailable".to_owned(),
            |rate| format!("aggregate_receive_bytes_per_second={rate}"),
        ),
    ]
}

fn automatic_lines(facts: &super::AutomaticFacts) -> Vec<String> {
    vec![
        format!("preference_enabled={}", facts.preference_enabled),
        format!("protect_on_battery={}", facts.protect_on_battery),
        format!("detector_intent={}", facts.detector_intent),
        format!(
            "desired_automatic_reason={}",
            facts.desired_automatic_reason
        ),
        format!(
            "suppression={}",
            facts.suppression.map(suppression).unwrap_or("none")
        ),
        format!(
            "coordination_failure={}",
            if facts.coordination_failure {
                "wake-coordination"
            } else {
                "none"
            }
        ),
    ]
}

fn power_lines(facts: &super::PowerFacts) -> Vec<String> {
    let mut lines = vec![
        format!("source={}", power_source(facts.source)),
        format!("health={}", power_health(facts.health)),
    ];
    if let Some(failure) = facts.failure {
        lines.push(format!(
            "failure_operation={}",
            power_operation(failure.operation)
        ));
        if let Some(code) = failure.code {
            lines.push(format!("failure_code={code}"));
        }
    }
    lines
}

fn wake_lines(facts: &super::WakeFacts) -> Vec<String> {
    vec![
        format!("active_reasons={}", wake_reasons(&facts.active_reasons)),
        format!("native_protection={}", protection(facts.protection)),
    ]
}

fn network_lines(facts: &super::NetworkFacts, interface_count: usize) -> Vec<String> {
    vec![
        format!("status={}", network_status(facts.status)),
        format!(
            "failure={}",
            facts.failure.map(network_failure).unwrap_or("none")
        ),
        format!("interface_count={interface_count}"),
    ]
}

fn launch_at_login_lines(facts: &super::LaunchAtLoginFacts) -> Vec<String> {
    vec![
        format!("available={}", facts.available),
        format!("desired={}", facts.desired),
        format!("observed={}", registration(facts.observed)),
        format!(
            "failure_stage={}",
            facts.failure_stage.map(launch_stage).unwrap_or("none")
        ),
    ]
}

fn updater_lines(facts: &super::UpdaterFacts) -> Vec<String> {
    let mut lines = vec![
        format!("availability={}", update_availability(facts.availability)),
        format!("automatic_checks={}", facts.automatic_checks),
    ];
    lines.extend(update_status(&facts.status));
    lines
}

fn section(output: &mut BoundedText, name: &str, lines: Option<&[String]>) {
    output.line(&format!("[{name}]"));
    match lines {
        Some(lines) => {
            for line in lines {
                output.line(line);
            }
        }
        None => output.line("status=not-initialized"),
    }
}

struct BoundedText {
    text: String,
    omitted: bool,
}

impl BoundedText {
    fn new() -> Self {
        Self {
            text: String::new(),
            omitted: false,
        }
    }

    fn line(&mut self, line: &str) {
        if self.omitted {
            return;
        }
        let required = line.len().saturating_add(1);
        if self
            .text
            .len()
            .saturating_add(required)
            .saturating_add(OMITTED_MARKER.len())
            > MAX_DIAGNOSTIC_BYTES
        {
            self.omitted = true;
            return;
        }
        self.text.push_str(line);
        self.text.push('\n');
    }

    fn finish(mut self) -> String {
        if self.omitted {
            self.text.push_str(OMITTED_MARKER);
        }
        self.text
    }
}

fn safe_external(value: &str, maximum_bytes: usize) -> String {
    if value.is_empty()
        || value.len() > maximum_bytes
        || looks_like_ipv4(value)
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '-' | '_' | '+' | '(' | ')')
        })
    {
        "unavailable".to_owned()
    } else {
        value.to_owned()
    }
}

fn safe_timestamp(value: &str) -> String {
    if value.is_empty()
        || value.len() > 64
        || !value.chars().all(|character| {
            character.is_ascii_digit() || matches!(character, 'T' | 'Z' | ':' | '.' | '-' | '+')
        })
    {
        "unavailable".to_owned()
    } else {
        value.to_owned()
    }
}

fn looks_like_ipv4(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 3
                && part.chars().all(|character| character.is_ascii_digit())
                && part.parse::<u8>().is_ok()
        })
}

fn application_status(status: ApplicationStatus) -> &'static str {
    match status {
        ApplicationStatus::Starting => "starting",
        ApplicationStatus::Ready => "ready",
        ApplicationStatus::Degraded => "degraded",
        ApplicationStatus::Failed => "failed",
        ApplicationStatus::ShuttingDown => "shutting-down",
        ApplicationStatus::Stopped => "stopped",
    }
}

fn startup_failure(failure: StartupFailurePoint) -> String {
    match failure {
        StartupFailurePoint::Settings => "settings".to_owned(),
        StartupFailurePoint::Required(capability) => {
            format!("required-capability:{}", capability.as_str())
        }
    }
}

fn joined_capabilities(capabilities: &[crate::application::CapabilityId]) -> String {
    if capabilities.is_empty() {
        "none".to_owned()
    } else {
        capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn manual_state(state: &ManualSessionState) -> String {
    match state {
        ManualSessionState::Inactive => "inactive".to_owned(),
        ManualSessionState::Finite { remaining } => {
            format!("finite remaining_seconds={}", remaining.as_secs())
        }
        ManualSessionState::UntilDisabled => "until-disabled".to_owned(),
    }
}

fn detector_state(state: &AutomaticDownloadState) -> String {
    match state {
        AutomaticDownloadState::Idle => "idle".to_owned(),
        AutomaticDownloadState::Active => "active".to_owned(),
        AutomaticDownloadState::Hold { remaining_grace } => {
            format!(
                "hold remaining_milliseconds={}",
                remaining_grace.as_millis()
            )
        }
    }
}

fn power_source(source: PowerSource) -> &'static str {
    match source {
        PowerSource::External => "external",
        PowerSource::Limited => "limited",
        PowerSource::Unknown => "unknown",
    }
}

fn suppression(value: AutomaticProtectionSuppression) -> &'static str {
    match value {
        AutomaticProtectionSuppression::AutomaticDisabled => "automatic-disabled",
        AutomaticProtectionSuppression::LimitedPower => "limited-power",
        AutomaticProtectionSuppression::UnknownPower => "unknown-power",
    }
}

fn power_health(health: PowerSourceObserverHealth) -> &'static str {
    match health {
        PowerSourceObserverHealth::Starting => "starting",
        PowerSourceObserverHealth::Observing => "observing",
        PowerSourceObserverHealth::Degraded => "degraded",
        PowerSourceObserverHealth::Stopped => "stopped",
    }
}

fn power_operation(operation: PowerSourceOperation) -> &'static str {
    match operation {
        PowerSourceOperation::Registration => "registration",
        PowerSourceOperation::Sample => "sample",
        PowerSourceOperation::Unregistration => "unregistration",
    }
}

fn network_status(status: NetworkActivitySourceStatus) -> &'static str {
    match status {
        NetworkActivitySourceStatus::NotStarted => "not-started",
        NetworkActivitySourceStatus::Starting => "starting",
        NetworkActivitySourceStatus::Available => "available",
        NetworkActivitySourceStatus::Unavailable => "unavailable",
        NetworkActivitySourceStatus::Stopped => "stopped",
    }
}

fn network_failure(failure: NetworkActivityFailure) -> &'static str {
    match failure {
        NetworkActivityFailure::ProviderRead => "provider-read",
    }
}

fn continuity(value: NetworkInterfaceContinuity) -> &'static str {
    match value {
        NetworkInterfaceContinuity::New => "new",
        NetworkInterfaceContinuity::Continuous => "continuous",
        NetworkInterfaceContinuity::CounterReset => "counter-reset",
        NetworkInterfaceContinuity::Removed => "removed",
    }
}

fn registration(value: Registration) -> &'static str {
    match value {
        Registration::Enabled => "enabled",
        Registration::Disabled => "disabled",
        Registration::Unknown => "unknown",
    }
}

fn launch_stage(stage: LaunchAtLoginFailureStage) -> &'static str {
    match stage {
        LaunchAtLoginFailureStage::Initialize => "initialize",
        LaunchAtLoginFailureStage::Enable => "enable",
        LaunchAtLoginFailureStage::Disable => "disable",
        LaunchAtLoginFailureStage::Verify => "verify",
    }
}

fn update_availability(value: UpdateAvailability) -> &'static str {
    match value {
        UpdateAvailability::Available => "available",
        UpdateAvailability::Unconfigured => "unconfigured",
        UpdateAvailability::InitializationFailed => "initialization-failed",
    }
}

fn update_status(status: &UpdateStatus) -> Vec<String> {
    match status {
        UpdateStatus::Idle => vec!["status=idle".to_owned()],
        UpdateStatus::Checking(trigger) => vec![
            "status=checking".to_owned(),
            format!("trigger={}", update_trigger(*trigger)),
        ],
        UpdateStatus::UpToDate => vec!["status=up-to-date".to_owned()],
        UpdateStatus::UpdateAvailable { version } => vec![
            "status=update-available".to_owned(),
            format!("target_version={}", safe_version(version.as_deref())),
        ],
        UpdateStatus::AwaitingConfirmation { version } => vec![
            "status=awaiting-confirmation".to_owned(),
            format!("target_version={}", safe_version(version.as_deref())),
        ],
        UpdateStatus::Downloading { version, percent } => vec![
            "status=downloading".to_owned(),
            format!("target_version={}", safe_version(version.as_deref())),
            percent.map_or_else(
                || "progress_percent=unavailable".to_owned(),
                |percent| format!("progress_percent={percent}"),
            ),
        ],
        UpdateStatus::Installing { version } => vec![
            "status=installing".to_owned(),
            format!("target_version={}", safe_version(version.as_deref())),
        ],
        UpdateStatus::Failed {
            stage,
            trigger,
            pending,
        } => vec![
            "status=failed".to_owned(),
            format!("failure_stage={}", update_stage(*stage)),
            format!("trigger={}", update_trigger(*trigger)),
            format!("target_version={}", pending_version(pending.as_ref())),
        ],
    }
}

fn safe_version(version: Option<&str>) -> String {
    version.map_or_else(
        || "unavailable".to_owned(),
        |value| safe_external(value, 64),
    )
}

fn pending_version(pending: Option<&PendingUpdateView>) -> String {
    pending.and_then(PendingUpdateView::version).map_or_else(
        || "unavailable".to_owned(),
        |value| safe_external(value, 64),
    )
}

fn update_trigger(trigger: UpdateTrigger) -> &'static str {
    match trigger {
        UpdateTrigger::Automatic => "automatic",
        UpdateTrigger::Manual => "manual",
    }
}

fn update_stage(stage: UpdateFailureStage) -> &'static str {
    match stage {
        UpdateFailureStage::Scheduling => "scheduling",
        UpdateFailureStage::Check => "check",
        UpdateFailureStage::Download => "download",
        UpdateFailureStage::Verification => "verification",
        UpdateFailureStage::Install => "install",
        UpdateFailureStage::Restart => "restart",
    }
}

fn protection(value: ProtectionState) -> &'static str {
    match value {
        ProtectionState::Inactive => "inactive",
        ProtectionState::Active => "active",
    }
}

fn wake_reasons(reasons: &[WakeReason]) -> String {
    if reasons.is_empty() {
        return "none".to_owned();
    }
    reasons
        .iter()
        .map(|reason| match reason {
            WakeReason::AutomaticDownload => "automatic-download",
            WakeReason::ManualKeepAwake => "manual-keep-awake",
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn load_condition(value: LoadCondition) -> String {
    match value {
        LoadCondition::FirstRun => "first-run".to_owned(),
        LoadCondition::CurrentVersion => "current-version".to_owned(),
        LoadCondition::PartialRecovery => "partial-recovery".to_owned(),
        LoadCondition::RecoveredFromBackup => "recovered-from-backup".to_owned(),
        LoadCondition::MalformedDefaulted => "malformed-defaulted".to_owned(),
        LoadCondition::PersistenceUnavailable => "persistence-unavailable".to_owned(),
        LoadCondition::SchemaMetadataRecovered => "schema-metadata-recovered".to_owned(),
        LoadCondition::UnsupportedFutureSchema { found } => {
            format!("unsupported-future-schema:{found}")
        }
    }
}

fn persistence(value: PersistenceCondition) -> &'static str {
    match value {
        PersistenceCondition::Writable => "writable",
        PersistenceCondition::Unavailable => "unavailable",
        PersistenceCondition::BlockedByFutureSchema => "blocked-by-future-schema",
    }
}

fn artifact_warning(value: ArtifactWarning) -> &'static str {
    match value {
        ArtifactWarning::StaleTemporary => "stale-temporary",
        ArtifactWarning::Quarantine => "quarantine",
        ArtifactWarning::QuarantineCleanup => "quarantine-cleanup",
    }
}

fn setting_path(value: SettingPath) -> &'static str {
    match value {
        SettingPath::AutomaticDownloadProtectionEnabled => "automatic-download-protection",
        SettingPath::ProtectOnBattery => "protect-on-battery",
        SettingPath::LaunchAtLogin => "launch-at-login",
        SettingPath::AutomaticUpdateChecksEnabled => "automatic-update-checks",
        SettingPath::LastAutomaticUpdateCheckUnixSeconds => "last-automatic-update-check",
        SettingPath::MeaningfulReceiveRateBytesPerSecond => "meaningful-receive-rate",
        SettingPath::ActivationQualificationMilliseconds => "activation-qualification",
        SettingPath::GraceMilliseconds => "grace",
    }
}

fn recovery_reason(value: RecoveryReason) -> &'static str {
    match value {
        RecoveryReason::Missing => "missing",
        RecoveryReason::WrongType => "wrong-type",
        RecoveryReason::InvalidValue => "invalid-value",
    }
}

fn settings_failure(value: SettingsPersistenceFailure) -> &'static str {
    match value {
        SettingsPersistenceFailure::Unavailable => "unavailable",
        SettingsPersistenceFailure::CreateDirectory => "create-directory",
        SettingsPersistenceFailure::PreserveSource => "preserve-source",
        SettingsPersistenceFailure::Serialize => "serialize",
        SettingsPersistenceFailure::WriteBackup => "write-backup",
        SettingsPersistenceFailure::WritePrimary => "write-primary",
        SettingsPersistenceFailure::ReplaceBackup => "replace-backup",
        SettingsPersistenceFailure::ReplacePrimary => "replace-primary",
    }
}

fn logging_health_name(value: LoggingHealth) -> &'static str {
    match value {
        LoggingHealth::Available => "available",
        LoggingHealth::Unavailable(LoggingFailureStage::ResolveDirectory) => {
            "unavailable:directory-resolution"
        }
        LoggingHealth::Unavailable(LoggingFailureStage::CreateDirectory) => {
            "unavailable:directory-creation"
        }
        LoggingHealth::Unavailable(LoggingFailureStage::Open) => "unavailable:open",
        LoggingHealth::Unavailable(LoggingFailureStage::Prune) => "unavailable:prune",
        LoggingHealth::Unavailable(LoggingFailureStage::Rotate) => "unavailable:rotation",
        LoggingHealth::Unavailable(LoggingFailureStage::Write) => "unavailable:write",
        LoggingHealth::Unavailable(LoggingFailureStage::Flush) => "unavailable:flush",
    }
}
