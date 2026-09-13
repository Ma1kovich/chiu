use super::*;
use std::{
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

fn unavailable_log() -> crate::local_log::LocalLog {
    crate::local_log::LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
}

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("chiu-settings-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("test settings directory should be created");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn first_run_uses_exact_defaults_without_creating_a_file() {
    let directory = TestDirectory::new();
    let (log, lines) = crate::local_log::LocalLog::recording();

    let service = SettingsService::load(directory.0.clone(), log);

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(service.health().load_condition(), LoadCondition::FirstRun);
    assert!(!directory.0.join("settings.json").exists());
    assert!(
        lines
            .lock()
            .unwrap()
            .join("")
            .contains("settings.loaded condition=first-run persistence=writable recoveries=0")
    );
}

#[test]
fn unavailable_construction_records_its_load_outcome() {
    let (log, lines) = crate::local_log::LocalLog::recording();

    SettingsService::unavailable(log);

    assert!(lines.lock().unwrap().join("").contains(
        "settings.loaded condition=persistence-unavailable persistence=unavailable recoveries=0"
    ));
}

#[test]
fn explicit_setting_changes_and_write_failures_use_safe_field_categories() {
    let directory = TestDirectory::new();
    let (log, lines) = crate::local_log::LocalLog::recording();
    let service = SettingsService::load(directory.0.clone(), log);
    service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .unwrap();

    let (failed_log, failed_lines) = crate::local_log::LocalLog::recording();
    let unavailable = SettingsService::unavailable(failed_log);
    assert!(
        unavailable
            .update(SettingsUpdate::LaunchAtLogin(false))
            .is_err()
    );

    assert!(
        lines
            .lock()
            .unwrap()
            .join("")
            .contains("settings.changed field=protect-on-battery")
    );
    assert!(
        failed_lines
            .lock()
            .unwrap()
            .join("")
            .contains("settings.write-failed field=launch-at-login")
    );
}

#[test]
fn every_v1_setting_survives_save_and_fresh_restore() {
    let directory = TestDirectory::new();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    service
        .update(SettingsUpdate::AutomaticDownloadProtectionEnabled(false))
        .unwrap();
    service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .unwrap();
    service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .unwrap();
    service
        .update(SettingsUpdate::AutomaticUpdateChecksEnabled(false))
        .unwrap();
    service
        .update(SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(Some(
            123,
        )))
        .unwrap();
    let expected = service
        .update(SettingsUpdate::AutomaticDownloadTuning {
            meaningful_receive_rate_bytes_per_second: 42,
            activation_qualification_milliseconds: 2_000,
            grace_milliseconds: 0,
        })
        .unwrap();

    let restored = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(restored.snapshot(), expected);
    assert_eq!(
        restored.health().load_condition(),
        LoadCondition::CurrentVersion
    );
    let document = fs::read_to_string(directory.0.join("settings.json")).unwrap();
    assert!(document.contains("\"schema_version\": 1"));
    assert!(!document.contains("manual"));
}

#[test]
fn invalid_fields_default_independently_without_discarding_valid_siblings() {
    let directory = TestDirectory::new();
    fs::write(
        directory.0.join("settings.json"),
        r#"{
          "schema_version": 1,
          "automatic_download_protection_enabled": false,
          "protect_on_battery": "sometimes",
          "automatic_update_checks_enabled": true,
          "last_automatic_update_check_unix_seconds": null,
          "automatic_download": {
            "meaningful_receive_rate_bytes_per_second": 0,
            "activation_qualification_milliseconds": 2000,
            "grace_milliseconds": 0,
            "future_detector_field": true
          },
          "future_top_level_field": true
        }"#,
    )
    .unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let snapshot = service.snapshot();

    assert!(!snapshot.automatic_download_protection_enabled);
    assert!(!snapshot.protect_on_battery);
    assert!(snapshot.launch_at_login);
    assert_eq!(
        snapshot
            .automatic_download
            .meaningful_receive_rate_bytes_per_second(),
        AutomaticDownloadSettings::default().meaningful_receive_rate_bytes_per_second()
    );
    assert_eq!(
        snapshot.automatic_download.activation_qualification(),
        Duration::from_secs(2)
    );
    assert_eq!(snapshot.automatic_download.grace(), Duration::ZERO);
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::PartialRecovery
    );
    assert_eq!(
        service.health().field_recoveries(),
        &[
            FieldRecovery::new(SettingPath::ProtectOnBattery, RecoveryReason::WrongType),
            FieldRecovery::new(SettingPath::LaunchAtLogin, RecoveryReason::Missing),
            FieldRecovery::new(
                SettingPath::MeaningfulReceiveRateBytesPerSecond,
                RecoveryReason::InvalidValue,
            ),
        ]
    );
}

#[test]
fn malformed_primary_recovers_valid_backup_and_quarantines_source() {
    let directory = TestDirectory::new();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let expected = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .unwrap();
    fs::copy(
        directory.0.join("settings.json"),
        directory.0.join("settings.json.bak"),
    )
    .unwrap();
    fs::write(directory.0.join("settings.json"), b"{not-json").unwrap();

    let restored = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(restored.snapshot(), expected);
    assert_eq!(
        restored.health().load_condition(),
        LoadCondition::RecoveredFromBackup
    );
    assert_eq!(quarantines(&directory.0).len(), 1);
}

#[test]
fn unsupported_future_schema_uses_defaults_and_blocks_all_writes() {
    let directory = TestDirectory::new();
    let future = br#"{
      "schema_version": 2,
      "protect_on_battery": true,
      "future_only": {"value": 42}
    }"#;
    fs::write(directory.0.join("settings.json"), future).unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let error = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .expect_err("future schema must block writes");

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::UnsupportedFutureSchema { found: 2 }
    );
    assert_eq!(
        service.health().persistence(),
        PersistenceCondition::BlockedByFutureSchema
    );
    assert!(matches!(
        error,
        SettingsUpdateError::UnsupportedFutureSchema(2)
    ));
    assert_eq!(fs::read(directory.0.join("settings.json")).unwrap(), future);
    assert!(quarantines(&directory.0).is_empty());
}

#[test]
fn unsupported_future_backup_is_not_overwritten_when_primary_is_absent() {
    let directory = TestDirectory::new();
    let future = br#"{
      "schema_version": 2,
      "future_only": {"value": 42}
    }"#;
    fs::write(directory.0.join("settings.json.bak"), future).unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let error = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .expect_err("future backup must block writes");

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::UnsupportedFutureSchema { found: 2 }
    );
    assert!(matches!(
        error,
        SettingsUpdateError::UnsupportedFutureSchema(2)
    ));
    assert_eq!(
        fs::read(directory.0.join("settings.json.bak")).unwrap(),
        future
    );
    assert!(!directory.0.join("settings.json").exists());
}

#[test]
fn missing_schema_salvages_fields_and_preserves_source_before_normalizing() {
    let directory = TestDirectory::new();
    let source = br#"{
      "automatic_download_protection_enabled": false,
      "protect_on_battery": true,
      "launch_at_login": true,
      "automatic_update_checks_enabled": true,
      "last_automatic_update_check_unix_seconds": null,
      "automatic_download": {
        "meaningful_receive_rate_bytes_per_second": 64,
        "activation_qualification_milliseconds": 1000,
        "grace_milliseconds": 0
      }
    }"#;
    fs::write(directory.0.join("settings.json"), source).unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    assert!(!service.snapshot().automatic_download_protection_enabled);
    assert!(service.snapshot().protect_on_battery);
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::SchemaMetadataRecovered
    );

    service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .unwrap();

    assert_eq!(quarantines(&directory.0).len(), 1);
    assert_eq!(fs::read(&quarantines(&directory.0)[0]).unwrap(), source);
    let normalized: Value =
        serde_json::from_slice(&fs::read(directory.0.join("settings.json")).unwrap()).unwrap();
    assert_eq!(normalized["schema_version"], 1);
}

#[test]
fn failed_normalization_keeps_salvaged_primary_restart_recoverable() {
    let directory = TestDirectory::new();
    let source = br#"{
      "automatic_download_protection_enabled": false,
      "protect_on_battery": true,
      "launch_at_login": true,
      "automatic_update_checks_enabled": true,
      "last_automatic_update_check_unix_seconds": null,
      "automatic_download": {
        "meaningful_receive_rate_bytes_per_second": 64,
        "activation_qualification_milliseconds": 1000,
        "grace_milliseconds": 0
      }
    }"#;
    fs::write(directory.0.join("settings.json"), source).unwrap();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let recovered = service.snapshot();
    fs::create_dir(directory.0.join("settings.json.tmp")).unwrap();

    let error = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .expect_err("normalization write should fail");

    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::WritePrimary)
    ));
    assert_eq!(service.snapshot(), recovered);
    assert_eq!(
        SettingsService::load(directory.0.clone(), unavailable_log()).snapshot(),
        recovered
    );
    assert_eq!(fs::read(directory.0.join("settings.json")).unwrap(), source);
}

struct FailReplacement {
    calls: AtomicU64,
    fail_at: u64,
}

impl AtomicReplace for FailReplacement {
    fn replace(&self, source: &std::path::Path, destination: &std::path::Path) -> io::Result<()> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == self.fail_at {
            return Err(io::Error::other("scripted replacement failure"));
        }
        platform_atomic_replace(source, destination)
    }
}

#[test]
fn failed_primary_replacement_keeps_memory_and_last_known_good_files() {
    let directory = TestDirectory::new();
    let replacer = Arc::new(FailReplacement {
        calls: AtomicU64::new(0),
        fail_at: 3,
    });
    let service =
        SettingsService::load_with_replacer(directory.0.clone(), replacer, unavailable_log());
    let persisted = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .unwrap();

    let error = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .expect_err("primary replacement should fail");

    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::ReplacePrimary)
    ));
    assert_eq!(service.snapshot(), persisted);
    assert_eq!(
        service.health().last_save_failure(),
        Some(SettingsPersistenceFailure::ReplacePrimary)
    );
    assert_eq!(
        SettingsService::load(directory.0.clone(), unavailable_log()).snapshot(),
        persisted
    );
    assert!(directory.0.join("settings.json.bak").exists());

    let retried = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .expect("retry should succeed after the scripted failure");
    assert_eq!(
        SettingsService::load(directory.0.clone(), unavailable_log()).snapshot(),
        retried
    );
}

#[test]
fn failed_backup_replacement_keeps_primary_and_allows_restart_and_retry() {
    let directory = TestDirectory::new();
    let replacer = Arc::new(FailReplacement {
        calls: AtomicU64::new(0),
        fail_at: 2,
    });
    let service =
        SettingsService::load_with_replacer(directory.0.clone(), replacer, unavailable_log());
    let persisted = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .unwrap();

    let error = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .expect_err("backup replacement should fail");

    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::ReplaceBackup)
    ));
    assert_eq!(service.snapshot(), persisted);
    assert_eq!(
        SettingsService::load(directory.0.clone(), unavailable_log()).snapshot(),
        persisted
    );

    let retried = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .expect("retry should succeed after the scripted failure");
    assert_eq!(
        SettingsService::load(directory.0.clone(), unavailable_log()).snapshot(),
        retried
    );
}

#[test]
fn malformed_primary_without_backup_defaults_and_preserves_source() {
    let directory = TestDirectory::new();
    let source = b"not valid json";
    fs::write(directory.0.join("settings.json"), source).unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::MalformedDefaulted
    );
    assert_eq!(fs::read(&quarantines(&directory.0)[0]).unwrap(), source);
}

#[test]
fn absent_primary_recovers_last_known_good_backup() {
    let directory = TestDirectory::new();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let expected = service
        .update(SettingsUpdate::LaunchAtLogin(false))
        .unwrap();
    fs::copy(
        directory.0.join("settings.json"),
        directory.0.join("settings.json.bak"),
    )
    .unwrap();
    fs::remove_file(directory.0.join("settings.json")).unwrap();

    let restored = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(restored.snapshot(), expected);
    assert_eq!(
        restored.health().load_condition(),
        LoadCondition::RecoveredFromBackup
    );
}

#[test]
fn unreadable_primary_defaults_and_makes_later_writes_observably_unavailable() {
    let directory = TestDirectory::new();
    fs::create_dir(directory.0.join("settings.json")).unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    let error = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .expect_err("unavailable persistence must reject writes");

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::PersistenceUnavailable
    );
    assert_eq!(
        service.health().persistence(),
        PersistenceCondition::Unavailable
    );
    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::Unavailable)
    ));
}

#[test]
fn stale_temporary_files_are_removed_without_affecting_first_run() {
    let directory = TestDirectory::new();
    fs::write(directory.0.join("settings.json.tmp"), b"stale").unwrap();
    fs::write(directory.0.join("settings.json.bak.tmp"), b"stale").unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(service.health().load_condition(), LoadCondition::FirstRun);
    assert!(!directory.0.join("settings.json.tmp").exists());
    assert!(!directory.0.join("settings.json.bak.tmp").exists());
}

#[test]
fn corrupt_quarantine_retention_is_bounded_to_two_files() {
    let directory = TestDirectory::new();
    for sequence in 0..3 {
        fs::write(
            directory.0.join("settings.json"),
            format!("malformed-{sequence}"),
        )
        .unwrap();
        let service = SettingsService::load(directory.0.clone(), unavailable_log());
        assert_eq!(
            service.health().load_condition(),
            LoadCondition::MalformedDefaulted
        );
    }

    assert_eq!(quarantines(&directory.0).len(), 2);
}

#[test]
fn invalid_detector_update_neither_writes_nor_changes_memory() {
    let directory = TestDirectory::new();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    let error = service
        .update(SettingsUpdate::AutomaticDownloadTuning {
            meaningful_receive_rate_bytes_per_second: 0,
            activation_qualification_milliseconds: 1,
            grace_milliseconds: 0,
        })
        .expect_err("zero threshold should remain invalid");

    assert!(matches!(
        error,
        SettingsUpdateError::InvalidAutomaticDownload(
            AutomaticDownloadSettingsError::ZeroMeaningfulReceiveRate
        )
    ));
    assert_eq!(service.snapshot(), Settings::default());
    assert!(!directory.0.join("settings.json").exists());
}

#[test]
fn relative_settings_location_is_rejected_without_touching_the_working_directory() {
    let relative = PathBuf::from("relative-settings-directory");

    let service = SettingsService::load(relative.clone(), unavailable_log());
    let error = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .expect_err("relative persistence must be unavailable");

    assert_eq!(
        service.health().persistence(),
        PersistenceCondition::Unavailable
    );
    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::Unavailable)
    ));
    assert!(!relative.exists());
}

#[test]
fn zero_activation_defaults_only_activation_while_zero_grace_remains_valid() {
    let directory = TestDirectory::new();
    fs::write(
        directory.0.join("settings.json"),
        r#"{
          "schema_version": 1,
          "automatic_download_protection_enabled": true,
          "protect_on_battery": false,
          "launch_at_login": true,
          "automatic_update_checks_enabled": true,
          "last_automatic_update_check_unix_seconds": null,
          "automatic_download": {
            "meaningful_receive_rate_bytes_per_second": 99,
            "activation_qualification_milliseconds": 0,
            "grace_milliseconds": 0
          }
        }"#,
    )
    .unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(
        service
            .snapshot()
            .automatic_download
            .meaningful_receive_rate_bytes_per_second(),
        99
    );
    assert_eq!(
        service
            .snapshot()
            .automatic_download
            .activation_qualification(),
        AutomaticDownloadSettings::default().activation_qualification()
    );
    assert_eq!(
        service.snapshot().automatic_download.grace(),
        Duration::ZERO
    );
    assert_eq!(
        service.health().field_recoveries(),
        &[FieldRecovery::new(
            SettingPath::ActivationQualificationMilliseconds,
            RecoveryReason::InvalidValue,
        )]
    );
}

#[test]
fn invalid_schema_metadata_and_timestamp_are_salvaged_with_typed_warnings() {
    let directory = TestDirectory::new();
    fs::write(
        directory.0.join("settings.json"),
        r#"{
          "schema_version": "one",
          "automatic_download_protection_enabled": false,
          "protect_on_battery": false,
          "launch_at_login": true,
          "automatic_update_checks_enabled": true,
          "last_automatic_update_check_unix_seconds": -1,
          "automatic_download": {
            "meaningful_receive_rate_bytes_per_second": 1,
            "activation_qualification_milliseconds": 1,
            "grace_milliseconds": 1
          }
        }"#,
    )
    .unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    assert!(!service.snapshot().automatic_download_protection_enabled);
    assert_eq!(
        service.health().load_condition(),
        LoadCondition::SchemaMetadataRecovered
    );
    assert_eq!(
        service.health().field_recoveries(),
        &[FieldRecovery::new(
            SettingPath::LastAutomaticUpdateCheckUnixSeconds,
            RecoveryReason::InvalidValue,
        )]
    );
}

#[test]
fn temporary_write_failure_preserves_memory_and_is_reported_in_health() {
    let directory = TestDirectory::new();
    fs::create_dir(directory.0.join("settings.json.tmp")).unwrap();
    let service = SettingsService::load(directory.0.clone(), unavailable_log());
    assert_eq!(
        service.health().artifact_warnings(),
        &[ArtifactWarning::StaleTemporary]
    );

    let error = service
        .update(SettingsUpdate::ProtectOnBattery(true))
        .expect_err("temporary file should be unwritable");

    assert!(matches!(
        error,
        SettingsUpdateError::Persistence(SettingsPersistenceFailure::WritePrimary)
    ));
    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().last_save_failure(),
        Some(SettingsPersistenceFailure::WritePrimary)
    );
}

#[test]
fn wrong_typed_detector_object_reports_each_affected_leaf() {
    let directory = TestDirectory::new();
    fs::write(
        directory.0.join("settings.json"),
        r#"{
          "schema_version": 1,
          "automatic_download_protection_enabled": true,
          "protect_on_battery": false,
          "launch_at_login": true,
          "automatic_update_checks_enabled": true,
          "last_automatic_update_check_unix_seconds": null,
          "automatic_download": "fast"
        }"#,
    )
    .unwrap();

    let service = SettingsService::load(directory.0.clone(), unavailable_log());

    assert_eq!(service.snapshot(), Settings::default());
    assert_eq!(
        service.health().field_recoveries(),
        &[
            FieldRecovery::new(
                SettingPath::MeaningfulReceiveRateBytesPerSecond,
                RecoveryReason::WrongType,
            ),
            FieldRecovery::new(
                SettingPath::ActivationQualificationMilliseconds,
                RecoveryReason::WrongType,
            ),
            FieldRecovery::new(SettingPath::GraceMilliseconds, RecoveryReason::WrongType),
        ]
    );
}

fn quarantines(directory: &std::path::Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("settings.corrupt.")
        })
        .collect()
}
