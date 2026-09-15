use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const ARCHIVE_COUNT: usize = 4;
const CURRENT_FILE: &str = "chiu.log";
const MAX_FILE_SIZE: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoggingFailureStage {
    ResolveDirectory,
    CreateDirectory,
    Open,
    Prune,
    Rotate,
    Write,
    Flush,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoggingHealth {
    Available,
    Unavailable(LoggingFailureStage),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionPolicy {
    pub(crate) maximum_files: usize,
    pub(crate) maximum_file_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventOutcome {
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtectionAction {
    Acquire,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetectorStateEvent {
    Idle,
    Active,
    Hold,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualCommandEvent {
    Start,
    Add,
    Convert,
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SafeEvent {
    ProcessStarted,
    ProcessStopped,
    ApplicationReady,
    ApplicationDegraded,
    ApplicationFailed,
    ShutdownRequested,
    CleanupFailed {
        capability: &'static str,
    },
    OptionalCapabilityUnavailable {
        capability: &'static str,
    },
    SettingsLoaded {
        condition: &'static str,
        persistence: &'static str,
        recoveries: usize,
    },
    SettingsChanged {
        field: &'static str,
    },
    SettingsWriteFailed {
        field: &'static str,
    },
    ManualCommand {
        command: ManualCommandEvent,
        outcome: EventOutcome,
    },
    ManualExpired {
        outcome: EventOutcome,
    },
    DetectorTransition {
        from: DetectorStateEvent,
        to: DetectorStateEvent,
    },
    DetectorContinuityReset,
    AutomaticEligibilityChanged {
        eligible: bool,
        suppression: &'static str,
    },
    AutomaticReconciliation {
        active: bool,
        outcome: EventOutcome,
    },
    PowerChanged {
        source: &'static str,
        health: &'static str,
    },
    WakeReasonsChanged {
        automatic: bool,
        manual: bool,
    },
    NativeProtection {
        action: ProtectionAction,
        outcome: EventOutcome,
    },
    NetworkAvailability {
        status: &'static str,
    },
    NetworkContinuityReset,
    NetworkTopologyChanged {
        interfaces: usize,
    },
    LaunchAtLogin {
        action: &'static str,
        outcome: EventOutcome,
        stage: &'static str,
    },
    Updater {
        action: &'static str,
        outcome: EventOutcome,
        stage: &'static str,
    },
    UpdaterAvailable {
        version: String,
    },
    DiagnosticsAction {
        action: &'static str,
        outcome: EventOutcome,
    },
    PresentationFailure {
        stage: &'static str,
    },
}

impl SafeEvent {
    pub(crate) fn updater_available(version: Option<&str>) -> Self {
        Self::UpdaterAvailable {
            version: safe_version(version),
        }
    }

    fn render(&self) -> String {
        match self {
            Self::ProcessStarted => {
                format!("INFO process.started version={}", env!("CARGO_PKG_VERSION"))
            }
            Self::ProcessStopped => "INFO process.stopped".to_owned(),
            Self::ApplicationReady => "INFO application.ready".to_owned(),
            Self::ApplicationDegraded => "WARN application.degraded".to_owned(),
            Self::ApplicationFailed => "ERROR application.failed".to_owned(),
            Self::ShutdownRequested => "INFO application.shutdown-requested".to_owned(),
            Self::CleanupFailed { capability } => {
                format!("ERROR application.cleanup-failed capability={capability}")
            }
            Self::OptionalCapabilityUnavailable { capability } => {
                format!("WARN capability.unavailable capability={capability}")
            }
            Self::SettingsLoaded {
                condition,
                persistence,
                recoveries,
            } => format!(
                "INFO settings.loaded condition={condition} persistence={persistence} recoveries={recoveries}"
            ),
            Self::SettingsChanged { field } => {
                format!("INFO settings.changed field={field}")
            }
            Self::SettingsWriteFailed { field } => {
                format!("WARN settings.write-failed field={field}")
            }
            Self::ManualCommand { command, outcome } => format!(
                "{} manual.command command={} outcome={}",
                level(*outcome),
                manual_command(*command),
                outcome_name(*outcome)
            ),
            Self::ManualExpired { outcome } => format!(
                "{} manual.expired outcome={}",
                level(*outcome),
                outcome_name(*outcome)
            ),
            Self::DetectorTransition { from, to } => format!(
                "INFO detector.transition from={} to={}",
                detector_state(*from),
                detector_state(*to)
            ),
            Self::DetectorContinuityReset => "WARN detector.continuity-reset".to_owned(),
            Self::AutomaticEligibilityChanged {
                eligible,
                suppression,
            } => {
                format!("INFO automatic.eligibility eligible={eligible} suppression={suppression}")
            }
            Self::AutomaticReconciliation { active, outcome } => format!(
                "{} automatic.reconciliation active={active} outcome={}",
                level(*outcome),
                outcome_name(*outcome)
            ),
            Self::PowerChanged { source, health } => {
                format!("INFO power.changed source={source} health={health}")
            }
            Self::WakeReasonsChanged { automatic, manual } => {
                format!("INFO wake.reasons-changed automatic={automatic} manual={manual}")
            }
            Self::NativeProtection { action, outcome } => format!(
                "{} wake.native-protection action={} outcome={}",
                level(*outcome),
                protection_action(*action),
                outcome_name(*outcome)
            ),
            Self::NetworkAvailability { status } => {
                format!("INFO network.availability status={status}")
            }
            Self::NetworkContinuityReset => "WARN network.continuity-reset".to_owned(),
            Self::NetworkTopologyChanged { interfaces } => {
                format!("INFO network.topology-changed interfaces={interfaces}")
            }
            Self::LaunchAtLogin {
                action,
                outcome,
                stage,
            } => format!(
                "{} launch-at-login action={action} outcome={} stage={stage}",
                level(*outcome),
                outcome_name(*outcome)
            ),
            Self::Updater {
                action,
                outcome,
                stage,
            } => format!(
                "{} updater action={action} outcome={} stage={stage}",
                level(*outcome),
                outcome_name(*outcome)
            ),
            Self::UpdaterAvailable { version } => {
                format!("INFO updater.update-available version={version}")
            }
            Self::DiagnosticsAction { action, outcome } => format!(
                "{} diagnostics action={action} outcome={}",
                level(*outcome),
                outcome_name(*outcome)
            ),
            Self::PresentationFailure { stage } => {
                format!("ERROR presentation.failed stage={stage}")
            }
        }
    }
}

fn outcome_name(outcome: EventOutcome) -> &'static str {
    match outcome {
        EventOutcome::Succeeded => "succeeded",
        EventOutcome::Failed => "failed",
        EventOutcome::Rejected => "rejected",
    }
}

fn level(outcome: EventOutcome) -> &'static str {
    match outcome {
        EventOutcome::Succeeded => "INFO",
        EventOutcome::Failed => "WARN",
        EventOutcome::Rejected => "INFO",
    }
}

fn protection_action(action: ProtectionAction) -> &'static str {
    match action {
        ProtectionAction::Acquire => "acquire",
        ProtectionAction::Release => "release",
    }
}

fn detector_state(state: DetectorStateEvent) -> &'static str {
    match state {
        DetectorStateEvent::Idle => "idle",
        DetectorStateEvent::Active => "active",
        DetectorStateEvent::Hold => "hold",
    }
}

fn manual_command(command: ManualCommandEvent) -> &'static str {
    match command {
        ManualCommandEvent::Start => "start",
        ManualCommandEvent::Add => "add",
        ManualCommandEvent::Convert => "convert",
        ManualCommandEvent::Stop => "stop",
    }
}

fn safe_version(version: Option<&str>) -> String {
    let Some(version) = version else {
        return "unknown".to_owned();
    };
    if version.is_empty()
        || version.len() > 64
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return "redacted".to_owned();
    }
    version.to_owned()
}

trait Clock: Send + Sync {
    fn timestamp(&self) -> String;
}

struct UtcClock;

impl Clock for UtcClock {
    fn timestamp(&self) -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
    }
}

trait LogWriter: Send {
    fn write_line(&mut self, line: &[u8]) -> Result<(), LoggingFailureStage>;
}

struct RotatingWriter {
    directory: PathBuf,
    file: File,
    length: u64,
    maximum_file_bytes: u64,
    archive_count: usize,
}

impl RotatingWriter {
    fn open(
        directory: PathBuf,
        maximum_file_bytes: u64,
        archive_count: usize,
    ) -> Result<Self, LoggingFailureStage> {
        fs::create_dir_all(&directory).map_err(|_| LoggingFailureStage::CreateDirectory)?;
        prune_owned_archives(&directory, archive_count)?;
        let current = directory.join(CURRENT_FILE);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)
            .map_err(|_| LoggingFailureStage::Open)?;
        let length = file
            .metadata()
            .map_err(|_| LoggingFailureStage::Open)?
            .len();
        Ok(Self {
            directory,
            file,
            length,
            maximum_file_bytes,
            archive_count,
        })
    }

    fn rotate(&mut self) -> Result<(), LoggingFailureStage> {
        self.file.flush().map_err(|_| LoggingFailureStage::Flush)?;
        for index in (1..=self.archive_count).rev() {
            let destination = archive_path(&self.directory, index);
            if index == self.archive_count {
                remove_if_exists(&destination).map_err(|_| LoggingFailureStage::Prune)?;
            }
            let source = if index == 1 {
                self.directory.join(CURRENT_FILE)
            } else {
                archive_path(&self.directory, index - 1)
            };
            if source.exists() {
                fs::rename(&source, &destination).map_err(|_| LoggingFailureStage::Rotate)?;
            }
        }
        let current = self.directory.join(CURRENT_FILE);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(current)
            .map_err(|_| LoggingFailureStage::Open)?;
        self.length = 0;
        Ok(())
    }
}

impl LogWriter for RotatingWriter {
    fn write_line(&mut self, line: &[u8]) -> Result<(), LoggingFailureStage> {
        let incoming = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.length > 0 && self.length.saturating_add(incoming) > self.maximum_file_bytes {
            self.rotate()?;
        }
        self.file
            .write_all(line)
            .map_err(|_| LoggingFailureStage::Write)?;
        self.file.flush().map_err(|_| LoggingFailureStage::Flush)?;
        self.length = self.length.saturating_add(incoming);
        Ok(())
    }
}

struct LogState {
    health: LoggingHealth,
    writer: Option<Box<dyn LogWriter>>,
}

#[derive(Clone)]
pub(crate) struct LocalLog {
    state: Arc<Mutex<LogState>>,
    directory: Option<PathBuf>,
    clock: Arc<dyn Clock>,
}

impl LocalLog {
    pub(crate) fn initialize(directory: Option<PathBuf>) -> Self {
        let Some(directory) = directory else {
            return Self::unavailable(LoggingFailureStage::ResolveDirectory);
        };
        match RotatingWriter::open(directory.clone(), MAX_FILE_SIZE, ARCHIVE_COUNT) {
            Ok(writer) => Self::with_writer(directory, Box::new(writer), Arc::new(UtcClock)),
            Err(stage) => Self {
                state: Arc::new(Mutex::new(LogState {
                    health: LoggingHealth::Unavailable(stage),
                    writer: None,
                })),
                directory: None,
                clock: Arc::new(UtcClock),
            },
        }
    }

    pub(crate) fn unavailable(stage: LoggingFailureStage) -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState {
                health: LoggingHealth::Unavailable(stage),
                writer: None,
            })),
            directory: None,
            clock: Arc::new(UtcClock),
        }
    }

    fn with_writer(directory: PathBuf, writer: Box<dyn LogWriter>, clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState {
                health: LoggingHealth::Available,
                writer: Some(writer),
            })),
            directory: Some(directory),
            clock,
        }
    }

    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        (
            Self::with_writer(
                PathBuf::from("/test/chiu/logs"),
                Box::new(RecordingWriter(lines.clone())),
                Arc::new(FixedClock),
            ),
            lines,
        )
    }

    #[cfg(test)]
    pub(crate) fn failing_for_tests() -> Self {
        Self::with_writer(
            PathBuf::from("/test/chiu/logs"),
            Box::new(AlwaysFailWriter),
            Arc::new(FixedClock),
        )
    }

    pub(crate) fn record(&self, event: SafeEvent) {
        let line = format!("{} {}\n", self.clock.timestamp(), event.render());
        let failure = {
            let mut state = self.state.lock().expect("local log lock poisoned");
            let Some(writer) = state.writer.as_mut() else {
                eprint!("{line}");
                return;
            };
            writer.write_line(line.as_bytes()).err()
        };
        if let Some(stage) = failure {
            let mut state = self.state.lock().expect("local log lock poisoned");
            state.health = LoggingHealth::Unavailable(stage);
            state.writer = None;
            eprintln!("Chiù local logging unavailable: {}", stage_name(stage));
        }
    }

    pub(crate) fn health(&self) -> LoggingHealth {
        self.state.lock().expect("local log lock poisoned").health
    }

    pub(crate) fn directory(&self) -> Option<&Path> {
        (self.health() == LoggingHealth::Available)
            .then_some(())
            .and(self.directory.as_deref())
    }

    pub(crate) const fn retention_policy() -> RetentionPolicy {
        RetentionPolicy {
            maximum_files: ARCHIVE_COUNT + 1,
            maximum_file_bytes: MAX_FILE_SIZE,
        }
    }
}

#[cfg(test)]
struct RecordingWriter(Arc<Mutex<Vec<String>>>);

#[cfg(test)]
struct AlwaysFailWriter;

#[cfg(test)]
impl LogWriter for AlwaysFailWriter {
    fn write_line(&mut self, _: &[u8]) -> Result<(), LoggingFailureStage> {
        Err(LoggingFailureStage::Write)
    }
}

#[cfg(test)]
impl LogWriter for RecordingWriter {
    fn write_line(&mut self, line: &[u8]) -> Result<(), LoggingFailureStage> {
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8(line.to_vec()).unwrap());
        Ok(())
    }
}

#[cfg(test)]
struct FixedClock;

#[cfg(test)]
impl Clock for FixedClock {
    fn timestamp(&self) -> String {
        "2026-08-10T12:00:00Z".to_owned()
    }
}

fn archive_path(directory: &Path, index: usize) -> PathBuf {
    directory.join(format!("chiu.{index}.log"))
}

fn prune_owned_archives(directory: &Path, archive_count: usize) -> Result<(), LoggingFailureStage> {
    let entries = fs::read_dir(directory).map_err(|_| LoggingFailureStage::Prune)?;
    for entry in entries {
        let entry = entry.map_err(|_| LoggingFailureStage::Prune)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(index) = name
            .strip_prefix("chiu.")
            .and_then(|value| value.strip_suffix(".log"))
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        if index == 0 || index > archive_count {
            remove_if_exists(&entry.path()).map_err(|_| LoggingFailureStage::Prune)?;
        }
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn stage_name(stage: LoggingFailureStage) -> &'static str {
    match stage {
        LoggingFailureStage::ResolveDirectory => "directory-resolution",
        LoggingFailureStage::CreateDirectory => "directory-creation",
        LoggingFailureStage::Open => "open",
        LoggingFailureStage::Prune => "prune",
        LoggingFailureStage::Rotate => "rotation",
        LoggingFailureStage::Write => "write",
        LoggingFailureStage::Flush => "flush",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedClock;

    impl Clock for FixedClock {
        fn timestamp(&self) -> String {
            "2026-08-10T12:00:00Z".to_owned()
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("chiu-local-log-{name}-{}", std::process::id()));
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

    #[test]
    fn one_safe_event_is_persisted_with_a_stable_utc_line() {
        let directory = TestDirectory::new("first-event");
        let writer =
            RotatingWriter::open(directory.0.clone(), MAX_FILE_SIZE, ARCHIVE_COUNT).unwrap();
        let log =
            LocalLog::with_writer(directory.0.clone(), Box::new(writer), Arc::new(FixedClock));

        log.record(SafeEvent::ProcessStarted);

        assert_eq!(
            fs::read_to_string(directory.0.join(CURRENT_FILE)).unwrap(),
            format!(
                "2026-08-10T12:00:00Z INFO process.started version={}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(log.health(), LoggingHealth::Available);
        assert_eq!(log.directory(), Some(directory.0.as_path()));
    }

    #[test]
    fn rotation_and_reopen_keep_only_current_plus_four_archives() {
        let directory = TestDirectory::new("rotation");
        for _ in 0..12 {
            let writer = RotatingWriter::open(directory.0.clone(), 1, ARCHIVE_COUNT).unwrap();
            let log =
                LocalLog::with_writer(directory.0.clone(), Box::new(writer), Arc::new(FixedClock));
            log.record(SafeEvent::ProcessStarted);
        }

        let files = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 5);
        assert!(directory.0.join(CURRENT_FILE).exists());
        for index in 1..=ARCHIVE_COUNT {
            assert!(archive_path(&directory.0, index).exists());
        }
        assert_eq!(LocalLog::retention_policy().maximum_files, 5);
        assert_eq!(LocalLog::retention_policy().maximum_file_bytes, 1_048_576);
    }

    struct FailingWriter {
        calls: Arc<AtomicUsize>,
    }

    impl LogWriter for FailingWriter {
        fn write_line(&mut self, _: &[u8]) -> Result<(), LoggingFailureStage> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(LoggingFailureStage::Write)
        }
    }

    #[test]
    fn writer_failure_disables_file_attempts_without_recursive_retries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let log = LocalLog::with_writer(
            PathBuf::from("/unused"),
            Box::new(FailingWriter {
                calls: calls.clone(),
            }),
            Arc::new(FixedClock),
        );

        log.record(SafeEvent::ProcessStarted);
        log.record(SafeEvent::ProcessStopped);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            log.health(),
            LoggingHealth::Unavailable(LoggingFailureStage::Write)
        );
        assert_eq!(log.directory(), None);
    }

    #[test]
    fn safe_event_projection_never_persists_unchecked_update_text() {
        let (log, lines) = LocalLog::recording();

        log.record(SafeEvent::updater_available(Some(
            "https://example.test/update?file=secret.pkg 10.0.0.1",
        )));
        log.record(SafeEvent::ManualCommand {
            command: ManualCommandEvent::Start,
            outcome: EventOutcome::Succeeded,
        });
        log.record(SafeEvent::NetworkTopologyChanged { interfaces: 3 });
        log.record(SafeEvent::NativeProtection {
            action: ProtectionAction::Acquire,
            outcome: EventOutcome::Failed,
        });

        let persisted = lines.lock().unwrap().join("");
        assert!(persisted.contains("version=redacted"));
        assert!(persisted.contains("manual.command"));
        assert!(persisted.contains("network.topology-changed"));
        assert!(persisted.contains("wake.native-protection"));
        for prohibited in ["https://", "example.test", "secret.pkg", "10.0.0.1"] {
            assert!(!persisted.contains(prohibited), "{prohibited}");
        }
    }
}
