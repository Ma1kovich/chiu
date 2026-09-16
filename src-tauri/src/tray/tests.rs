use super::*;
use crate::{
    application::LifecyclePlan,
    launch_at_login::{LaunchAtLoginFailureStage, Registration},
    manual_session::{ManualSessionAddPreset, ManualSessionCommand, ManualSessionStartPreset},
    settings::SettingsService,
    updater::{
        PendingUpdateView, UpdateAvailability, UpdateStatus, UpdaterService, UpdaterSnapshot,
    },
    wake_coordinator::SleepInhibitor,
};
use std::{error::Error, fs, path::PathBuf};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("chiu-tray-{name}-{}", std::process::id()));
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

struct NoopInhibitor(ProtectionState);

impl SleepInhibitor for NoopInhibitor {
    fn state(&self) -> ProtectionState {
        self.0
    }

    fn acquire(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.0 = ProtectionState::Active;
        Ok(())
    }

    fn release(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.0 = ProtectionState::Inactive;
        Ok(())
    }
}

fn normal_input() -> TrayInput {
    TrayInput {
        application_status: ApplicationStatus::Ready,
        manual_state: ManualSessionState::Inactive,
        manual_failure: false,
        detector_state: AutomaticDownloadState::Idle,
        receive_rate: None,
        tuning: AutomaticDownloadSettings::default(),
        automatic_settings: AutomaticProtectionSettings::new(true, false),
        detector_intent: false,
        desired_automatic_reason: false,
        suppression: None,
        automatic_failure: false,
        network_status: NetworkActivitySourceStatus::Available,
        network_failure: false,
        power_source: PowerSource::External,
        power_health: PowerSourceObserverHealth::Observing,
        power_failure: false,
        settings_alert: SettingsAlert::None,
        launch_at_login_available: true,
        launch_at_login_desired: true,
        launch_at_login_observed: Registration::Enabled,
        launch_at_login_failure: None,
        updater: UpdaterSnapshot {
            availability: UpdateAvailability::Unconfigured,
            automatic_checks_enabled: true,
            status: UpdateStatus::Idle,
        },
        wake_reasons: Vec::new(),
        protection: ProtectionState::Inactive,
    }
}

#[test]
fn launch_at_login_view_uses_saved_preference_and_explicit_availability() {
    let mut input = normal_input();

    let available = project(&input).launch_at_login;
    assert_eq!(available.label, "Launch at login");
    assert!(available.enabled);
    assert!(available.checked);

    input.launch_at_login_available = false;
    input.launch_at_login_observed = Registration::Unknown;
    input.launch_at_login_failure = Some(LaunchAtLoginFailureStage::Initialize);
    let unavailable_view = project(&input);
    let unavailable = unavailable_view.launch_at_login;
    assert_eq!(unavailable.label, "Launch at login — unavailable");
    assert!(!unavailable.enabled);
    assert!(unavailable.checked);
    assert_eq!(
        unavailable_view.warning.as_deref(),
        Some("Launch at login is unavailable")
    );
}

#[test]
fn updater_projection_preserves_preference_manual_access_and_pending_install() {
    let mut input = normal_input();
    input.updater = UpdaterSnapshot {
        availability: UpdateAvailability::Available,
        automatic_checks_enabled: false,
        status: UpdateStatus::UpdateAvailable {
            version: Some("1.2.3".to_owned()),
        },
    };

    let updater = project(&input).updater;

    assert_eq!(updater.automatic_label, "Automatically check for updates");
    assert!(updater.automatic_enabled);
    assert!(!updater.automatic_checked);
    assert!(updater.check_enabled);
    assert_eq!(
        updater.result.as_deref(),
        Some("Update available — Chiù 1.2.3")
    );
    assert_eq!(
        updater.install,
        Some(("Install Chiù 1.2.3…".to_owned(), true))
    );
}

#[test]
fn updater_operation_and_failure_project_truthfully_without_hiding_core_warning() {
    let mut input = normal_input();
    input.updater = UpdaterSnapshot {
        availability: UpdateAvailability::Available,
        automatic_checks_enabled: true,
        status: UpdateStatus::Downloading {
            version: Some("1.2.3".to_owned()),
            percent: Some(42),
        },
    };
    let downloading = project(&input);
    assert!(!downloading.updater.check_enabled);
    assert_eq!(
        downloading.updater.result.as_deref(),
        Some("Downloading update… 42%")
    );

    input.updater.status = UpdateStatus::Failed {
        stage: UpdateFailureStage::Verification,
        trigger: crate::updater::UpdateTrigger::Manual,
        pending: Some(PendingUpdateView::Version("1.2.3".to_owned())),
    };
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Update verification failed")
    );

    input.manual_failure = true;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Sleep protection needs attention")
    );
}

#[test]
fn unattached_tray_projects_startup_failure_and_accepts_only_quit() {
    let mut lifecycle = LifecyclePlan::new(|| {
        Err::<SettingsService, Box<dyn Error + Send + Sync>>(
            std::io::Error::other("settings unavailable").into(),
        )
    })
    .build();
    let application = TrayApplication::new(
        lifecycle.state(),
        DiagnosticsService::unavailable_for_tests(),
    );

    let starting = application.view();
    assert_eq!(starting.primary, "Starting…");
    assert!(!starting.commands_enabled);

    lifecycle.start().expect_err("startup should fail");

    let failed = application.view();
    assert_eq!(failed.primary, "Startup failed");
    assert!(!failed.commands_enabled);
    for command in [
        TrayCommand::ToggleAutomaticProtection,
        TrayCommand::ToggleProtectOnBattery,
        TrayCommand::Manual(ManualSessionCommand::Start(
            ManualSessionStartPreset::FifteenMinutes,
        )),
        TrayCommand::SetThreshold(ThresholdPreset::Kibibytes256),
        TrayCommand::SetGrace(GracePreset::Seconds30),
        TrayCommand::ToggleLaunchAtLogin,
    ] {
        assert_eq!(
            application.execute(command),
            TrayCommandOutcome::Ignored,
            "{}",
            command.id()
        );
    }
    assert_eq!(
        application.execute(TrayCommand::Quit),
        TrayCommandOutcome::QuitRequested
    );
}

#[test]
fn startup_failure_keeps_diagnostics_available_on_the_recovery_surface() {
    let mut lifecycle = LifecyclePlan::new(|| {
        Err::<SettingsService, Box<dyn Error + Send + Sync>>(
            std::io::Error::other("settings unavailable").into(),
        )
    })
    .build();
    let (log, _) = crate::local_log::LocalLog::recording();
    let diagnostics = DiagnosticsService::available_for_tests(log);
    let application = TrayApplication::new(lifecycle.state(), diagnostics.clone());
    lifecycle.start().unwrap_err();

    let view = application.view();
    assert!(!view.commands_enabled);
    assert!(view.diagnostics.copy_enabled);
    assert!(view.diagnostics.open_logs_enabled);
    assert_eq!(
        application.execute(TrayCommand::CopyDiagnostics),
        TrayCommandOutcome::RefreshRequested
    );
    assert_eq!(
        application.execute(TrayCommand::OpenLogs),
        TrayCommandOutcome::RefreshRequested
    );

    diagnostics.begin_shutdown();
    let shutdown_view = application.view();
    assert!(!shutdown_view.diagnostics.copy_enabled);
    assert!(!shutdown_view.diagnostics.open_logs_enabled);
}

#[test]
fn lifecycle_status_has_precedence_and_gates_product_commands() {
    for (status, primary, enabled) in [
        (ApplicationStatus::Starting, "Starting…", false),
        (ApplicationStatus::Failed, "Startup failed", false),
        (ApplicationStatus::ShuttingDown, "Shutting down…", false),
        (ApplicationStatus::Stopped, "Shutting down…", false),
        (ApplicationStatus::Ready, "Ready to keep awake", true),
        (ApplicationStatus::Degraded, "Ready to keep awake", true),
    ] {
        let mut input = normal_input();
        input.application_status = status;

        let view = project(&input);

        assert_eq!(view.primary, primary);
        assert_eq!(view.commands_enabled, enabled);
    }
}

#[test]
fn coordinator_truth_drives_protection_status_and_reason_text() {
    let mut input = normal_input();
    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(65 * 60),
    };
    input.detector_state = AutomaticDownloadState::Hold {
        remaining_grace: Duration::from_secs(61),
    };
    input.detector_intent = true;
    input.desired_automatic_reason = true;
    input.wake_reasons = vec![WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake];
    input.protection = ProtectionState::Active;

    let protected = project(&input);

    assert_eq!(protected.primary, "Keeping awake");
    assert_eq!(
        protected.detail.as_deref(),
        Some("Reasons: Recent network activity — 2 min remaining; Manual — 1 h 5 min remaining")
    );

    input.protection = ProtectionState::Inactive;
    assert_eq!(project(&input).primary, "Protection failed");

    input.wake_reasons.clear();
    input.manual_state = ManualSessionState::Inactive;
    input.detector_state = AutomaticDownloadState::Idle;
    input.detector_intent = false;
    input.desired_automatic_reason = false;
    input.protection = ProtectionState::Active;
    let release_failed = project(&input);
    assert_eq!(release_failed.primary, "Sleep protection could not stop");
    assert_eq!(
        release_failed.detail.as_deref(),
        Some("No wake reason remains")
    );
}

#[test]
fn manual_reason_wording_is_exact() {
    let mut input = normal_input();
    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(60),
    };
    input.wake_reasons = vec![WakeReason::ManualKeepAwake];
    input.protection = ProtectionState::Active;
    assert_eq!(
        project(&input).detail.as_deref(),
        Some("Reason: Manual — 1 min remaining")
    );
}

#[test]
fn automatic_reason_distinguishes_current_rate_from_recent_activity() {
    for (state, receive_rate, manual_state, wake_reasons, expected) in [
        (
            AutomaticDownloadState::Active,
            Some(3 * 1024 * 1024),
            ManualSessionState::Inactive,
            vec![WakeReason::AutomaticDownload],
            "Reason: Network activity — 3 MB/s",
        ),
        (
            AutomaticDownloadState::Active,
            None,
            ManualSessionState::Inactive,
            vec![WakeReason::AutomaticDownload],
            "Reason: Network activity",
        ),
        (
            AutomaticDownloadState::Hold {
                remaining_grace: Duration::from_secs(61),
            },
            Some(3 * 1024 * 1024),
            ManualSessionState::Inactive,
            vec![WakeReason::AutomaticDownload],
            "Reason: Recent network activity — 2 min remaining",
        ),
        (
            AutomaticDownloadState::Idle,
            Some(3 * 1024 * 1024),
            ManualSessionState::Inactive,
            vec![WakeReason::AutomaticDownload],
            "Reason: Network activity",
        ),
        (
            AutomaticDownloadState::Active,
            Some(3 * 1024 * 1024),
            ManualSessionState::Finite {
                remaining: Duration::from_secs(60),
            },
            vec![WakeReason::AutomaticDownload, WakeReason::ManualKeepAwake],
            "Reasons: Network activity — 3 MB/s; Manual — 1 min remaining",
        ),
    ] {
        let mut input = normal_input();
        input.detector_state = state;
        input.receive_rate = receive_rate;
        input.manual_state = manual_state;
        input.detector_intent = true;
        input.desired_automatic_reason = true;
        input.wake_reasons = wake_reasons;
        input.protection = ProtectionState::Active;

        assert_eq!(project(&input).detail.as_deref(), Some(expected));
    }
}

#[test]
fn checked_controls_are_projected_only_from_effective_policy_settings() {
    let mut input = normal_input();
    input.automatic_settings = AutomaticProtectionSettings::new(false, true);

    let view = project(&input);

    assert!(!view.automatic_checked);
    assert!(view.protect_on_battery_checked);
}

#[test]
fn idle_product_statuses_follow_the_settled_precedence() {
    let mut input = normal_input();
    input.detector_state = AutomaticDownloadState::Active;
    input.detector_intent = true;
    input.suppression = Some(AutomaticProtectionSuppression::LimitedPower);
    assert_eq!(
        project(&input).primary,
        "Network activity detected, not keeping awake"
    );
    assert_eq!(
        project(&input).detail.as_deref(),
        Some("Paused on battery or limited power")
    );

    input.detector_state = AutomaticDownloadState::Idle;
    input.detector_intent = false;
    input.suppression = None;
    input.network_status = NetworkActivitySourceStatus::Unavailable;
    let unavailable = project(&input);
    assert_eq!(unavailable.primary, "Can’t detect network activity");
    assert_eq!(
        unavailable.warning.as_deref(),
        Some("Keep awake during downloads is unavailable")
    );

    input.network_status = NetworkActivitySourceStatus::Available;
    input.automatic_settings = AutomaticProtectionSettings::new(false, false);
    assert_eq!(project(&input).primary, "Keep awake during downloads: Off");

    input.detector_state = AutomaticDownloadState::Active;
    input.detector_intent = true;
    input.desired_automatic_reason = false;
    input.suppression = Some(AutomaticProtectionSuppression::AutomaticDisabled);
    input.wake_reasons.clear();
    input.protection = ProtectionState::Inactive;
    let disabled_during_activity = project(&input);
    assert_eq!(
        disabled_during_activity.primary,
        "Network activity detected, not keeping awake"
    );
    assert_eq!(
        disabled_during_activity.detail.as_deref(),
        Some("Keep awake during downloads: Off")
    );
}

#[test]
fn warnings_are_safe_and_follow_one_authoritative_priority() {
    let mut input = normal_input();
    input.settings_alert = SettingsAlert::Recovered;
    input.power_failure = true;
    input.network_failure = true;
    input.settings_alert = SettingsAlert::SaveFailed;
    input.automatic_failure = true;

    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Sleep protection needs attention")
    );

    input.automatic_failure = false;
    input.launch_at_login_failure = Some(LaunchAtLoginFailureStage::Enable);
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Settings could not be saved")
    );
    input.settings_alert = SettingsAlert::None;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Launch at login could not be enabled")
    );
    input.launch_at_login_failure = None;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Keep awake during downloads is unavailable")
    );
    input.network_failure = false;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Power source monitoring is unavailable")
    );
}

#[test]
fn launch_warning_wording_distinguishes_apply_and_verification_failures() {
    let mut input = normal_input();
    input.launch_at_login_failure = Some(LaunchAtLoginFailureStage::Disable);
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Launch at login could not be disabled")
    );

    input.launch_at_login_failure = Some(LaunchAtLoginFailureStage::Verify);
    input.launch_at_login_observed = Registration::Unknown;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Launch-at-login status could not be verified")
    );

    input.launch_at_login_desired = false;
    input.launch_at_login_observed = Registration::Enabled;
    assert_eq!(
        project(&input).warning.as_deref(),
        Some("Launch at login could not be disabled")
    );

    input.launch_at_login_failure = None;
    input.launch_at_login_observed = Registration::Disabled;
    assert!(project(&input).warning.is_none());
}

#[test]
fn inconsistent_intent_never_becomes_a_normal_protection_claim() {
    let mut input = normal_input();
    input.manual_state = ManualSessionState::UntilDisabled;

    let view = project(&input);

    assert_ne!(view.primary, "Keeping awake");
    assert_eq!(
        view.warning.as_deref(),
        Some("Sleep protection needs attention")
    );
}

#[test]
fn manual_menu_shapes_and_remaining_boundaries_are_presentation_only() {
    let mut input = normal_input();
    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(59),
    };
    input.wake_reasons = vec![WakeReason::ManualKeepAwake];
    input.protection = ProtectionState::Active;
    assert_eq!(
        project(&input).keep_awake,
        KeepAwakeView::Finite {
            remaining: "<1 min remaining".to_owned()
        }
    );

    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(60 * 60),
    };
    assert_eq!(
        project(&input).keep_awake,
        KeepAwakeView::Finite {
            remaining: "1 h remaining".to_owned()
        }
    );

    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(60 * 60 + 1),
    };
    assert_eq!(
        project(&input).keep_awake,
        KeepAwakeView::Finite {
            remaining: "1 h 1 min remaining".to_owned()
        }
    );

    input.manual_state = ManualSessionState::Finite {
        remaining: Duration::from_secs(60) + Duration::from_nanos(1),
    };
    assert_eq!(
        project(&input).keep_awake,
        KeepAwakeView::Finite {
            remaining: "2 min remaining".to_owned()
        }
    );

    input.manual_state = ManualSessionState::UntilDisabled;
    assert_eq!(project(&input).keep_awake, KeepAwakeView::UntilDisabled);
}

#[test]
fn detector_context_and_exact_preset_selection_are_truthful() {
    let mut input = normal_input();
    input.detector_state = AutomaticDownloadState::Hold {
        remaining_grace: Duration::from_secs(119),
    };
    input.detector_intent = true;
    input.desired_automatic_reason = true;
    input.receive_rate = Some(3 * 1024 * 1024);
    input.power_source = PowerSource::Limited;

    let view = project(&input).detection;

    assert_eq!(view.state, "Grace — 2 min remaining");
    assert_eq!(view.receive_rate.as_deref(), Some("3 MB/s"));
    assert_eq!(view.power_source, "Battery or limited power");
    assert_eq!(view.threshold.checked, [false, true, false]);
    assert_eq!(view.threshold.custom, None);
    assert_eq!(view.grace.checked, [false, true, false, false, false]);
    assert_eq!(view.grace.custom, None);
}

#[test]
fn valid_custom_tuning_is_shown_without_checking_a_nearest_preset() {
    let mut input = normal_input();
    input.tuning = AutomaticDownloadSettings::try_new(
        3 * 1024 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(90),
    )
    .unwrap();

    let view = project(&input).detection;

    assert_eq!(view.threshold.checked, [false; 3]);
    assert_eq!(view.threshold.custom.as_deref(), Some("Custom — 3 MB/s"));
    assert_eq!(view.grace.checked, [false; 5]);
    assert_eq!(view.grace.custom.as_deref(), Some("Custom — 90 seconds"));
}

#[test]
fn fifteen_minute_grace_preset_is_projected_as_selected() {
    let mut input = normal_input();
    input.tuning = AutomaticDownloadSettings::try_new(
        1_048_576,
        Duration::from_secs(5),
        Duration::from_secs(15 * 60),
    )
    .unwrap();

    let view = project(&input).detection;

    assert_eq!(view.grace.checked, [false, false, false, true, false]);
    assert_eq!(view.grace.custom, None);
}

#[test]
fn thirty_minute_grace_preset_is_projected_as_selected() {
    let mut input = normal_input();
    input.tuning = AutomaticDownloadSettings::try_new(
        1_048_576,
        Duration::from_secs(5),
        Duration::from_secs(30 * 60),
    )
    .unwrap();

    let view = project(&input).detection;

    assert_eq!(view.grace.checked, [false, false, false, false, true]);
    assert_eq!(view.grace.custom, None);
}

#[test]
fn command_facade_routes_real_services_and_rejects_shutdown_or_placeholder_work() {
    let directory = TestDirectory::new("commands");
    let log = crate::local_log::LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open);
    let settings = SettingsService::load(directory.0.clone(), log.clone());
    let coordinator =
        WakeCoordinator::with_inhibitor(NoopInhibitor(ProtectionState::Inactive), log.clone());
    let manual = ManualSession::new(coordinator.clone(), log.clone());
    let controller = AutomaticProtectionController::new(
        AutomaticProtectionSettings::new(true, false),
        coordinator.clone(),
        log.clone(),
    );
    controller.set_power_source(PowerSource::External);
    let detector = AutomaticDownloadMonitor::new(
        AutomaticDownloadSettings::default(),
        controller.clone(),
        log.clone(),
    );
    let tuning = AutomaticDownloadTuningService::new(settings.clone(), detector.clone());
    let policy = AutomaticProtectionService::new(settings.clone(), controller.clone());
    let network = NetworkActivitySource::new(log.clone());
    let power = PowerSourceObserver::new(controller.clone(), log.clone());
    let launch_at_login = crate::test_launch_at_login(settings.clone(), log.clone()).unwrap();
    let updater = UpdaterService::unconfigured(&settings);
    let restored = settings.clone();
    let mut lifecycle = LifecyclePlan::new(move || Ok(restored)).build();
    lifecycle.start().unwrap();
    let application = TrayApplication::new(
        lifecycle.state(),
        DiagnosticsService::unavailable_for_tests(),
    );
    application.activate(TrayProduct::new(
        manual.clone(),
        detector.clone(),
        tuning,
        controller.clone(),
        policy,
        network,
        power,
        launch_at_login,
        updater,
        settings.clone(),
        coordinator.clone(),
    ));

    assert_eq!(
        application.execute(TrayCommand::Manual(ManualSessionCommand::Start(
            ManualSessionStartPreset::FifteenMinutes,
        ))),
        TrayCommandOutcome::RefreshRequested
    );
    assert!(matches!(
        manual.snapshot().state,
        ManualSessionState::Finite { .. }
    ));
    application.execute(TrayCommand::Manual(ManualSessionCommand::Add(
        ManualSessionAddPreset::FifteenMinutes,
    )));
    let ManualSessionState::Finite { remaining } = manual.snapshot().state else {
        panic!("manual add must preserve a finite session");
    };
    assert!(remaining > Duration::from_secs(29 * 60));
    application.execute(TrayCommand::Manual(
        ManualSessionCommand::ConvertToUntilDisabled,
    ));
    assert_eq!(manual.snapshot().state, ManualSessionState::UntilDisabled);
    application.execute(TrayCommand::Manual(ManualSessionCommand::Stop));
    assert_eq!(manual.snapshot().state, ManualSessionState::Inactive);

    application.execute(TrayCommand::ToggleAutomaticProtection);
    assert!(
        !controller
            .snapshot()
            .settings()
            .automatic_download_protection_enabled()
    );
    application.execute(TrayCommand::ToggleProtectOnBattery);
    assert!(controller.snapshot().settings().protect_on_battery());
    application.execute(TrayCommand::SetThreshold(ThresholdPreset::Kibibytes256));
    application.execute(TrayCommand::SetGrace(GracePreset::Seconds30));
    assert_eq!(
        detector
            .snapshot()
            .settings
            .meaningful_receive_rate_bytes_per_second(),
        262_144
    );
    assert_eq!(
        detector.snapshot().settings.grace(),
        Duration::from_secs(30)
    );

    for (preset, expected) in [
        (GracePreset::Minutes15, Duration::from_secs(15 * 60)),
        (GracePreset::Minutes30, Duration::from_secs(30 * 60)),
    ] {
        assert_eq!(
            application.execute(TrayCommand::SetGrace(preset)),
            TrayCommandOutcome::RefreshRequested
        );
        assert_eq!(detector.snapshot().settings.grace(), expected);
        assert_eq!(settings.snapshot().automatic_download().grace(), expected);

        let restored = SettingsService::load(directory.0.clone(), log.clone());
        assert_eq!(restored.snapshot().automatic_download().grace(), expected);
    }

    let before_launch_manual = manual.snapshot();
    let before_launch_automatic = controller.snapshot();
    let before_launch_wake = coordinator.snapshot();
    assert_eq!(
        application.execute(TrayCommand::ToggleLaunchAtLogin),
        TrayCommandOutcome::RefreshRequested
    );
    assert!(!settings.snapshot().launch_at_login());
    assert_eq!(manual.snapshot(), before_launch_manual);
    assert_eq!(controller.snapshot(), before_launch_automatic);
    assert_eq!(coordinator.snapshot(), before_launch_wake);

    lifecycle.request_shutdown();
    let before_shutdown = controller.snapshot().settings();
    assert_eq!(
        application.execute(TrayCommand::ToggleAutomaticProtection),
        TrayCommandOutcome::Ignored
    );
    assert_eq!(controller.snapshot().settings(), before_shutdown);
    let launch_preference_before_shutdown = settings.snapshot().launch_at_login();
    assert_eq!(
        application.execute(TrayCommand::ToggleLaunchAtLogin),
        TrayCommandOutcome::Ignored
    );
    assert_eq!(
        settings.snapshot().launch_at_login(),
        launch_preference_before_shutdown
    );
    assert_eq!(
        application.execute(TrayCommand::Quit),
        TrayCommandOutcome::QuitRequested
    );
    lifecycle.shutdown();
}
