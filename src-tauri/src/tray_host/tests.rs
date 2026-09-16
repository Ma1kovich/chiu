use super::menu_layout::SettingsItem;
use super::*;
use crate::{
    application::{CapabilityId, LifecyclePlan, OwnedResource},
    diagnostics::DiagnosticsService,
    manual_session::{ManualSessionAddPreset, ManualSessionCommand, ManualSessionStartPreset},
    tray::{ChoiceView, DetectionView, DiagnosticsView, LaunchAtLoginView, UpdaterView},
    wake_coordinator::{WakeCoordinator, WakeReason},
};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::RecvTimeoutError,
};

#[derive(Default)]
struct RecordingCopyDiagnosticsErrorPresenter(std::sync::atomic::AtomicUsize);

impl CopyDiagnosticsErrorPresenter for RecordingCopyDiagnosticsErrorPresenter {
    fn show_copy_failed(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
struct MutableSource(Arc<Mutex<TrayView>>);

impl ViewSource for MutableSource {
    fn view(&self) -> TrayView {
        self.0.lock().unwrap().clone()
    }
}

struct RecordingRenderer(mpsc::Sender<TrayView>);

impl Renderer for RecordingRenderer {
    fn render(&mut self, view: &TrayView) -> Result<(), BoxError> {
        self.0.send(view.clone()).unwrap();
        Ok(())
    }
}

struct BlockingPanicRenderer {
    entered: mpsc::Sender<()>,
    proceed: mpsc::Receiver<()>,
}

struct FailOnceRenderer {
    attempts: mpsc::Sender<bool>,
    failed: bool,
}

impl Renderer for FailOnceRenderer {
    fn render(&mut self, _: &TrayView) -> Result<(), BoxError> {
        if !self.failed {
            self.failed = true;
            self.attempts.send(false).unwrap();
            return Err(Box::new(std::io::Error::other("scripted render failure")));
        }
        self.attempts.send(true).unwrap();
        Ok(())
    }
}

impl Renderer for BlockingPanicRenderer {
    fn render(&mut self, _: &TrayView) -> Result<(), BoxError> {
        self.entered.send(()).unwrap();
        self.proceed.recv().unwrap();
        panic!("scripted renderer panic");
    }
}

fn view(primary: &str) -> TrayView {
    TrayView {
        primary: primary.to_owned(),
        detail: None,
        warning: None,
        commands_enabled: true,
        automatic_checked: true,
        protect_on_battery_checked: false,
        keep_awake: KeepAwakeView::Inactive,
        detection: DetectionView {
            state: "Waiting".to_owned(),
            receive_rate: None,
            power_source: "External power".to_owned(),
            threshold: ChoiceView {
                checked: [false, true, false],
                custom: None,
            },
            grace: ChoiceView {
                checked: [false, true, false, false, false],
                custom: None,
            },
        },
        launch_at_login: LaunchAtLoginView {
            label: "Launch at login".to_owned(),
            enabled: true,
            checked: true,
        },
        updater: UpdaterView {
            automatic_label: "Automatically check for updates — unavailable".to_owned(),
            automatic_enabled: false,
            automatic_checked: true,
            check_label: "Check for updates… — unavailable".to_owned(),
            check_enabled: false,
            result: None,
            install: None,
        },
        diagnostics: DiagnosticsView {
            copy_label: "Copy Diagnostics".to_owned(),
            copy_enabled: false,
            open_logs_label: "Open Logs — unavailable".to_owned(),
            open_logs_enabled: false,
        },
    }
}

#[test]
fn grace_choices_use_compact_copy_and_the_settled_order() {
    assert_eq!(GRACE_MENU_LABEL, "Stop keeping awake after traffic drops");
    assert_eq!(
        grace_choice_spec().map(|(command, _)| command.id()),
        [
            "detection.grace.30",
            "detection.grace.120",
            "detection.grace.300",
            "detection.grace.900",
            "detection.grace.1800",
        ]
    );
    assert_eq!(
        grace_choice_spec().map(|(_, label)| label),
        [
            "30 seconds",
            "2 minutes",
            "5 minutes",
            "15 minutes",
            "30 minutes"
        ]
    );
}

fn control(sender: mpsc::Sender<WorkerCommand>) -> PresentationControl {
    PresentationControl {
        sender,
        refresh_pending: Arc::new(AtomicBool::new(false)),
        exit_phase: Arc::new(Mutex::new(ExitPhase::Running)),
    }
}

#[test]
fn command_dispatch_presents_one_copy_error_and_none_for_success_or_stale_actions() {
    let presenter = RecordingCopyDiagnosticsErrorPresenter::default();
    let refreshes = std::sync::atomic::AtomicUsize::new(0);
    let exits = std::sync::atomic::AtomicUsize::new(0);

    dispatch_command_outcome(
        TrayCommandOutcome::CopyDiagnosticsFailed,
        || {
            refreshes.fetch_add(1, Ordering::AcqRel);
        },
        || {
            exits.fetch_add(1, Ordering::AcqRel);
        },
        Some(&presenter),
    );
    dispatch_command_outcome(
        TrayCommandOutcome::RefreshRequested,
        || {
            refreshes.fetch_add(1, Ordering::AcqRel);
        },
        || {
            exits.fetch_add(1, Ordering::AcqRel);
        },
        Some(&presenter),
    );
    dispatch_command_outcome(
        TrayCommandOutcome::Ignored,
        || {
            refreshes.fetch_add(1, Ordering::AcqRel);
        },
        || {
            exits.fetch_add(1, Ordering::AcqRel);
        },
        Some(&presenter),
    );

    assert_eq!(presenter.0.load(Ordering::Acquire), 1);
    assert_eq!(refreshes.load(Ordering::Acquire), 1);
    assert_eq!(exits.load(Ordering::Acquire), 0);
}

#[test]
fn settings_failure_is_rendered_by_the_production_startup_order_without_attachment() {
    let mut lifecycle = LifecyclePlan::new(|| {
        Err::<(), BoxError>(std::io::Error::other("settings unavailable").into())
    })
    .build();
    let presentation_owner = Arc::new(Mutex::new(None));
    let application_slot = Arc::new(OnceLock::new());
    let presentation_application = application_slot.clone();
    let application = TrayApplication::new(
        lifecycle.state(),
        DiagnosticsService::unavailable_for_tests(),
    );
    let (rendered_tx, rendered_rx) = mpsc::channel();

    let outcome = crate::orchestrate_startup(
        &presentation_owner,
        move || {
            assert!(presentation_application.set(application.clone()).is_ok());
            let initial_view = application.view();
            rendered_tx.send(initial_view.clone()).unwrap();
            let (command_tx, command_rx) = mpsc::channel();
            let control = control(command_tx);
            spawn(
                application,
                RecordingRenderer(rendered_tx),
                command_rx,
                control,
                Some(initial_view),
                Duration::from_secs(60),
                |_| {},
            )
        },
        || lifecycle.start(),
    )
    .expect("the recovery surface should start");

    assert!(matches!(outcome, crate::HostStartup::ProductFailed(_)));
    assert!(presentation_owner.lock().unwrap().is_some());
    assert_eq!(
        rendered_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .primary,
        "Starting…"
    );
    let failed = rendered_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap();
    assert_eq!(failed.primary, "Startup failed");
    assert!(!failed.commands_enabled);
    let application = application_slot.get().unwrap();
    assert_eq!(
        application.execute(TrayCommand::ToggleAutomaticProtection),
        TrayCommandOutcome::Ignored
    );
    assert_eq!(
        application.execute(TrayCommand::Quit),
        TrayCommandOutcome::QuitRequested
    );

    crate::finish_presentation_shutdown(&presentation_owner).unwrap();
}

#[test]
fn required_failure_rolls_back_before_rendering_and_never_attaches_product_handles() {
    let log = crate::local_log::LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open);
    let coordinator = WakeCoordinator::new(log.clone());
    coordinator
        .set_reason_active(WakeReason::ManualKeepAwake, true)
        .expect("preexisting wake intent should activate");
    let cleanup_coordinator = coordinator.clone();
    let plan = LifecyclePlan::new(|| Ok(()))
        .required(CapabilityId::new("wake-coordination"), move |_| {
            Ok(OwnedResource::new(move || {
                cleanup_coordinator
                    .shutdown()
                    .map_err(|error| Box::new(error) as BoxError)
            }))
        })
        .required(CapabilityId::new("scripted-failure"), |_| {
            Err(std::io::Error::other("required startup failed").into())
        });
    let mut lifecycle = plan.build();
    let application = TrayApplication::new(
        lifecycle.state(),
        DiagnosticsService::unavailable_for_tests(),
    );
    let presentation_application = application.clone();
    let presentation_owner = Arc::new(Mutex::new(None));
    let (rendered_tx, rendered_rx) = mpsc::channel();

    let outcome = crate::orchestrate_startup(
        &presentation_owner,
        move || {
            let initial_view = presentation_application.view();
            rendered_tx.send(initial_view.clone()).unwrap();
            let (command_tx, command_rx) = mpsc::channel();
            let control = control(command_tx);
            spawn(
                presentation_application,
                RecordingRenderer(rendered_tx),
                command_rx,
                control,
                Some(initial_view),
                Duration::from_secs(60),
                |_| {},
            )
        },
        || lifecycle.start(),
    )
    .expect("the recovery surface should start");

    let crate::HostStartup::ProductFailed(error) = outcome else {
        panic!("required startup should fail");
    };
    assert_eq!(
        error.failure_point(),
        Some(crate::application::StartupFailurePoint::Required(
            CapabilityId::new("scripted-failure")
        ))
    );
    assert!(coordinator.snapshot().active_reasons().is_empty());
    assert_eq!(
        rendered_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .primary,
        "Starting…"
    );
    let failed = rendered_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap();
    assert_eq!(failed.primary, "Startup failed");
    assert_eq!(failed.detection.state, "Unavailable");
    assert_eq!(
        application.execute(TrayCommand::ToggleAutomaticProtection),
        TrayCommandOutcome::Ignored
    );
    assert_eq!(
        application.execute(TrayCommand::Quit),
        TrayCommandOutcome::QuitRequested
    );

    crate::finish_presentation_shutdown(&presentation_owner).unwrap();
}

#[test]
fn presentation_creation_failure_prevents_product_startup() {
    let restored = Arc::new(AtomicBool::new(false));
    let restore_flag = restored.clone();
    let mut lifecycle = LifecyclePlan::new(move || {
        restore_flag.store(true, Ordering::Release);
        Ok(())
    })
    .build();
    let state = lifecycle.state();
    let presentation_owner = Arc::new(Mutex::new(None));
    let error = crate::orchestrate_startup(
        &presentation_owner,
        || Err(std::io::Error::other("tray unavailable").into()),
        || lifecycle.start(),
    )
    .err()
    .expect("presentation failure must remain fatal");

    assert_eq!(error.to_string(), "tray unavailable");
    assert!(!restored.load(Ordering::Acquire));
    assert_eq!(
        state.snapshot().status(),
        crate::application::ApplicationStatus::Starting
    );
    assert!(presentation_owner.lock().unwrap().is_none());
}

#[test]
fn background_changes_and_command_reconciliation_render_but_unchanged_ticks_do_not() {
    let source = MutableSource(Arc::new(Mutex::new(view("first"))));
    let (rendered_tx, rendered_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let control = control(command_tx);
    let mut runtime = spawn(
        source.clone(),
        RecordingRenderer(rendered_tx),
        command_rx,
        control.clone(),
        None,
        Duration::from_millis(5),
        |_| {},
    )
    .unwrap();

    assert_eq!(
        rendered_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .primary,
        "first"
    );
    assert_eq!(
        rendered_rx.recv_timeout(Duration::from_millis(30)),
        Err(RecvTimeoutError::Timeout)
    );

    control.request_refresh();
    assert_eq!(
        rendered_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .primary,
        "first"
    );

    source.0.lock().unwrap().primary = "second".to_owned();
    assert_eq!(
        rendered_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .primary,
        "second"
    );
    runtime.shutdown().unwrap();
}

#[test]
fn exit_handshake_preserves_the_first_code_and_allows_only_the_worker_exit() {
    let source = MutableSource(Arc::new(Mutex::new(view("ready"))));
    let (rendered_tx, _rendered_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let control = control(command_tx);
    let (exit_tx, exit_rx) = mpsc::channel();
    let mut runtime = spawn(
        source,
        RecordingRenderer(rendered_tx),
        command_rx,
        control.clone(),
        Some(view("ready")),
        Duration::from_secs(60),
        move |code| exit_tx.send(code).unwrap(),
    )
    .unwrap();

    assert!(control.request_exit(7));
    assert!(control.request_exit(9));
    assert_eq!(exit_rx.recv_timeout(Duration::from_millis(200)).unwrap(), 7);
    assert!(!control.request_exit(11));
    runtime.shutdown().unwrap();
}

#[test]
fn shutdown_joins_the_presentation_owner_promptly() {
    let source = MutableSource(Arc::new(Mutex::new(view("ready"))));
    let (rendered_tx, _rendered_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let control = control(command_tx);
    let mut runtime = spawn(
        source,
        RecordingRenderer(rendered_tx),
        command_rx,
        control,
        Some(view("ready")),
        Duration::from_secs(60),
        |_| {},
    )
    .unwrap();

    runtime.shutdown().unwrap();
    assert!(runtime.worker.is_none());
}

#[test]
fn worker_panic_during_a_requested_exit_cannot_leave_exit_prevented() {
    let source = MutableSource(Arc::new(Mutex::new(view("ready"))));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (proceed_tx, proceed_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let control = control(command_tx);
    let (exit_tx, exit_rx) = mpsc::channel();
    let mut runtime = spawn(
        source,
        BlockingPanicRenderer {
            entered: entered_tx,
            proceed: proceed_rx,
        },
        command_rx,
        control.clone(),
        None,
        Duration::from_millis(1),
        move |code| exit_tx.send(code).unwrap(),
    )
    .unwrap();
    entered_rx.recv_timeout(Duration::from_millis(200)).unwrap();

    assert!(control.request_exit(23));
    proceed_tx.send(()).unwrap();

    assert_eq!(
        exit_rx.recv_timeout(Duration::from_millis(200)).unwrap(),
        23
    );
    assert!(!control.request_exit(24));
    assert!(runtime.shutdown().is_err());
}

#[test]
fn lifecycle_command_gate_change_rebuilds_nested_command_items() {
    let mut starting = view("Starting…");
    starting.commands_enabled = false;
    let ready = view("Ready to keep awake");

    assert_ne!(MenuStructure::from(&starting), MenuStructure::from(&ready));
}

#[test]
fn updater_optional_rows_participate_in_native_menu_rebuilds() {
    let unavailable = view("Ready to keep awake");
    let mut result = unavailable.clone();
    result.updater.result = Some("Up to date".to_owned());
    let mut install = result.clone();
    install.updater.install = Some(("Install Chiù 1.2.3…".to_owned(), true));

    assert_ne!(
        MenuStructure::from(&unavailable),
        MenuStructure::from(&result)
    );
    assert_ne!(MenuStructure::from(&result), MenuStructure::from(&install));
    assert_eq!(
        MenuStructure::from(&install),
        MenuStructure::from(&install.clone())
    );
    assert!(
        !MenuLayout::from(&unavailable)
            .settings()
            .iter()
            .any(|entry| {
                matches!(
                    entry,
                    SettingsItem::Status {
                        id: "updater.result",
                        ..
                    } | SettingsItem::Command {
                        command: TrayCommand::InstallUpdate,
                        ..
                    }
                )
            })
    );
    assert!(MenuLayout::from(&result).settings().iter().any(|entry| {
        matches!(
            entry,
            SettingsItem::Status {
                id: "updater.result",
                ..
            }
        )
    }));
    assert!(MenuLayout::from(&install).settings().iter().any(|entry| {
        matches!(
            entry,
            SettingsItem::Command {
                command: TrayCommand::InstallUpdate,
                ..
            }
        )
    }));
}

#[test]
fn primary_menu_prioritizes_keep_awake_controls_with_settled_copy_and_commands() {
    let mut tray = view("Ready to keep awake");
    tray.detail = Some("Reason: Network activity — 3 MB/s".to_owned());
    tray.warning = Some("Sleep protection needs attention".to_owned());

    assert_eq!(
        MenuLayout::from(&tray).top_level(),
        [
            TopLevelItem::Status {
                id: "status.primary",
                label: "Ready to keep awake".to_owned(),
            },
            TopLevelItem::Status {
                id: "status.detail",
                label: "Reason: Network activity — 3 MB/s".to_owned(),
            },
            TopLevelItem::Status {
                id: "status.warning",
                label: "Sleep protection needs attention".to_owned(),
            },
            TopLevelItem::Separator,
            TopLevelItem::CheckedCommand {
                command: TrayCommand::ToggleAutomaticProtection,
                label: "Keep awake during downloads".to_owned(),
                enabled: true,
                checked: true,
            },
            TopLevelItem::CheckedCommand {
                command: TrayCommand::ToggleProtectOnBattery,
                label: "Keep awake for downloads on battery".to_owned(),
                enabled: true,
                checked: false,
            },
            TopLevelItem::Submenu {
                id: "detection",
                label: "Download detection",
                enabled: true,
            },
            TopLevelItem::Submenu {
                id: "manual",
                label: "Keep awake manually",
                enabled: true,
            },
            TopLevelItem::Separator,
            TopLevelItem::Submenu {
                id: "settings",
                label: "Settings",
                enabled: true,
            },
            TopLevelItem::Command {
                command: TrayCommand::Quit,
                label: "Quit Chiù".to_owned(),
                enabled: true,
            },
        ]
    );
}

#[test]
fn settings_menu_groups_application_actions_and_updater_transient_rows() {
    let mut tray = view("Ready to keep awake");
    tray.updater.automatic_label = "Automatically check for updates".to_owned();
    tray.updater.automatic_enabled = true;
    tray.updater.check_label = "Check for updates…".to_owned();
    tray.updater.check_enabled = true;
    tray.updater.result = Some("Update available — Chiù 1.2.3".to_owned());
    tray.updater.install = Some(("Install Chiù 1.2.3…".to_owned(), true));
    tray.diagnostics.copy_label = "Copy diagnostics".to_owned();
    tray.diagnostics.copy_enabled = true;
    tray.diagnostics.open_logs_label = "Open logs".to_owned();
    tray.diagnostics.open_logs_enabled = true;

    assert_eq!(
        MenuLayout::from(&tray).settings(),
        [
            SettingsItem::CheckedCommand {
                command: TrayCommand::ToggleLaunchAtLogin,
                label: "Launch at login".to_owned(),
                enabled: true,
                checked: true,
            },
            SettingsItem::Separator,
            SettingsItem::CheckedCommand {
                command: TrayCommand::ToggleAutomaticUpdates,
                label: "Automatically check for updates".to_owned(),
                enabled: true,
                checked: true,
            },
            SettingsItem::Command {
                command: TrayCommand::CheckUpdates,
                label: "Check for updates…".to_owned(),
                enabled: true,
            },
            SettingsItem::Status {
                id: "updater.result",
                label: "Update available — Chiù 1.2.3".to_owned(),
            },
            SettingsItem::Command {
                command: TrayCommand::InstallUpdate,
                label: "Install Chiù 1.2.3…".to_owned(),
                enabled: true,
            },
            SettingsItem::Separator,
            SettingsItem::Command {
                command: TrayCommand::CopyDiagnostics,
                label: "Copy diagnostics".to_owned(),
                enabled: true,
            },
            SettingsItem::Command {
                command: TrayCommand::OpenLogs,
                label: "Open logs".to_owned(),
                enabled: true,
            },
            SettingsItem::Separator,
            SettingsItem::About {
                label: "About Chiù".to_owned(),
            },
        ]
    );
}

#[test]
fn settings_remains_navigable_for_about_and_independently_available_diagnostics() {
    let mut tray = view("Startup failed");
    tray.commands_enabled = false;
    tray.launch_at_login.enabled = false;
    tray.updater.automatic_enabled = false;
    tray.updater.check_enabled = false;
    tray.diagnostics.copy_enabled = true;
    tray.diagnostics.open_logs_enabled = true;

    let layout = MenuLayout::from(&tray);
    assert!(layout.top_level().contains(&TopLevelItem::Submenu {
        id: "settings",
        label: "Settings",
        enabled: true,
    }));
    assert!(layout.top_level().contains(&TopLevelItem::CheckedCommand {
        command: TrayCommand::ToggleAutomaticProtection,
        label: "Keep awake during downloads".to_owned(),
        enabled: false,
        checked: true,
    }));
    assert!(layout.settings().contains(&SettingsItem::Command {
        command: TrayCommand::CopyDiagnostics,
        label: tray.diagnostics.copy_label.clone(),
        enabled: true,
    }));
    assert!(layout.settings().contains(&SettingsItem::Command {
        command: TrayCommand::OpenLogs,
        label: tray.diagnostics.open_logs_label.clone(),
        enabled: true,
    }));
    assert!(layout.settings().contains(&SettingsItem::About {
        label: "About Chiù".to_owned(),
    }));

    tray.diagnostics.copy_enabled = false;
    tray.diagnostics.open_logs_enabled = false;
    let shutdown = MenuLayout::from(&tray);
    assert!(shutdown.top_level().contains(&TopLevelItem::Submenu {
        id: "settings",
        label: "Settings",
        enabled: true,
    }));
    assert!(shutdown.settings().contains(&SettingsItem::Command {
        command: TrayCommand::CopyDiagnostics,
        label: tray.diagnostics.copy_label,
        enabled: false,
    }));
    assert!(shutdown.settings().contains(&SettingsItem::Command {
        command: TrayCommand::OpenLogs,
        label: tray.diagnostics.open_logs_label,
        enabled: false,
    }));
}

#[test]
fn every_manual_state_has_the_exact_native_command_shape() {
    let mut tray = view("ready");
    assert_eq!(
        manual_commands(&tray),
        vec![
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::FifteenMinutes,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::ThirtyMinutes,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::OneHour,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::TwoHours,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::FourHours,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::EightHours,
            )),
            TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::UntilDisabled,
            )),
        ]
    );

    tray.keep_awake = KeepAwakeView::Finite {
        remaining: "12 min remaining".to_owned(),
    };
    assert_eq!(
        manual_menu_entries(&tray).first(),
        Some(&ManualMenuEntry::Status {
            label: "12 min remaining".to_owned(),
            dynamic: true,
        })
    );
    assert_eq!(
        manual_commands(&tray),
        vec![
            TrayCommand::Manual(ManualSessionCommand::Stop),
            TrayCommand::Manual(ManualSessionCommand::Add(
                ManualSessionAddPreset::FifteenMinutes,
            )),
            TrayCommand::Manual(ManualSessionCommand::Add(
                ManualSessionAddPreset::ThirtyMinutes,
            )),
            TrayCommand::Manual(ManualSessionCommand::Add(ManualSessionAddPreset::OneHour,)),
            TrayCommand::Manual(ManualSessionCommand::Add(ManualSessionAddPreset::TwoHours,)),
            TrayCommand::Manual(ManualSessionCommand::Add(ManualSessionAddPreset::FourHours,)),
            TrayCommand::Manual(ManualSessionCommand::ConvertToUntilDisabled),
        ]
    );

    tray.keep_awake = KeepAwakeView::UntilDisabled;
    assert_eq!(
        manual_commands(&tray),
        vec![TrayCommand::Manual(ManualSessionCommand::Stop)]
    );
    assert_eq!(
        manual_menu_entries(&tray).first(),
        Some(&ManualMenuEntry::Status {
            label: "Active until disabled".to_owned(),
            dynamic: false,
        })
    );

    tray.commands_enabled = false;
    assert!(
        manual_menu_entries(&tray)
            .iter()
            .all(|entry| { !matches!(entry, ManualMenuEntry::Command { enabled: true, .. }) })
    );
}

#[test]
fn failed_forced_reconciliation_is_retried_on_the_next_tick() {
    let source = MutableSource(Arc::new(Mutex::new(view("ready"))));
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let control = control(command_tx);
    let mut runtime = spawn(
        source,
        FailOnceRenderer {
            attempts: attempt_tx,
            failed: false,
        },
        command_rx,
        control.clone(),
        Some(view("ready")),
        Duration::from_millis(5),
        |_| {},
    )
    .unwrap();

    control.request_refresh();
    assert!(!attempt_rx.recv_timeout(Duration::from_millis(200)).unwrap());
    assert!(attempt_rx.recv_timeout(Duration::from_millis(200)).unwrap());
    runtime.shutdown().unwrap();
}

fn manual_commands(view: &TrayView) -> Vec<TrayCommand> {
    manual_menu_entries(view)
        .into_iter()
        .filter_map(|entry| match entry {
            ManualMenuEntry::Command { command, .. } => Some(command),
            ManualMenuEntry::Status { .. } | ManualMenuEntry::Separator => None,
        })
        .collect()
}
