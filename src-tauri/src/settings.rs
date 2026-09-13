use crate::{
    automatic_download::{AutomaticDownloadSettings, AutomaticDownloadSettingsError},
    local_log::{LocalLog, SafeEvent},
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const BACKUP_FILE: &str = "settings.json.bak";
const BACKUP_TEMP_FILE: &str = "settings.json.bak.tmp";
const PRIMARY_FILE: &str = "settings.json";
const PRIMARY_TEMP_FILE: &str = "settings.json.tmp";
const QUARANTINE_PREFIX: &str = "settings.corrupt.";
const QUARANTINE_RETENTION: usize = 2;
const SCHEMA_VERSION: u64 = 1;
static NEXT_QUARANTINE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Settings {
    automatic_download_protection_enabled: bool,
    protect_on_battery: bool,
    launch_at_login: bool,
    automatic_update_checks_enabled: bool,
    last_automatic_update_check_unix_seconds: Option<u64>,
    automatic_download: AutomaticDownloadSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            automatic_download_protection_enabled: true,
            protect_on_battery: false,
            launch_at_login: true,
            automatic_update_checks_enabled: true,
            last_automatic_update_check_unix_seconds: None,
            automatic_download: AutomaticDownloadSettings::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoadCondition {
    FirstRun,
    CurrentVersion,
    PartialRecovery,
    RecoveredFromBackup,
    MalformedDefaulted,
    PersistenceUnavailable,
    SchemaMetadataRecovered,
    UnsupportedFutureSchema { found: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceCondition {
    Writable,
    Unavailable,
    BlockedByFutureSchema,
}

fn load_condition_name(condition: LoadCondition) -> &'static str {
    match condition {
        LoadCondition::FirstRun => "first-run",
        LoadCondition::CurrentVersion => "current-version",
        LoadCondition::PartialRecovery => "partial-recovery",
        LoadCondition::RecoveredFromBackup => "recovered-from-backup",
        LoadCondition::MalformedDefaulted => "malformed-defaulted",
        LoadCondition::PersistenceUnavailable => "persistence-unavailable",
        LoadCondition::SchemaMetadataRecovered => "schema-metadata-recovered",
        LoadCondition::UnsupportedFutureSchema { .. } => "unsupported-future-schema",
    }
}

fn persistence_condition_name(condition: PersistenceCondition) -> &'static str {
    match condition {
        PersistenceCondition::Writable => "writable",
        PersistenceCondition::Unavailable => "unavailable",
        PersistenceCondition::BlockedByFutureSchema => "blocked-future-schema",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactWarning {
    StaleTemporary,
    Quarantine,
    QuarantineCleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingPath {
    AutomaticDownloadProtectionEnabled,
    ProtectOnBattery,
    LaunchAtLogin,
    AutomaticUpdateChecksEnabled,
    LastAutomaticUpdateCheckUnixSeconds,
    MeaningfulReceiveRateBytesPerSecond,
    ActivationQualificationMilliseconds,
    GraceMilliseconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryReason {
    Missing,
    WrongType,
    InvalidValue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FieldRecovery {
    path: SettingPath,
    reason: RecoveryReason,
}

impl FieldRecovery {
    pub(crate) const fn new(path: SettingPath, reason: RecoveryReason) -> Self {
        Self { path, reason }
    }

    pub(crate) fn path(self) -> SettingPath {
        self.path
    }

    pub(crate) fn reason(self) -> RecoveryReason {
        self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingsHealth {
    load_condition: LoadCondition,
    field_recoveries: Vec<FieldRecovery>,
    persistence: PersistenceCondition,
    artifact_warnings: Vec<ArtifactWarning>,
    last_save_failure: Option<SettingsPersistenceFailure>,
}

impl SettingsHealth {
    pub(crate) fn load_condition(&self) -> LoadCondition {
        self.load_condition
    }

    pub(crate) fn field_recoveries(&self) -> &[FieldRecovery] {
        &self.field_recoveries
    }

    pub(crate) fn persistence(&self) -> PersistenceCondition {
        self.persistence
    }

    pub(crate) fn artifact_warnings(&self) -> &[ArtifactWarning] {
        &self.artifact_warnings
    }

    pub(crate) fn last_save_failure(&self) -> Option<SettingsPersistenceFailure> {
        self.last_save_failure
    }

    pub(crate) fn is_clean(&self) -> bool {
        matches!(
            self.load_condition,
            LoadCondition::FirstRun | LoadCondition::CurrentVersion
        ) && self.artifact_warnings.is_empty()
            && self.last_save_failure.is_none()
    }
}

struct SettingsState {
    snapshot: Settings,
    health: SettingsHealth,
    #[allow(dead_code, reason = "used by the explicit settings mutation seam")]
    directory: PathBuf,
    #[allow(dead_code, reason = "used by the explicit settings mutation seam")]
    preservation_required: bool,
    #[allow(dead_code, reason = "used by the explicit settings mutation seam")]
    replacer: Arc<dyn AtomicReplace>,
}

#[allow(
    dead_code,
    reason = "downstream settings consumers own these mutations"
)]
pub(crate) enum SettingsUpdate {
    AutomaticDownloadProtectionEnabled(bool),
    ProtectOnBattery(bool),
    LaunchAtLogin(bool),
    AutomaticUpdateChecksEnabled(bool),
    LastAutomaticUpdateCheckUnixSeconds(Option<u64>),
    AutomaticDownloadTuning {
        meaningful_receive_rate_bytes_per_second: u64,
        activation_qualification_milliseconds: u64,
        grace_milliseconds: u64,
    },
}

#[derive(Debug)]
pub(crate) enum SettingsUpdateError {
    InvalidAutomaticDownload(AutomaticDownloadSettingsError),
    Persistence(SettingsPersistenceFailure),
    UnsupportedFutureSchema(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsPersistenceFailure {
    Unavailable,
    CreateDirectory,
    PreserveSource,
    Serialize,
    WriteBackup,
    WritePrimary,
    ReplaceBackup,
    ReplacePrimary,
}

impl std::fmt::Display for SettingsUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAutomaticDownload(error) => error.fmt(formatter),
            Self::Persistence(failure) => {
                write!(formatter, "settings persistence failed: {failure:?}")
            }
            Self::UnsupportedFutureSchema(version) => {
                write!(
                    formatter,
                    "settings schema version {version} is not supported"
                )
            }
        }
    }
}

impl std::error::Error for SettingsUpdateError {}

#[allow(
    dead_code,
    reason = "constructed by the explicit settings mutation seam"
)]
#[derive(Serialize)]
struct PersistedSettingsV1 {
    schema_version: u64,
    automatic_download_protection_enabled: bool,
    protect_on_battery: bool,
    launch_at_login: bool,
    automatic_update_checks_enabled: bool,
    last_automatic_update_check_unix_seconds: Option<u64>,
    automatic_download: PersistedAutomaticDownload,
}

#[allow(
    dead_code,
    reason = "constructed by the explicit settings mutation seam"
)]
#[derive(Serialize)]
struct PersistedAutomaticDownload {
    meaningful_receive_rate_bytes_per_second: u64,
    activation_qualification_milliseconds: u64,
    grace_milliseconds: u64,
}

#[derive(Clone)]
pub(crate) struct SettingsService {
    state: Arc<Mutex<SettingsState>>,
    log: LocalLog,
}

impl SettingsService {
    pub(crate) fn load(directory: PathBuf, log: LocalLog) -> Self {
        Self::load_with_replacer(directory, Arc::new(PlatformAtomicReplace), log)
    }

    fn load_with_replacer(
        directory: PathBuf,
        replacer: Arc<dyn AtomicReplace>,
        log: LocalLog,
    ) -> Self {
        let (snapshot, health) = load_settings(&directory);
        let preservation_required = directory.join(PRIMARY_FILE).exists()
            && matches!(
                health.load_condition,
                LoadCondition::SchemaMetadataRecovered
                    | LoadCondition::MalformedDefaulted
                    | LoadCondition::RecoveredFromBackup
            );
        let service = Self {
            state: Arc::new(Mutex::new(SettingsState {
                snapshot,
                health,
                directory,
                preservation_required,
                replacer,
            })),
            log,
        };
        service.record_loaded();
        service
    }

    pub(crate) fn unavailable(log: LocalLog) -> Self {
        let (snapshot, health) = unavailable_load();
        let service = Self {
            state: Arc::new(Mutex::new(SettingsState {
                snapshot,
                health,
                directory: PathBuf::new(),
                preservation_required: false,
                replacer: Arc::new(PlatformAtomicReplace),
            })),
            log,
        };
        service.record_loaded();
        service
    }

    pub(crate) fn snapshot(&self) -> Settings {
        self.state
            .lock()
            .expect("settings state lock poisoned")
            .snapshot
            .clone()
    }

    pub(crate) fn health(&self) -> SettingsHealth {
        self.state
            .lock()
            .expect("settings state lock poisoned")
            .health
            .clone()
    }

    fn record_loaded(&self) {
        let health = self.health();
        self.log.record(SafeEvent::SettingsLoaded {
            condition: load_condition_name(health.load_condition()),
            persistence: persistence_condition_name(health.persistence()),
            recoveries: health.field_recoveries().len(),
        });
    }

    pub(crate) fn update(&self, update: SettingsUpdate) -> Result<Settings, SettingsUpdateError> {
        let field = settings_update_field(&update);
        let result = self.update_inner(update);
        self.log.record(if result.is_ok() {
            SafeEvent::SettingsChanged { field }
        } else {
            SafeEvent::SettingsWriteFailed { field }
        });
        result
    }

    fn update_inner(&self, update: SettingsUpdate) -> Result<Settings, SettingsUpdateError> {
        let mut state = self.state.lock().expect("settings state lock poisoned");
        if let LoadCondition::UnsupportedFutureSchema { found } = state.health.load_condition {
            return Err(SettingsUpdateError::UnsupportedFutureSchema(found));
        }
        if state.health.persistence == PersistenceCondition::Unavailable {
            return Err(SettingsUpdateError::Persistence(
                SettingsPersistenceFailure::Unavailable,
            ));
        }
        let mut proposed = state.snapshot.clone();
        proposed.apply(update)?;
        let document = proposed.to_persisted();
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| {
            record_persistence_failure(&mut state, SettingsPersistenceFailure::Serialize)
        })?;
        fs::create_dir_all(&state.directory).map_err(|_| {
            record_persistence_failure(&mut state, SettingsPersistenceFailure::CreateDirectory)
        })?;
        let temporary = state.directory.join(PRIMARY_TEMP_FILE);
        let primary = state.directory.join(PRIMARY_FILE);
        write_synchronized(&temporary, &bytes).map_err(|_| {
            record_persistence_failure(&mut state, SettingsPersistenceFailure::WritePrimary)
        })?;
        if state.preservation_required {
            preserve_source(&primary, &state.directory).map_err(|_| {
                record_persistence_failure(&mut state, SettingsPersistenceFailure::PreserveSource)
            })?;
            state.preservation_required = false;
            if prune_quarantines(&state.directory).is_err() {
                state
                    .health
                    .artifact_warnings
                    .push(ArtifactWarning::QuarantineCleanup);
            }
        }
        if primary.exists() {
            let backup_bytes =
                serde_json::to_vec_pretty(&state.snapshot.to_persisted()).map_err(|_| {
                    record_persistence_failure(&mut state, SettingsPersistenceFailure::Serialize)
                })?;
            let backup_temporary = state.directory.join(BACKUP_TEMP_FILE);
            let backup = state.directory.join(BACKUP_FILE);
            write_synchronized(&backup_temporary, &backup_bytes).map_err(|_| {
                record_persistence_failure(&mut state, SettingsPersistenceFailure::WriteBackup)
            })?;
            state
                .replacer
                .replace(&backup_temporary, &backup)
                .map_err(|_| {
                    record_persistence_failure(
                        &mut state,
                        SettingsPersistenceFailure::ReplaceBackup,
                    )
                })?;
        }
        state.replacer.replace(&temporary, &primary).map_err(|_| {
            record_persistence_failure(&mut state, SettingsPersistenceFailure::ReplacePrimary)
        })?;
        state.snapshot = proposed.clone();
        state.health.last_save_failure = None;
        Ok(proposed)
    }
}

fn settings_update_field(update: &SettingsUpdate) -> &'static str {
    match update {
        SettingsUpdate::AutomaticDownloadProtectionEnabled(_) => "automatic-protection",
        SettingsUpdate::ProtectOnBattery(_) => "protect-on-battery",
        SettingsUpdate::LaunchAtLogin(_) => "launch-at-login",
        SettingsUpdate::AutomaticUpdateChecksEnabled(_) => "automatic-update-checks",
        SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(_) => "last-update-check",
        SettingsUpdate::AutomaticDownloadTuning { .. } => "automatic-download-tuning",
    }
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
fn record_persistence_failure(
    state: &mut SettingsState,
    failure: SettingsPersistenceFailure,
) -> SettingsUpdateError {
    state.health.last_save_failure = Some(failure);
    SettingsUpdateError::Persistence(failure)
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
fn write_synchronized(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
trait AtomicReplace: Send + Sync {
    fn replace(&self, source: &Path, destination: &Path) -> std::io::Result<()>;
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
struct PlatformAtomicReplace;

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
impl AtomicReplace for PlatformAtomicReplace {
    fn replace(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        platform_atomic_replace(source, destination)
    }
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
#[cfg(target_os = "macos")]
fn platform_atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)?;
    if let Some(directory) = destination.parent() {
        fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[allow(dead_code, reason = "used by the explicit settings mutation seam")]
#[cfg(target_os = "windows")]
fn platform_atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn load_settings(directory: &Path) -> (Settings, SettingsHealth) {
    if !directory.is_absolute() {
        return unavailable_load();
    }
    let artifact_warnings = cleanup_stale_temporary_files(directory);
    let primary = directory.join(PRIMARY_FILE);
    match fs::read(&primary) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value)
                if value
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .is_some_and(|version| version > SCHEMA_VERSION) =>
            {
                let found = value
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .expect("future schema guard checked the version");
                (
                    Settings::default(),
                    SettingsHealth {
                        load_condition: LoadCondition::UnsupportedFutureSchema { found },
                        field_recoveries: Vec::new(),
                        persistence: PersistenceCondition::BlockedByFutureSchema,
                        artifact_warnings,
                        last_save_failure: None,
                    },
                )
            }
            Ok(value)
                if value.get("schema_version").and_then(Value::as_u64) == Some(SCHEMA_VERSION) =>
            {
                match Settings::from_document(&value) {
                    Some((snapshot, field_recoveries)) => {
                        let load_condition = if field_recoveries.is_empty() {
                            LoadCondition::CurrentVersion
                        } else {
                            LoadCondition::PartialRecovery
                        };
                        (
                            snapshot,
                            SettingsHealth {
                                load_condition,
                                field_recoveries,
                                persistence: PersistenceCondition::Writable,
                                artifact_warnings,
                                last_save_failure: None,
                            },
                        )
                    }
                    None => malformed_or_backup(directory, &primary, artifact_warnings),
                }
            }
            Ok(value) if value.is_object() => match Settings::from_unversioned_document(&value) {
                Some((snapshot, field_recoveries)) => (
                    snapshot,
                    SettingsHealth {
                        load_condition: LoadCondition::SchemaMetadataRecovered,
                        field_recoveries,
                        persistence: PersistenceCondition::Writable,
                        artifact_warnings,
                        last_save_failure: None,
                    },
                ),
                None => malformed_or_backup(directory, &primary, artifact_warnings),
            },
            Ok(_) => malformed_or_backup(directory, &primary, artifact_warnings),
            Err(_) => malformed_or_backup(directory, &primary, artifact_warnings),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match load_backup(directory) {
                BackupLoad::Clean(snapshot) => (
                    snapshot,
                    SettingsHealth {
                        load_condition: LoadCondition::RecoveredFromBackup,
                        field_recoveries: Vec::new(),
                        persistence: PersistenceCondition::Writable,
                        artifact_warnings,
                        last_save_failure: None,
                    },
                ),
                BackupLoad::UnsupportedFutureSchema(found) => {
                    unsupported_future_load(found, artifact_warnings)
                }
                BackupLoad::Unusable => (
                    Settings::default(),
                    SettingsHealth {
                        load_condition: LoadCondition::FirstRun,
                        field_recoveries: Vec::new(),
                        persistence: PersistenceCondition::Writable,
                        artifact_warnings,
                        last_save_failure: None,
                    },
                ),
            }
        }
        Err(_) => unavailable_load(),
    }
}

fn malformed_or_backup(
    directory: &Path,
    primary: &Path,
    mut artifact_warnings: Vec<ArtifactWarning>,
) -> (Settings, SettingsHealth) {
    if quarantine(primary, directory).is_err() {
        artifact_warnings.push(ArtifactWarning::Quarantine);
    }
    if prune_quarantines(directory).is_err() {
        artifact_warnings.push(ArtifactWarning::QuarantineCleanup);
    }
    match load_backup(directory) {
        BackupLoad::Clean(snapshot) => (
            snapshot,
            SettingsHealth {
                load_condition: LoadCondition::RecoveredFromBackup,
                field_recoveries: Vec::new(),
                persistence: PersistenceCondition::Writable,
                artifact_warnings,
                last_save_failure: None,
            },
        ),
        BackupLoad::UnsupportedFutureSchema(found) => {
            unsupported_future_load(found, artifact_warnings)
        }
        BackupLoad::Unusable => (
            Settings::default(),
            SettingsHealth {
                load_condition: LoadCondition::MalformedDefaulted,
                field_recoveries: Vec::new(),
                persistence: PersistenceCondition::Writable,
                artifact_warnings,
                last_save_failure: None,
            },
        ),
    }
}

enum BackupLoad {
    Clean(Settings),
    UnsupportedFutureSchema(u64),
    Unusable,
}

fn load_backup(directory: &Path) -> BackupLoad {
    let Ok(bytes) = fs::read(directory.join(BACKUP_FILE)) else {
        return BackupLoad::Unusable;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return BackupLoad::Unusable;
    };
    if let Some(version) = value.get("schema_version").and_then(Value::as_u64)
        && version > SCHEMA_VERSION
    {
        return BackupLoad::UnsupportedFutureSchema(version);
    }
    let Some((settings, recoveries)) = Settings::from_document(&value) else {
        return BackupLoad::Unusable;
    };
    if recoveries.is_empty() {
        BackupLoad::Clean(settings)
    } else {
        BackupLoad::Unusable
    }
}

fn unsupported_future_load(
    found: u64,
    artifact_warnings: Vec<ArtifactWarning>,
) -> (Settings, SettingsHealth) {
    (
        Settings::default(),
        SettingsHealth {
            load_condition: LoadCondition::UnsupportedFutureSchema { found },
            field_recoveries: Vec::new(),
            persistence: PersistenceCondition::BlockedByFutureSchema,
            artifact_warnings,
            last_save_failure: None,
        },
    )
}

fn unavailable_load() -> (Settings, SettingsHealth) {
    (
        Settings::default(),
        SettingsHealth {
            load_condition: LoadCondition::PersistenceUnavailable,
            field_recoveries: Vec::new(),
            persistence: PersistenceCondition::Unavailable,
            artifact_warnings: Vec::new(),
            last_save_failure: None,
        },
    )
}

fn cleanup_stale_temporary_files(directory: &Path) -> Vec<ArtifactWarning> {
    let mut warnings = Vec::new();
    for name in [PRIMARY_TEMP_FILE, BACKUP_TEMP_FILE] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => warnings.push(ArtifactWarning::StaleTemporary),
        }
    }
    warnings
}

fn quarantine(primary: &Path, directory: &Path) -> std::io::Result<()> {
    fs::rename(primary, next_quarantine_path(directory))
}

fn preserve_source(primary: &Path, directory: &Path) -> std::io::Result<()> {
    let preserved = next_quarantine_path(directory);
    fs::copy(primary, &preserved)?;
    fs::OpenOptions::new()
        .write(true)
        .open(preserved)?
        .sync_all()
}

fn next_quarantine_path(directory: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NEXT_QUARANTINE.fetch_add(1, Ordering::Relaxed);
    directory.join(format!(
        "{QUARANTINE_PREFIX}{timestamp:020}.{sequence:020}.json"
    ))
}

fn prune_quarantines(directory: &Path) -> std::io::Result<()> {
    let mut paths = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(QUARANTINE_PREFIX))
        })
        .collect::<Vec<_>>();
    paths.sort();
    let remove_count = paths.len().saturating_sub(QUARANTINE_RETENTION);
    for path in paths.into_iter().take(remove_count) {
        fs::remove_file(path)?;
    }
    Ok(())
}

impl Settings {
    pub(crate) fn automatic_download_protection_enabled(&self) -> bool {
        self.automatic_download_protection_enabled
    }

    pub(crate) fn protect_on_battery(&self) -> bool {
        self.protect_on_battery
    }

    pub(crate) fn launch_at_login(&self) -> bool {
        self.launch_at_login
    }

    pub(crate) fn automatic_update_checks_enabled(&self) -> bool {
        self.automatic_update_checks_enabled
    }

    pub(crate) fn last_automatic_update_check_unix_seconds(&self) -> Option<u64> {
        self.last_automatic_update_check_unix_seconds
    }

    pub(crate) fn automatic_download(&self) -> AutomaticDownloadSettings {
        self.automatic_download
    }

    #[allow(dead_code, reason = "used by the explicit settings mutation seam")]
    fn apply(&mut self, update: SettingsUpdate) -> Result<(), SettingsUpdateError> {
        match update {
            SettingsUpdate::AutomaticDownloadProtectionEnabled(value) => {
                self.automatic_download_protection_enabled = value;
            }
            SettingsUpdate::ProtectOnBattery(value) => self.protect_on_battery = value,
            SettingsUpdate::LaunchAtLogin(value) => self.launch_at_login = value,
            SettingsUpdate::AutomaticUpdateChecksEnabled(value) => {
                self.automatic_update_checks_enabled = value;
            }
            SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(value) => {
                self.last_automatic_update_check_unix_seconds = value;
            }
            SettingsUpdate::AutomaticDownloadTuning {
                meaningful_receive_rate_bytes_per_second,
                activation_qualification_milliseconds,
                grace_milliseconds,
            } => {
                self.automatic_download = AutomaticDownloadSettings::try_new(
                    meaningful_receive_rate_bytes_per_second,
                    Duration::from_millis(activation_qualification_milliseconds),
                    Duration::from_millis(grace_milliseconds),
                )
                .map_err(SettingsUpdateError::InvalidAutomaticDownload)?;
            }
        }
        Ok(())
    }

    fn from_document(value: &Value) -> Option<(Self, Vec<FieldRecovery>)> {
        let object = value.as_object()?;
        if object.get("schema_version").and_then(Value::as_u64) != Some(SCHEMA_VERSION) {
            return None;
        }
        Some(Self::from_object(object))
    }

    fn from_unversioned_document(value: &Value) -> Option<(Self, Vec<FieldRecovery>)> {
        Some(Self::from_object(value.as_object()?))
    }

    fn from_object(object: &Map<String, Value>) -> (Self, Vec<FieldRecovery>) {
        let defaults = Self::default();
        let mut recoveries = Vec::new();
        let automatic_download_protection_enabled = read_bool(
            object,
            "automatic_download_protection_enabled",
            SettingPath::AutomaticDownloadProtectionEnabled,
            defaults.automatic_download_protection_enabled,
            &mut recoveries,
        );
        let protect_on_battery = read_bool(
            object,
            "protect_on_battery",
            SettingPath::ProtectOnBattery,
            defaults.protect_on_battery,
            &mut recoveries,
        );
        let launch_at_login = read_bool(
            object,
            "launch_at_login",
            SettingPath::LaunchAtLogin,
            defaults.launch_at_login,
            &mut recoveries,
        );
        let automatic_update_checks_enabled = read_bool(
            object,
            "automatic_update_checks_enabled",
            SettingPath::AutomaticUpdateChecksEnabled,
            defaults.automatic_update_checks_enabled,
            &mut recoveries,
        );
        let last_automatic_update_check_unix_seconds = read_optional_u64(
            object,
            "last_automatic_update_check_unix_seconds",
            SettingPath::LastAutomaticUpdateCheckUnixSeconds,
            defaults.last_automatic_update_check_unix_seconds,
            &mut recoveries,
        );
        let (detector, unavailable_detector_reason) = match object.get("automatic_download") {
            Some(Value::Object(detector)) => (Some(detector), RecoveryReason::Missing),
            Some(_) => (None, RecoveryReason::WrongType),
            None => (None, RecoveryReason::Missing),
        };
        let default_threshold = defaults
            .automatic_download
            .meaningful_receive_rate_bytes_per_second();
        let threshold = read_nested_u64(
            detector,
            "meaningful_receive_rate_bytes_per_second",
            SettingPath::MeaningfulReceiveRateBytesPerSecond,
            default_threshold,
            true,
            unavailable_detector_reason,
            &mut recoveries,
        );
        let default_activation: u64 = defaults
            .automatic_download
            .activation_qualification()
            .as_millis()
            .try_into()
            .expect("default activation fits milliseconds");
        let activation = read_nested_u64(
            detector,
            "activation_qualification_milliseconds",
            SettingPath::ActivationQualificationMilliseconds,
            default_activation,
            true,
            unavailable_detector_reason,
            &mut recoveries,
        );
        let default_grace: u64 = defaults
            .automatic_download
            .grace()
            .as_millis()
            .try_into()
            .expect("default grace fits milliseconds");
        let grace = read_nested_u64(
            detector,
            "grace_milliseconds",
            SettingPath::GraceMilliseconds,
            default_grace,
            false,
            unavailable_detector_reason,
            &mut recoveries,
        );
        let automatic_download = AutomaticDownloadSettings::try_new(
            threshold,
            Duration::from_millis(activation),
            Duration::from_millis(grace),
        )
        .expect("field recovery produces valid detector settings");
        (
            Self {
                automatic_download_protection_enabled,
                protect_on_battery,
                launch_at_login,
                automatic_update_checks_enabled,
                last_automatic_update_check_unix_seconds,
                automatic_download,
            },
            recoveries,
        )
    }

    #[allow(dead_code, reason = "used by the explicit settings mutation seam")]
    fn to_persisted(&self) -> PersistedSettingsV1 {
        PersistedSettingsV1 {
            schema_version: SCHEMA_VERSION,
            automatic_download_protection_enabled: self.automatic_download_protection_enabled,
            protect_on_battery: self.protect_on_battery,
            launch_at_login: self.launch_at_login,
            automatic_update_checks_enabled: self.automatic_update_checks_enabled,
            last_automatic_update_check_unix_seconds: self.last_automatic_update_check_unix_seconds,
            automatic_download: PersistedAutomaticDownload {
                meaningful_receive_rate_bytes_per_second: self
                    .automatic_download
                    .meaningful_receive_rate_bytes_per_second(),
                activation_qualification_milliseconds: self
                    .automatic_download
                    .activation_qualification()
                    .as_millis()
                    .try_into()
                    .expect("validated settings durations fit the persisted unit"),
                grace_milliseconds: self
                    .automatic_download
                    .grace()
                    .as_millis()
                    .try_into()
                    .expect("validated settings durations fit the persisted unit"),
            },
        }
    }
}

fn read_bool(
    object: &Map<String, Value>,
    key: &str,
    path: SettingPath,
    default: bool,
    recoveries: &mut Vec<FieldRecovery>,
) -> bool {
    match object.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            recoveries.push(FieldRecovery::new(path, RecoveryReason::WrongType));
            default
        }
        None => {
            recoveries.push(FieldRecovery::new(path, RecoveryReason::Missing));
            default
        }
    }
}

fn read_optional_u64(
    object: &Map<String, Value>,
    key: &str,
    path: SettingPath,
    default: Option<u64>,
    recoveries: &mut Vec<FieldRecovery>,
) -> Option<u64> {
    match object.get(key) {
        Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(value) => Some(value),
            None => {
                let reason = if value.is_number() {
                    RecoveryReason::InvalidValue
                } else {
                    RecoveryReason::WrongType
                };
                recoveries.push(FieldRecovery::new(path, reason));
                default
            }
        },
        None => {
            recoveries.push(FieldRecovery::new(path, RecoveryReason::Missing));
            default
        }
    }
}

fn read_nested_u64(
    object: Option<&Map<String, Value>>,
    key: &str,
    path: SettingPath,
    default: u64,
    non_zero: bool,
    unavailable_reason: RecoveryReason,
    recoveries: &mut Vec<FieldRecovery>,
) -> u64 {
    match object.and_then(|object| object.get(key)) {
        Some(value) => match value.as_u64() {
            Some(0) if non_zero => {
                recoveries.push(FieldRecovery::new(path, RecoveryReason::InvalidValue));
                default
            }
            Some(value) => value,
            None => {
                recoveries.push(FieldRecovery::new(path, RecoveryReason::WrongType));
                default
            }
        },
        None => {
            recoveries.push(FieldRecovery::new(path, unavailable_reason));
            default
        }
    }
}

#[cfg(test)]
mod tests;
