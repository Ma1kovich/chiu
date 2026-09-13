use crate::tray::{
    ChoiceView, GracePreset, KeepAwakeView, ThresholdPreset, TrayApplication, TrayCommand,
    TrayCommandOutcome, TrayView,
};
use std::{
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tauri::{
    AppHandle,
    menu::{
        CheckMenuItem as TauriCheckMenuItem, Menu as TauriMenu, MenuItem as TauriMenuItem,
        PredefinedMenuItem, Submenu as TauriSubmenu,
    },
    tray::{TrayIcon, TrayIconBuilder},
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

mod manual_menu;
mod menu_layout;
mod settings_menu;

use manual_menu::{ManualMenuEntry, manual_menu_entries};
use menu_layout::{MenuLayout, MenuLayoutShape, MenuToken, TopLevelItem};
use settings_menu::SettingsMenu;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const TRAY_ICON_ID: &str = "chiu-tray";
type BoxError = Box<dyn Error + Send + Sync>;
type CheckMenuItem = TauriCheckMenuItem<tauri::Wry>;
type Menu = TauriMenu<tauri::Wry>;
type MenuItem = TauriMenuItem<tauri::Wry>;
type Submenu = TauriSubmenu<tauri::Wry>;

trait ViewSource: Send + 'static {
    fn view(&self) -> TrayView;
}

impl ViewSource for TrayApplication {
    fn view(&self) -> TrayView {
        self.view()
    }
}

trait Renderer: Send + 'static {
    fn render(&mut self, view: &TrayView) -> Result<(), BoxError>;
}

pub(crate) trait CopyDiagnosticsErrorPresenter: Send + Sync + 'static {
    fn show_copy_failed(&self);
}

struct TauriCopyDiagnosticsErrorPresenter {
    app: AppHandle,
}

impl CopyDiagnosticsErrorPresenter for TauriCopyDiagnosticsErrorPresenter {
    fn show_copy_failed(&self) {
        self.app
            .dialog()
            .message("Chiù could not copy diagnostics. Please try again.")
            .title("Chiù Diagnostics")
            .kind(MessageDialogKind::Error)
            .show(|_| {});
    }
}

pub(crate) fn register_copy_diagnostics_error_presenter(
    app: &AppHandle,
) -> Option<Arc<dyn CopyDiagnosticsErrorPresenter>> {
    app.plugin(tauri_plugin_dialog::init()).ok()?;
    Some(Arc::new(TauriCopyDiagnosticsErrorPresenter {
        app: app.clone(),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitPhase {
    Running,
    StopRequested(i32),
    Stopped,
}

enum WorkerCommand {
    Refresh,
    Stop,
    StopAndExit(i32),
}

#[derive(Clone)]
pub(crate) struct PresentationControl {
    sender: mpsc::Sender<WorkerCommand>,
    refresh_pending: Arc<AtomicBool>,
    exit_phase: Arc<Mutex<ExitPhase>>,
}

impl PresentationControl {
    pub(crate) fn request_refresh(&self) {
        if self
            .refresh_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && self.sender.send(WorkerCommand::Refresh).is_err()
        {
            self.refresh_pending.store(false, Ordering::Release);
        }
    }

    /// Returns true while the ordinary exit must be prevented so the main loop can
    /// service any in-flight native menu operation.
    pub(crate) fn request_exit(&self, code: i32) -> bool {
        let mut phase = self
            .exit_phase
            .lock()
            .expect("presentation exit lock poisoned");
        match *phase {
            ExitPhase::Running => {
                *phase = ExitPhase::StopRequested(code);
                if self.sender.send(WorkerCommand::StopAndExit(code)).is_ok() {
                    true
                } else {
                    *phase = ExitPhase::Stopped;
                    false
                }
            }
            ExitPhase::StopRequested(_) => true,
            ExitPhase::Stopped => false,
        }
    }
}

pub(crate) struct PresentationRuntime {
    control: PresentationControl,
    worker: Option<JoinHandle<()>>,
}

impl PresentationRuntime {
    pub(crate) fn control(&self) -> PresentationControl {
        self.control.clone()
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), BoxError> {
        if let Some(worker) = self.worker.take() {
            let _ = self.control.sender.send(WorkerCommand::Stop);
            worker.join().map_err(|_| {
                Box::new(std::io::Error::other("tray presentation worker panicked")) as BoxError
            })?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_shutdown_test(on_shutdown: impl FnOnce() + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let control = PresentationControl {
            sender,
            refresh_pending: Arc::new(AtomicBool::new(false)),
            exit_phase: Arc::new(Mutex::new(ExitPhase::Running)),
        };
        let worker = thread::spawn(move || {
            let mut on_shutdown = Some(on_shutdown);
            while let Ok(command) = receiver.recv() {
                match command {
                    WorkerCommand::Refresh => {}
                    WorkerCommand::Stop | WorkerCommand::StopAndExit(_) => {
                        if let Some(on_shutdown) = on_shutdown.take() {
                            on_shutdown();
                        }
                        break;
                    }
                }
            }
        });
        Self {
            control,
            worker: Some(worker),
        }
    }
}

pub(crate) fn start(
    app: &AppHandle,
    application: TrayApplication,
    copy_error_presenter: Option<Arc<dyn CopyDiagnosticsErrorPresenter>>,
) -> Result<PresentationRuntime, BoxError> {
    let (sender, receiver) = mpsc::channel();
    let control = PresentationControl {
        sender,
        refresh_pending: Arc::new(AtomicBool::new(false)),
        exit_phase: Arc::new(Mutex::new(ExitPhase::Running)),
    };
    let initial_view = application.view();
    let native_menu = NativeMenu::build(app, &initial_view)?;
    let callback_application = application.clone();
    let callback_control = control.clone();
    let callback_copy_error_presenter = copy_error_presenter;
    #[cfg(target_os = "macos")]
    let tray_icon = tauri::include_image!("./icons/chiu-tray-macos-template.png");
    #[cfg(target_os = "windows")]
    let tray_icon = tauri::include_image!("./icons/chiu-tray-windows.png");
    let tray = TrayIconBuilder::with_id(TRAY_ICON_ID)
        .icon(tray_icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .menu(&native_menu.menu)
        .on_menu_event(move |app, event| {
            let outcome = TrayCommand::from_id(event.id.as_ref())
                .map_or(TrayCommandOutcome::Ignored, |command| {
                    callback_application.execute(command)
                });
            dispatch_command_outcome(
                outcome,
                || callback_control.request_refresh(),
                || app.exit(0),
                callback_copy_error_presenter.as_deref(),
            );
        })
        .build(app)?;
    let renderer = NativeRenderer {
        app: app.clone(),
        tray,
        menu: native_menu,
        view: initial_view.clone(),
    };
    spawn(
        application,
        renderer,
        receiver,
        control,
        Some(initial_view),
        REFRESH_INTERVAL,
        {
            let app = app.clone();
            move |code| app.exit(code)
        },
    )
}

fn dispatch_command_outcome(
    outcome: TrayCommandOutcome,
    request_refresh: impl FnOnce(),
    request_exit: impl FnOnce(),
    copy_error_presenter: Option<&dyn CopyDiagnosticsErrorPresenter>,
) {
    match outcome {
        TrayCommandOutcome::RefreshRequested => request_refresh(),
        TrayCommandOutcome::QuitRequested => request_exit(),
        TrayCommandOutcome::CopyDiagnosticsFailed => {
            if let Some(presenter) = copy_error_presenter {
                presenter.show_copy_failed();
            }
        }
        TrayCommandOutcome::Ignored => {}
    }
}

fn spawn<S, R, E>(
    source: S,
    renderer: R,
    receiver: mpsc::Receiver<WorkerCommand>,
    control: PresentationControl,
    last_view: Option<TrayView>,
    interval: Duration,
    request_exit: E,
) -> Result<PresentationRuntime, BoxError>
where
    S: ViewSource,
    R: Renderer,
    E: Fn(i32) + Send + 'static,
{
    let worker_control = control.clone();
    let worker = thread::Builder::new()
        .name("chiu-tray-presentation".to_owned())
        .spawn(move || {
            run(
                source,
                renderer,
                receiver,
                &worker_control,
                last_view,
                interval,
                request_exit,
            );
        })
        .map_err(|error| Box::new(error) as BoxError)?;
    Ok(PresentationRuntime {
        control,
        worker: Some(worker),
    })
}

fn run<S, R, E>(
    source: S,
    mut renderer: R,
    receiver: mpsc::Receiver<WorkerCommand>,
    control: &PresentationControl,
    mut last_view: Option<TrayView>,
    interval: Duration,
    request_exit: E,
) where
    S: ViewSource,
    R: Renderer,
    E: Fn(i32),
{
    let _exit_guard = WorkerExitGuard {
        control,
        request_exit: &request_exit,
    };
    loop {
        let command = receiver.recv_timeout(interval);
        let force_render = match command {
            Ok(WorkerCommand::Refresh) => {
                control.refresh_pending.store(false, Ordering::Release);
                true
            }
            Ok(WorkerCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(WorkerCommand::StopAndExit(code)) => {
                *control
                    .exit_phase
                    .lock()
                    .expect("presentation exit lock poisoned") = ExitPhase::Stopped;
                request_exit(code);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
        };

        let view = source.view();
        if !force_render && last_view.as_ref() == Some(&view) {
            continue;
        }
        match renderer.render(&view) {
            Ok(()) => last_view = Some(view),
            Err(error) => {
                last_view = None;
                eprintln!("Chiù could not update its tray menu: {error}");
            }
        }
    }
}

struct WorkerExitGuard<'a, E: Fn(i32)> {
    control: &'a PresentationControl,
    request_exit: &'a E,
}

impl<E: Fn(i32)> Drop for WorkerExitGuard<'_, E> {
    fn drop(&mut self) {
        let exit_code = {
            let mut phase = self
                .control
                .exit_phase
                .lock()
                .expect("presentation exit lock poisoned");
            let exit_code = match *phase {
                ExitPhase::StopRequested(code) => Some(code),
                ExitPhase::Running | ExitPhase::Stopped => None,
            };
            *phase = ExitPhase::Stopped;
            exit_code
        };
        if let Some(code) = exit_code {
            (self.request_exit)(code);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MenuStructure {
    commands_enabled: bool,
    layout: MenuLayoutShape,
    keep_awake: KeepAwakeShape,
    receive_rate: bool,
    custom_threshold: bool,
    custom_grace: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeepAwakeShape {
    Inactive,
    Finite,
    UntilDisabled,
}

impl From<&TrayView> for MenuStructure {
    fn from(view: &TrayView) -> Self {
        let layout = MenuLayout::from(view);
        Self {
            commands_enabled: view.commands_enabled,
            layout: layout.shape(),
            keep_awake: match view.keep_awake {
                KeepAwakeView::Inactive => KeepAwakeShape::Inactive,
                KeepAwakeView::Finite { .. } => KeepAwakeShape::Finite,
                KeepAwakeView::UntilDisabled => KeepAwakeShape::UntilDisabled,
            },
            receive_rate: view.detection.receive_rate.is_some(),
            custom_threshold: view.detection.threshold.custom.is_some(),
            custom_grace: view.detection.grace.custom.is_some(),
        }
    }
}

struct NativeRenderer {
    app: AppHandle,
    tray: TrayIcon,
    menu: NativeMenu,
    view: TrayView,
}

impl Renderer for NativeRenderer {
    fn render(&mut self, view: &TrayView) -> Result<(), BoxError> {
        if MenuStructure::from(&self.view) != MenuStructure::from(view) {
            let menu = NativeMenu::build(&self.app, view)?;
            self.tray.set_menu(Some(menu.menu.clone()))?;
            self.menu = menu;
        } else {
            self.menu.update(view)?;
        }
        self.view = view.clone();
        Ok(())
    }
}

struct NativeMenu {
    menu: Menu,
    primary: MenuItem,
    detail: Option<MenuItem>,
    warning: Option<MenuItem>,
    automatic: CheckMenuItem,
    keep_awake: Submenu,
    manual_remaining: Option<MenuItem>,
    detection: Submenu,
    detection_state: MenuItem,
    receive_rate: Option<MenuItem>,
    power_source: MenuItem,
    threshold_menu: Submenu,
    threshold_presets: [CheckMenuItem; 3],
    custom_threshold: Option<CheckMenuItem>,
    grace_menu: Submenu,
    grace_presets: [CheckMenuItem; 3],
    custom_grace: Option<CheckMenuItem>,
    battery: CheckMenuItem,
    settings: SettingsMenu,
    quit: MenuItem,
}

impl NativeMenu {
    fn build(app: &AppHandle, view: &TrayView) -> tauri::Result<Self> {
        let layout = MenuLayout::from(view);
        let menu = Menu::new(app)?;
        let settings = SettingsMenu::build(app, &layout)?;
        let mut primary = None;
        let mut detail = None;
        let mut warning = None;
        let mut automatic = None;
        let mut battery = None;
        let mut keep_awake = None;
        let mut detection = None;
        let mut quit = None;

        for entry in layout.top_level() {
            match entry {
                TopLevelItem::Status { id, label } => {
                    let item = disabled_item(app, id, label)?;
                    menu.append(&item)?;
                    match *id {
                        "status.primary" => primary = Some(item),
                        "status.detail" => detail = Some(item),
                        "status.warning" => warning = Some(item),
                        _ => return Err(unexpected_menu_entry(id)),
                    }
                }
                TopLevelItem::Separator => {
                    menu.append(&PredefinedMenuItem::separator(app)?)?;
                }
                TopLevelItem::CheckedCommand {
                    command,
                    label,
                    enabled,
                    checked,
                } => {
                    let item = CheckMenuItem::with_id(
                        app,
                        command.id(),
                        label,
                        *enabled,
                        *checked,
                        None::<&str>,
                    )?;
                    menu.append(&item)?;
                    match command {
                        TrayCommand::ToggleAutomaticProtection => automatic = Some(item),
                        TrayCommand::ToggleProtectOnBattery => battery = Some(item),
                        _ => return Err(unexpected_menu_entry(command.id())),
                    }
                }
                TopLevelItem::Submenu { id, label, enabled } => match *id {
                    "detection" => {
                        let submenu = build_detection(app, view, label, *enabled)?;
                        menu.append(&submenu.submenu)?;
                        detection = Some(submenu);
                    }
                    "manual" => {
                        let submenu = build_keep_awake(app, view, label, *enabled)?;
                        menu.append(&submenu.0)?;
                        keep_awake = Some(submenu);
                    }
                    "settings" => menu.append(settings.submenu())?,
                    _ => return Err(unexpected_menu_entry(id)),
                },
                TopLevelItem::Command {
                    command,
                    label,
                    enabled,
                } => {
                    let item = command_item(app, *command, label, *enabled)?;
                    menu.append(&item)?;
                    if *command == TrayCommand::Quit {
                        quit = Some(item);
                    } else {
                        return Err(unexpected_menu_entry(command.id()));
                    }
                }
            }
        }

        let (keep_awake, manual_remaining) = required_menu(keep_awake, "manual submenu")?;
        let detection = required_menu(detection, "detection submenu")?;

        let native = Self {
            menu,
            primary: required_menu(primary, "primary status")?,
            detail,
            warning,
            automatic: required_menu(automatic, "automatic protection")?,
            keep_awake,
            manual_remaining,
            detection: detection.submenu,
            detection_state: detection.state,
            receive_rate: detection.receive_rate,
            power_source: detection.power_source,
            threshold_menu: detection.threshold_menu,
            threshold_presets: detection.threshold_presets,
            custom_threshold: detection.custom_threshold,
            grace_menu: detection.grace_menu,
            grace_presets: detection.grace_presets,
            custom_grace: detection.custom_grace,
            battery: required_menu(battery, "battery protection")?,
            settings,
            quit: required_menu(quit, "Quit Chiù")?,
        };
        native.verify_action_access(view)?;
        Ok(native)
    }

    fn update(&self, view: &TrayView) -> tauri::Result<()> {
        let layout = MenuLayout::from(view);
        for entry in layout.top_level() {
            match entry {
                TopLevelItem::Status { id, label } => match *id {
                    "status.primary" => self.primary.set_text(label)?,
                    "status.detail" => {
                        required_menu_ref(self.detail.as_ref(), "status detail")?
                            .set_text(label)?;
                    }
                    "status.warning" => {
                        required_menu_ref(self.warning.as_ref(), "status warning")?
                            .set_text(label)?;
                    }
                    _ => return Err(unexpected_menu_entry(id)),
                },
                TopLevelItem::Separator => {}
                TopLevelItem::CheckedCommand {
                    command,
                    label,
                    enabled,
                    checked,
                } => {
                    let item = match command {
                        TrayCommand::ToggleAutomaticProtection => &self.automatic,
                        TrayCommand::ToggleProtectOnBattery => &self.battery,
                        _ => return Err(unexpected_menu_entry(command.id())),
                    };
                    item.set_text(label)?;
                    item.set_enabled(*enabled)?;
                    item.set_checked(*checked)?;
                }
                TopLevelItem::Submenu { id, label, enabled } => match *id {
                    "detection" => {
                        self.detection.set_text(label)?;
                        self.detection.set_enabled(*enabled)?;
                    }
                    "manual" => {
                        self.keep_awake.set_text(label)?;
                        self.keep_awake.set_enabled(*enabled)?;
                    }
                    "settings" => {}
                    _ => return Err(unexpected_menu_entry(id)),
                },
                TopLevelItem::Command {
                    command,
                    label,
                    enabled,
                } => {
                    if *command != TrayCommand::Quit {
                        return Err(unexpected_menu_entry(command.id()));
                    }
                    self.quit.set_text(label)?;
                    self.quit.set_enabled(*enabled)?;
                }
            }
        }
        if let (Some(item), KeepAwakeView::Finite { remaining }) =
            (&self.manual_remaining, &view.keep_awake)
        {
            item.set_text(remaining)?;
        }
        self.detection_state.set_text(&view.detection.state)?;
        if let (Some(item), Some(rate)) = (&self.receive_rate, &view.detection.receive_rate) {
            item.set_text(format!("Receive rate — {rate}"))?;
        }
        self.power_source
            .set_text(format!("Power source — {}", view.detection.power_source))?;
        self.threshold_menu.set_enabled(view.commands_enabled)?;
        update_choices(
            &self.threshold_presets,
            self.custom_threshold.as_ref(),
            &view.detection.threshold,
        )?;
        self.grace_menu.set_enabled(view.commands_enabled)?;
        update_choices(
            &self.grace_presets,
            self.custom_grace.as_ref(),
            &view.detection.grace,
        )?;
        self.settings.update(&layout)?;
        self.verify_action_access(view)?;
        Ok(())
    }

    fn verify_action_access(&self, view: &TrayView) -> tauri::Result<()> {
        let layout = MenuLayout::from(view);
        let product_enabled = view.commands_enabled;
        ensure_native_access(
            "automatic download protection",
            self.automatic.is_enabled()?,
            product_enabled,
        )?;
        ensure_native_access(
            "automatic download protection checked state",
            self.automatic.is_checked()?,
            view.automatic_checked,
        )?;
        ensure_native_access(
            "manual keep awake",
            self.keep_awake.is_enabled()?,
            product_enabled,
        )?;
        for item in self.keep_awake.items()? {
            if let Some(command) = item.as_menuitem() {
                let id = command.id().as_ref();
                if id != "manual.remaining" {
                    ensure_native_access(id, command.is_enabled()?, product_enabled)?;
                }
            }
        }
        ensure_native_access(
            "detection threshold",
            self.threshold_menu.is_enabled()?,
            product_enabled,
        )?;
        for item in &self.threshold_presets {
            ensure_native_access(item.id().as_ref(), item.is_enabled()?, product_enabled)?;
        }
        ensure_native_access(
            "detection grace",
            self.grace_menu.is_enabled()?,
            product_enabled,
        )?;
        for item in &self.grace_presets {
            ensure_native_access(item.id().as_ref(), item.is_enabled()?, product_enabled)?;
        }
        ensure_native_access(
            "protect on battery",
            self.battery.is_enabled()?,
            product_enabled,
        )?;
        ensure_native_access(
            "protect on battery checked state",
            self.battery.is_checked()?,
            view.protect_on_battery_checked,
        )?;
        ensure_native_access("quit", self.quit.is_enabled()?, true)?;

        let actual = self
            .menu
            .items()?
            .iter()
            .map(|item| {
                if item.as_predefined_menuitem().is_some() {
                    MenuToken::Separator
                } else {
                    MenuToken::Item(item.id().as_ref().to_owned())
                }
            })
            .collect::<Vec<_>>();
        ensure_native_access(
            "top-level menu ordering and grouping",
            actual == layout.top_level_tokens(),
            true,
        )
    }
}

fn ensure_native_access(name: &str, actual: bool, expected: bool) -> tauri::Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "native menu access for {name} was {actual}, expected {expected}"
        ))
        .into())
    }
}

fn disabled_item(app: &AppHandle, id: &str, text: &str) -> tauri::Result<MenuItem> {
    MenuItem::with_id(app, id, text, false, None::<&str>)
}

fn command_item(
    app: &AppHandle,
    command: TrayCommand,
    text: &str,
    enabled: bool,
) -> tauri::Result<MenuItem> {
    MenuItem::with_id(app, command.id(), text, enabled, None::<&str>)
}

fn required_menu<T>(item: Option<T>, name: &str) -> tauri::Result<T> {
    item.ok_or_else(|| std::io::Error::other(format!("missing native menu item: {name}")).into())
}

fn required_menu_ref<'a, T>(item: Option<&'a T>, name: &str) -> tauri::Result<&'a T> {
    item.ok_or_else(|| std::io::Error::other(format!("missing native menu item: {name}")).into())
}

fn unexpected_menu_entry(id: &str) -> tauri::Error {
    std::io::Error::other(format!("unexpected top-level menu entry: {id}")).into()
}

fn build_keep_awake(
    app: &AppHandle,
    view: &TrayView,
    label: &str,
    enabled: bool,
) -> tauri::Result<(Submenu, Option<MenuItem>)> {
    let submenu = Submenu::with_id(app, "manual", label, enabled)?;
    let mut dynamic_remaining = None;
    for entry in manual_menu_entries(view) {
        match entry {
            ManualMenuEntry::Status { label, dynamic } => {
                let item = disabled_item(app, "manual.remaining", &label)?;
                submenu.append(&item)?;
                if dynamic {
                    dynamic_remaining = Some(item);
                }
            }
            ManualMenuEntry::Command {
                command,
                label,
                enabled,
            } => {
                submenu.append(&command_item(app, command, label, enabled)?)?;
            }
            ManualMenuEntry::Separator => {
                submenu.append(&PredefinedMenuItem::separator(app)?)?;
            }
        }
    }
    Ok((submenu, dynamic_remaining))
}

struct DetectionMenu {
    submenu: Submenu,
    state: MenuItem,
    receive_rate: Option<MenuItem>,
    power_source: MenuItem,
    threshold_menu: Submenu,
    threshold_presets: [CheckMenuItem; 3],
    custom_threshold: Option<CheckMenuItem>,
    grace_menu: Submenu,
    grace_presets: [CheckMenuItem; 3],
    custom_grace: Option<CheckMenuItem>,
}

fn build_detection(
    app: &AppHandle,
    view: &TrayView,
    label: &str,
    enabled: bool,
) -> tauri::Result<DetectionMenu> {
    let submenu = Submenu::with_id(app, "detection", label, enabled)?;
    let state = disabled_item(app, "detection.state", &view.detection.state)?;
    submenu.append(&state)?;
    let receive_rate = view
        .detection
        .receive_rate
        .as_ref()
        .map(|rate| disabled_item(app, "detection.rate", &format!("Receive rate — {rate}")))
        .transpose()?;
    if let Some(rate) = &receive_rate {
        submenu.append(rate)?;
    }
    let power_source = disabled_item(
        app,
        "detection.power",
        &format!("Power source — {}", view.detection.power_source),
    )?;
    submenu.append(&power_source)?;
    submenu.append(&PredefinedMenuItem::separator(app)?)?;

    let (threshold_menu, threshold_presets, custom_threshold) = build_choices(
        app,
        "detection.threshold",
        "Threshold",
        [
            TrayCommand::SetThreshold(ThresholdPreset::Kibibytes256),
            TrayCommand::SetThreshold(ThresholdPreset::Mebibyte1),
            TrayCommand::SetThreshold(ThresholdPreset::Mebibytes5),
        ],
        ["256 KB/s", "1 MB/s", "5 MB/s"],
        &view.detection.threshold,
        view.commands_enabled,
    )?;
    submenu.append(&threshold_menu)?;
    let (grace_menu, grace_presets, custom_grace) = build_choices(
        app,
        "detection.grace",
        "Grace period",
        [
            TrayCommand::SetGrace(GracePreset::Seconds30),
            TrayCommand::SetGrace(GracePreset::Minutes2),
            TrayCommand::SetGrace(GracePreset::Minutes5),
        ],
        ["30 seconds", "2 minutes", "5 minutes"],
        &view.detection.grace,
        view.commands_enabled,
    )?;
    submenu.append(&grace_menu)?;

    Ok(DetectionMenu {
        submenu,
        state,
        receive_rate,
        power_source,
        threshold_menu,
        threshold_presets,
        custom_threshold,
        grace_menu,
        grace_presets,
        custom_grace,
    })
}

fn build_choices(
    app: &AppHandle,
    id: &str,
    label: &str,
    commands: [TrayCommand; 3],
    labels: [&str; 3],
    view: &ChoiceView,
    enabled: bool,
) -> tauri::Result<(Submenu, [CheckMenuItem; 3], Option<CheckMenuItem>)> {
    let submenu = Submenu::with_id(app, id, label, enabled)?;
    let preset = |index: usize| {
        CheckMenuItem::with_id(
            app,
            commands[index].id(),
            labels[index],
            enabled,
            view.checked[index],
            None::<&str>,
        )
    };
    let presets = [preset(0)?, preset(1)?, preset(2)?];
    for item in &presets {
        submenu.append(item)?;
    }
    let custom = view
        .custom
        .as_ref()
        .map(|text| {
            CheckMenuItem::with_id(app, format!("{id}.custom"), text, false, true, None::<&str>)
        })
        .transpose()?;
    if let Some(item) = &custom {
        submenu.append(item)?;
    }
    Ok((submenu, presets, custom))
}

fn update_choices(
    presets: &[CheckMenuItem; 3],
    custom: Option<&CheckMenuItem>,
    view: &ChoiceView,
) -> tauri::Result<()> {
    for (item, checked) in presets.iter().zip(view.checked) {
        item.set_checked(checked)?;
    }
    if let (Some(item), Some(text)) = (custom, &view.custom) {
        item.set_text(text)?;
        item.set_checked(true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
