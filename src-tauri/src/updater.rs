use crate::{
    local_log::{EventOutcome, LocalLog, SafeEvent},
    settings::{SettingsService, SettingsUpdate},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use worker_exit::{WorkerCompletion, WorkerExit, WorkerExitState};

const AUTOMATIC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateAvailability {
    Available,
    Unconfigured,
    InitializationFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateTrigger {
    Automatic,
    Manual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateFailureStage {
    Scheduling,
    Check,
    Download,
    Verification,
    Install,
    Restart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UpdateStatus {
    Idle,
    Checking(UpdateTrigger),
    UpToDate,
    UpdateAvailable {
        version: Option<String>,
    },
    AwaitingConfirmation {
        version: Option<String>,
    },
    Downloading {
        version: Option<String>,
        percent: Option<u8>,
    },
    Installing {
        version: Option<String>,
    },
    Failed {
        stage: UpdateFailureStage,
        trigger: UpdateTrigger,
        pending: Option<PendingUpdateView>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PendingUpdateView {
    Generic,
    Version(String),
}

impl PendingUpdateView {
    pub(crate) fn version(&self) -> Option<&str> {
        match self {
            Self::Generic => None,
            Self::Version(version) => Some(version),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdaterSnapshot {
    pub(crate) availability: UpdateAvailability,
    pub(crate) automatic_checks_enabled: bool,
    pub(crate) status: UpdateStatus,
}

impl UpdaterSnapshot {
    pub(crate) fn user_failure(&self) -> Option<UpdateFailureStage> {
        match self.status {
            UpdateStatus::Failed {
                stage,
                trigger: UpdateTrigger::Manual,
                ..
            } => Some(stage),
            _ => None,
        }
    }

    pub(crate) fn background_failure(&self) -> Option<UpdateFailureStage> {
        match self.status {
            UpdateStatus::Failed {
                stage,
                trigger: UpdateTrigger::Automatic,
                ..
            } => Some(stage),
            _ if self.availability == UpdateAvailability::InitializationFailed => {
                Some(UpdateFailureStage::Check)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdaterInitializationFailure;

impl std::fmt::Display for UpdaterInitializationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("configured updater could not be initialized")
    }
}

impl std::error::Error for UpdaterInitializationFailure {}

trait Clock: Send + Sync {
    fn unix_seconds(&self) -> u64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

trait PendingUpdate: Send {
    fn announced_version(&self) -> &str;

    fn download(
        &mut self,
        progress: &mut dyn FnMut(usize, Option<u64>),
        download_finished: &mut dyn FnMut(),
    ) -> Result<Vec<u8>, UpdateFailureStage>;

    fn install(&mut self, bytes: Vec<u8>) -> Result<(), UpdateFailureStage>;
}

trait UpdateBackend: Send {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()>;
}

trait UpdatePresentation: Send + Sync {
    fn information(&self, message: &'static str);
    fn error(&self, message: &'static str);
    fn confirm_install(&self, version: Option<String>, callback: Box<dyn FnOnce(bool) + Send>);
}

trait InstalledUpdate: Send + Sync {
    fn finish(&self) -> Result<(), ()>;
}

#[cfg(test)]
struct ShutdownTestAdapter;

#[cfg(test)]
impl UpdateBackend for ShutdownTestAdapter {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()> {
        Ok(None)
    }
}

#[cfg(test)]
impl UpdatePresentation for ShutdownTestAdapter {
    fn information(&self, _: &'static str) {}

    fn error(&self, _: &'static str) {}

    fn confirm_install(&self, _: Option<String>, _: Box<dyn FnOnce(bool) + Send>) {}
}

#[cfg(test)]
impl InstalledUpdate for ShutdownTestAdapter {
    fn finish(&self) -> Result<(), ()> {
        Ok(())
    }
}

enum WorkerCommand {
    Activate,
    ToggleAutomaticChecks,
    ManualCheck,
    RequestInstall,
    Confirmation(bool),
    #[cfg(test)]
    Synchronize(mpsc::Sender<()>),
    Stop,
}

#[derive(Clone)]
pub(crate) struct UpdaterService {
    snapshot: Arc<Mutex<UpdaterSnapshot>>,
    sender: Option<mpsc::Sender<WorkerCommand>>,
    accepting: Arc<AtomicBool>,
    operation_claimed: Arc<AtomicBool>,
}

impl UpdaterService {
    pub(crate) fn unconfigured(settings: &SettingsService) -> Self {
        Self::unavailable(UpdateAvailability::Unconfigured, settings)
    }

    pub(crate) fn initialization_failed(settings: &SettingsService) -> Self {
        Self::unavailable(UpdateAvailability::InitializationFailed, settings)
    }

    fn unavailable(availability: UpdateAvailability, settings: &SettingsService) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(UpdaterSnapshot {
                availability,
                automatic_checks_enabled: settings.snapshot().automatic_update_checks_enabled(),
                status: UpdateStatus::Idle,
            })),
            sender: None,
            accepting: Arc::new(AtomicBool::new(false)),
            operation_claimed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn available(
        settings: SettingsService,
        clock: Arc<dyn Clock>,
        backend: Box<dyn UpdateBackend>,
        presentation: Arc<dyn UpdatePresentation>,
        installed_update: Arc<dyn InstalledUpdate>,
        log: LocalLog,
    ) -> Result<(Self, UpdaterRuntime), std::io::Error> {
        Self::available_with_exit(
            settings,
            clock,
            backend,
            presentation,
            installed_update,
            log,
            Arc::new(WorkerExit::running()),
        )
    }

    fn available_with_exit(
        settings: SettingsService,
        clock: Arc<dyn Clock>,
        backend: Box<dyn UpdateBackend>,
        presentation: Arc<dyn UpdatePresentation>,
        installed_update: Arc<dyn InstalledUpdate>,
        log: LocalLog,
        worker_exit: Arc<WorkerExit>,
    ) -> Result<(Self, UpdaterRuntime), std::io::Error> {
        let snapshot = Arc::new(Mutex::new(UpdaterSnapshot {
            availability: UpdateAvailability::Available,
            automatic_checks_enabled: settings.snapshot().automatic_update_checks_enabled(),
            status: UpdateStatus::Idle,
        }));
        let accepting = Arc::new(AtomicBool::new(true));
        let operation_claimed = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let worker_snapshot = snapshot.clone();
        let worker_accepting = accepting.clone();
        let worker_operation = operation_claimed.clone();
        let worker_completion = WorkerCompletion(worker_exit.clone());
        let confirmation_sender = sender.clone();
        let worker = thread::Builder::new()
            .name("chiu-updater".to_owned())
            .spawn(move || {
                let _completion = worker_completion;
                UpdaterWorker {
                    settings,
                    clock,
                    backend,
                    presentation,
                    installed_update,
                    receiver,
                    confirmation_sender,
                    snapshot: worker_snapshot,
                    accepting: worker_accepting,
                    operation_claimed: worker_operation,
                    log,
                    state: WorkerState {
                        pending: None,
                        next_automatic: None,
                    },
                }
                .run();
            })?;
        Ok((
            Self {
                snapshot,
                sender: Some(sender.clone()),
                accepting: accepting.clone(),
                operation_claimed,
            },
            UpdaterRuntime {
                accepting,
                sender: Some(sender),
                worker: Some(worker),
                worker_exit,
            },
        ))
    }

    pub(crate) fn snapshot(&self) -> UpdaterSnapshot {
        self.snapshot
            .lock()
            .expect("updater snapshot lock poisoned")
            .clone()
    }

    pub(crate) fn activate(&self) {
        self.send(WorkerCommand::Activate);
    }

    pub(crate) fn toggle_automatic_checks(&self) {
        self.send(WorkerCommand::ToggleAutomaticChecks);
    }

    pub(crate) fn check_manually(&self) {
        if self.claim_operation() {
            self.send_claimed(WorkerCommand::ManualCheck);
        }
    }

    pub(crate) fn request_install(&self) {
        let installable = matches!(
            self.snapshot().status,
            UpdateStatus::UpdateAvailable { .. }
                | UpdateStatus::Failed {
                    pending: Some(_),
                    ..
                }
        );
        if installable && self.claim_operation() {
            self.send_claimed(WorkerCommand::RequestInstall);
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn synchronize(&self) {
        let (acknowledge, acknowledged) = mpsc::channel();
        self.sender
            .as_ref()
            .expect("available updater has a command sender")
            .send(WorkerCommand::Synchronize(acknowledge))
            .expect("updater worker accepts synchronization command");
        acknowledged
            .recv_timeout(Duration::from_secs(1))
            .expect("updater worker acknowledges synchronization command");
    }

    fn send(&self, command: WorkerCommand) {
        if self.accepting.load(Ordering::Acquire)
            && let Some(sender) = &self.sender
        {
            let _ = sender.send(command);
        }
    }

    fn claim_operation(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
            && self
                .operation_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    fn send_claimed(&self, command: WorkerCommand) {
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(command).is_err())
        {
            self.operation_claimed.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
pub(crate) fn shutdown_test_registration() -> (UpdaterService, UpdaterRuntime) {
    let log = LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open);
    UpdaterService::available(
        SettingsService::unavailable(log.clone()),
        Arc::new(SystemClock),
        Box::new(ShutdownTestAdapter),
        Arc::new(ShutdownTestAdapter),
        Arc::new(ShutdownTestAdapter),
        log,
    )
    .expect("shutdown test updater worker starts")
}

pub(crate) struct UpdaterRuntime {
    accepting: Arc<AtomicBool>,
    sender: Option<mpsc::Sender<WorkerCommand>>,
    worker: Option<thread::JoinHandle<()>>,
    worker_exit: Arc<WorkerExit>,
}

impl UpdaterRuntime {
    pub(crate) fn empty() -> Self {
        Self {
            accepting: Arc::new(AtomicBool::new(false)),
            sender: None,
            worker: None,
            worker_exit: Arc::new(WorkerExit::finished()),
        }
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), std::io::Error> {
        self.accepting.store(false, Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(WorkerCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            if worker.thread().id() == thread::current().id() {
                return Ok(());
            }
            match self.worker_exit.wait_until_shutdown_can_continue() {
                WorkerExitState::Finished => worker
                    .join()
                    .map_err(|_| std::io::Error::other("updater worker panicked"))?,
                #[cfg(any(test, target_os = "windows"))]
                WorkerExitState::NativeExitPending => drop(worker),
                WorkerExitState::Running => unreachable!("worker exit wait returned while running"),
            }
        }
        Ok(())
    }
}

struct WorkerState {
    pending: Option<Box<dyn PendingUpdate>>,
    next_automatic: Option<Instant>,
}

struct UpdaterWorker {
    settings: SettingsService,
    clock: Arc<dyn Clock>,
    backend: Box<dyn UpdateBackend>,
    presentation: Arc<dyn UpdatePresentation>,
    installed_update: Arc<dyn InstalledUpdate>,
    receiver: mpsc::Receiver<WorkerCommand>,
    confirmation_sender: mpsc::Sender<WorkerCommand>,
    snapshot: Arc<Mutex<UpdaterSnapshot>>,
    accepting: Arc<AtomicBool>,
    operation_claimed: Arc<AtomicBool>,
    log: LocalLog,
    state: WorkerState,
}

impl UpdaterWorker {
    fn run(mut self) {
        while let Some(command) = self.receive() {
            match command {
                WorkerCommand::Activate if self.is_accepting() => self.attempt_automatic(),
                WorkerCommand::ToggleAutomaticChecks if self.is_accepting() => {
                    self.toggle_automatic_checks();
                }
                WorkerCommand::ManualCheck if self.is_accepting() => self.check_manually(),
                WorkerCommand::RequestInstall if self.is_accepting() => self.request_install(),
                WorkerCommand::Confirmation(confirmed) if self.is_accepting() => {
                    self.handle_confirmation(confirmed);
                }
                WorkerCommand::ManualCheck
                | WorkerCommand::RequestInstall
                | WorkerCommand::Confirmation(_) => self.finish_operation(),
                WorkerCommand::Activate | WorkerCommand::ToggleAutomaticChecks => {}
                #[cfg(test)]
                WorkerCommand::Synchronize(acknowledge) => {
                    let _ = acknowledge.send(());
                }
                WorkerCommand::Stop => break,
            }
        }
    }

    fn receive(&self) -> Option<WorkerCommand> {
        let received = match self.state.next_automatic {
            Some(deadline) => self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now())),
            None => self
                .receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(command) => Some(command),
            Err(mpsc::RecvTimeoutError::Timeout) => Some(WorkerCommand::Activate),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    fn toggle_automatic_checks(&mut self) {
        let current = self.settings.snapshot().automatic_update_checks_enabled();
        if let Ok(updated) = self
            .settings
            .update(SettingsUpdate::AutomaticUpdateChecksEnabled(!current))
        {
            let enabled = updated.automatic_update_checks_enabled();
            self.snapshot
                .lock()
                .expect("updater snapshot lock poisoned")
                .automatic_checks_enabled = enabled;
            if enabled {
                self.attempt_automatic();
            } else {
                self.state.next_automatic = None;
            }
        }
    }

    fn check_manually(&mut self) {
        self.state.next_automatic = Some(Instant::now() + AUTOMATIC_INTERVAL);
        let now = self.clock.unix_seconds();
        let _ = self
            .settings
            .update(SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(Some(
                now,
            )));
        self.set_status(UpdateStatus::Checking(UpdateTrigger::Manual));
        match self.backend.check() {
            Ok(None) => {
                self.state.pending = None;
                if self.finish_with_status(UpdateStatus::UpToDate) {
                    self.presentation.information("Chiù is up to date.");
                }
            }
            Ok(Some(update)) => {
                let version = display_version(update.announced_version());
                self.state.pending = Some(update);
                if self.is_accepting() {
                    self.set_status(UpdateStatus::AwaitingConfirmation {
                        version: version.clone(),
                    });
                    self.show_confirmation(version);
                } else {
                    self.finish_operation();
                }
            }
            Err(()) => {
                let pending = self.pending_view();
                if self.finish_with_status(UpdateStatus::Failed {
                    stage: UpdateFailureStage::Check,
                    trigger: UpdateTrigger::Manual,
                    pending,
                }) {
                    self.presentation.error("Chiù could not check for updates.");
                }
            }
        }
    }

    fn request_install(&mut self) {
        if self.state.pending.is_none() {
            self.finish_operation();
            return;
        }
        let version = self.pending_version();
        self.set_status(UpdateStatus::AwaitingConfirmation {
            version: version.clone(),
        });
        self.show_confirmation(version);
    }

    fn handle_confirmation(&mut self, confirmed: bool) {
        self.record(SafeEvent::Updater {
            action: "confirmation",
            outcome: if confirmed {
                EventOutcome::Succeeded
            } else {
                EventOutcome::Rejected
            },
            stage: "confirmation",
        });
        if confirmed {
            self.install_pending();
        } else {
            self.finish_with_status(UpdateStatus::UpdateAvailable {
                version: self.pending_version(),
            });
        }
    }

    fn attempt_automatic(&mut self) {
        if self.state.pending.is_some()
            || !self.settings.snapshot().automatic_update_checks_enabled()
        {
            self.state.next_automatic = None;
            return;
        }
        if self
            .operation_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let restored = self.settings.snapshot();
        let now = self.clock.unix_seconds();
        match automatic_due(restored.last_automatic_update_check_unix_seconds(), now) {
            AutomaticDue::Later(delay) => {
                self.state.next_automatic = Some(Instant::now() + delay);
                self.finish_operation();
                return;
            }
            AutomaticDue::Future => {
                self.state.next_automatic = Some(Instant::now() + AUTOMATIC_INTERVAL);
                if self.persist_cadence(now).is_err() {
                    self.finish_with_automatic_failure(UpdateFailureStage::Scheduling);
                } else {
                    self.finish_operation();
                }
                return;
            }
            AutomaticDue::Now => {}
        }
        self.state.next_automatic = Some(Instant::now() + AUTOMATIC_INTERVAL);
        if self.persist_cadence(now).is_err() {
            self.finish_with_automatic_failure(UpdateFailureStage::Scheduling);
            return;
        }
        self.set_status(UpdateStatus::Checking(UpdateTrigger::Automatic));
        let status = match self.backend.check() {
            Ok(Some(update)) => {
                let version = display_version(update.announced_version());
                self.state.pending = Some(update);
                self.state.next_automatic = None;
                UpdateStatus::UpdateAvailable { version }
            }
            Ok(None) => UpdateStatus::UpToDate,
            Err(()) => UpdateStatus::Failed {
                stage: UpdateFailureStage::Check,
                trigger: UpdateTrigger::Automatic,
                pending: None,
            },
        };
        self.finish_with_status(status);
    }

    fn persist_cadence(&self, now: u64) -> Result<(), ()> {
        self.settings
            .update(SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(Some(
                now,
            )))
            .map(|_| ())
            .map_err(|_| ())
    }

    fn finish_with_automatic_failure(&self, stage: UpdateFailureStage) {
        self.finish_with_status(UpdateStatus::Failed {
            stage,
            trigger: UpdateTrigger::Automatic,
            pending: None,
        });
    }

    fn show_confirmation(&self, version: Option<String>) {
        let sender = self.confirmation_sender.clone();
        let accepting = self.accepting.clone();
        let operation_claimed = self.operation_claimed.clone();
        self.presentation.confirm_install(
            version,
            Box::new(move |confirmed| {
                if !accepting.load(Ordering::Acquire)
                    || sender.send(WorkerCommand::Confirmation(confirmed)).is_err()
                {
                    operation_claimed.store(false, Ordering::Release);
                }
            }),
        );
    }

    fn install_pending(&mut self) {
        let Some(mut update) = self.state.pending.take() else {
            self.finish_operation();
            return;
        };
        let version = display_version(update.announced_version());
        self.set_status(UpdateStatus::Downloading {
            version: version.clone(),
            percent: None,
        });
        let mut downloaded = 0_u64;
        let download_result = {
            let snapshot = &self.snapshot;
            let accepting = &self.accepting;
            let progress_version = version.clone();
            let mut progress = |chunk: usize, total: Option<u64>| {
                downloaded = downloaded.saturating_add(u64::try_from(chunk).unwrap_or(u64::MAX));
                if accepting.load(Ordering::Acquire) {
                    set_status(
                        snapshot,
                        UpdateStatus::Downloading {
                            version: progress_version.clone(),
                            percent: bounded_percent(downloaded, total),
                        },
                    );
                }
            };
            update.download(&mut progress, &mut || {})
        };
        let bytes = match download_result {
            Ok(bytes) => bytes,
            Err(stage) => {
                self.retain_failed_update(update, version, stage);
                return;
            }
        };
        if !self.is_accepting() {
            self.state.pending = Some(update);
            self.finish_operation();
            return;
        }
        self.set_status(UpdateStatus::Installing {
            version: version.clone(),
        });
        if let Err(stage) = update.install(bytes) {
            self.retain_failed_update(update, version, stage);
            return;
        }
        if !self.is_accepting() {
            self.finish_operation();
            return;
        }
        if self.installed_update.finish().is_err() {
            self.state.pending = Some(update);
            if self.finish_with_status(UpdateStatus::Failed {
                stage: UpdateFailureStage::Restart,
                trigger: UpdateTrigger::Manual,
                pending: Some(pending_view(version)),
            }) {
                self.presentation
                    .error("Chiù installed the update but could not restart.");
            }
            return;
        }
        self.finish_operation();
    }

    fn retain_failed_update(
        &mut self,
        update: Box<dyn PendingUpdate>,
        version: Option<String>,
        stage: UpdateFailureStage,
    ) {
        self.state.pending = Some(update);
        if self.finish_with_status(UpdateStatus::Failed {
            stage,
            trigger: UpdateTrigger::Manual,
            pending: Some(pending_view(version)),
        }) {
            self.presentation.error(install_failure_message(stage));
        }
    }

    fn pending_view(&self) -> Option<PendingUpdateView> {
        self.state
            .pending
            .as_ref()
            .map(|update| pending_view(display_version(update.announced_version())))
    }

    fn pending_version(&self) -> Option<String> {
        self.state
            .pending
            .as_ref()
            .and_then(|update| display_version(update.announced_version()))
    }

    fn set_status(&self, status: UpdateStatus) {
        let previous = self
            .snapshot
            .lock()
            .expect("updater snapshot lock poisoned")
            .status
            .clone();
        set_status(&self.snapshot, status);
        let current = self
            .snapshot
            .lock()
            .expect("updater snapshot lock poisoned")
            .status
            .clone();
        if update_status_kind(&previous) != update_status_kind(&current) {
            self.record(update_status_event(&current));
        }
    }

    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }

    fn finish_operation(&self) {
        self.operation_claimed.store(false, Ordering::Release);
    }

    fn finish_with_status(&self, status: UpdateStatus) -> bool {
        self.finish_operation();
        if self.is_accepting() {
            self.set_status(status);
            true
        } else {
            false
        }
    }
}

fn update_status_kind(status: &UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::Idle => "idle",
        UpdateStatus::Checking(_) => "checking",
        UpdateStatus::UpToDate => "up-to-date",
        UpdateStatus::AwaitingConfirmation { .. } => "awaiting-confirmation",
        UpdateStatus::UpdateAvailable { .. } => "update-available",
        UpdateStatus::Downloading { .. } => "downloading",
        UpdateStatus::Installing { .. } => "installing",
        UpdateStatus::Failed { .. } => "failed",
    }
}

fn update_status_event(status: &UpdateStatus) -> SafeEvent {
    match status {
        UpdateStatus::UpdateAvailable { version }
        | UpdateStatus::AwaitingConfirmation { version } => {
            SafeEvent::updater_available(version.as_deref())
        }
        UpdateStatus::Checking(trigger) => SafeEvent::Updater {
            action: match trigger {
                UpdateTrigger::Automatic => "automatic-check",
                UpdateTrigger::Manual => "manual-check",
            },
            outcome: EventOutcome::Succeeded,
            stage: "started",
        },
        UpdateStatus::UpToDate => SafeEvent::Updater {
            action: "check",
            outcome: EventOutcome::Succeeded,
            stage: "up-to-date",
        },
        UpdateStatus::Downloading { .. } => SafeEvent::Updater {
            action: "install",
            outcome: EventOutcome::Succeeded,
            stage: "downloading",
        },
        UpdateStatus::Installing { .. } => SafeEvent::Updater {
            action: "install",
            outcome: EventOutcome::Succeeded,
            stage: "installing",
        },
        UpdateStatus::Failed { stage, .. } => SafeEvent::Updater {
            action: "operation",
            outcome: EventOutcome::Failed,
            stage: update_failure_stage(*stage),
        },
        UpdateStatus::Idle => SafeEvent::Updater {
            action: "operation",
            outcome: EventOutcome::Succeeded,
            stage: "idle",
        },
    }
}

fn update_failure_stage(stage: UpdateFailureStage) -> &'static str {
    match stage {
        UpdateFailureStage::Scheduling => "scheduling",
        UpdateFailureStage::Check => "check",
        UpdateFailureStage::Download => "download",
        UpdateFailureStage::Verification => "verification",
        UpdateFailureStage::Install => "installation",
        UpdateFailureStage::Restart => "restart",
    }
}

fn set_status(snapshot: &Mutex<UpdaterSnapshot>, status: UpdateStatus) {
    snapshot
        .lock()
        .expect("updater snapshot lock poisoned")
        .status = status;
}

fn pending_view(version: Option<String>) -> PendingUpdateView {
    version.map_or(PendingUpdateView::Generic, PendingUpdateView::Version)
}

fn bounded_percent(downloaded: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|total| *total > 0)?;
    Some(((u128::from(downloaded).saturating_mul(100) / u128::from(total)).min(100)) as u8)
}

fn display_version(version: &str) -> Option<String> {
    (!version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')))
    .then(|| version.to_owned())
}

fn install_failure_message(stage: UpdateFailureStage) -> &'static str {
    match stage {
        UpdateFailureStage::Download => "Chiù could not download the update.",
        UpdateFailureStage::Verification => "Chiù could not verify the update.",
        UpdateFailureStage::Install => "Chiù could not install the update.",
        UpdateFailureStage::Restart => "Chiù installed the update but could not restart.",
        UpdateFailureStage::Scheduling | UpdateFailureStage::Check => {
            "Chiù could not check for updates."
        }
    }
}

enum AutomaticDue {
    Now,
    Later(Duration),
    Future,
}

fn automatic_due(last: Option<u64>, now: u64) -> AutomaticDue {
    match last {
        None => AutomaticDue::Now,
        Some(last) if last > now => AutomaticDue::Future,
        Some(last) => {
            let elapsed = Duration::from_secs(now - last);
            if elapsed >= AUTOMATIC_INTERVAL {
                AutomaticDue::Now
            } else {
                AutomaticDue::Later(AUTOMATIC_INTERVAL - elapsed)
            }
        }
    }
}

mod tauri_adapter;
mod worker_exit;
#[cfg(test)]
use tauri_adapter::{ConfiguredUpdater, configured_updater};
pub(crate) use tauri_adapter::{UpdaterRegistration, register};

#[cfg(test)]
mod shutdown_tests;
#[cfg(test)]
mod tests;
