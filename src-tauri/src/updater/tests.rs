use super::*;
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

fn unavailable_log() -> LocalLog {
    LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("chiu-updater-{name}-{}", std::process::id()));
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

struct FakeClock(u64);

impl Clock for FakeClock {
    fn unix_seconds(&self) -> u64 {
        self.0
    }
}

struct FakeUpdate {
    version: String,
    install_result: Result<(), UpdateFailureStage>,
    installs: Arc<AtomicUsize>,
}

impl PendingUpdate for FakeUpdate {
    fn announced_version(&self) -> &str {
        &self.version
    }

    fn download(
        &mut self,
        progress: &mut dyn FnMut(usize, Option<u64>),
        download_finished: &mut dyn FnMut(),
    ) -> Result<Vec<u8>, UpdateFailureStage> {
        progress(5, Some(10));
        progress(20, Some(10));
        download_finished();
        match self.install_result {
            Err(stage @ (UpdateFailureStage::Download | UpdateFailureStage::Verification)) => {
                Err(stage)
            }
            _ => Ok(vec![1, 2, 3]),
        }
    }

    fn install(&mut self, _: Vec<u8>) -> Result<(), UpdateFailureStage> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        self.install_result
    }
}

enum FakeCheck {
    Current,
    Available(
        &'static str,
        Result<(), UpdateFailureStage>,
        Arc<AtomicUsize>,
    ),
    Failed,
}

struct FakeBackend {
    checks: Arc<AtomicUsize>,
    results: VecDeque<FakeCheck>,
}

struct BlockingBackend {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

struct BlockingUpdateBackend(Option<BlockingUpdate>);

struct BlockingUpdate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    installs: Arc<AtomicUsize>,
}

impl UpdateBackend for BlockingBackend {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()> {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        Ok(None)
    }
}

impl UpdateBackend for BlockingUpdateBackend {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()> {
        Ok(self
            .0
            .take()
            .map(|update| Box::new(update) as Box<dyn PendingUpdate>))
    }
}

impl PendingUpdate for BlockingUpdate {
    fn announced_version(&self) -> &str {
        "1.2.3"
    }

    fn download(
        &mut self,
        progress: &mut dyn FnMut(usize, Option<u64>),
        download_finished: &mut dyn FnMut(),
    ) -> Result<Vec<u8>, UpdateFailureStage> {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        progress(3, Some(3));
        download_finished();
        Ok(vec![1, 2, 3])
    }

    fn install(&mut self, _: Vec<u8>) -> Result<(), UpdateFailureStage> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl UpdateBackend for FakeBackend {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        match self.results.pop_front().unwrap_or(FakeCheck::Current) {
            FakeCheck::Current => Ok(None),
            FakeCheck::Available(version, install_result, installs) => {
                Ok(Some(Box::new(FakeUpdate {
                    version: version.to_owned(),
                    install_result,
                    installs,
                })))
            }
            FakeCheck::Failed => Err(()),
        }
    }
}

type Confirmation = Box<dyn FnOnce(bool) + Send>;

#[derive(Default)]
struct FakePresentation {
    information: AtomicUsize,
    errors: AtomicUsize,
    confirmations: Mutex<Vec<Confirmation>>,
}

impl FakePresentation {
    fn answer(&self, answer: bool) {
        self.confirmations.lock().unwrap().remove(0)(answer);
    }
}

impl UpdatePresentation for FakePresentation {
    fn information(&self, _: &'static str) {
        self.information.fetch_add(1, Ordering::SeqCst);
    }

    fn error(&self, _: &'static str) {
        self.errors.fetch_add(1, Ordering::SeqCst);
    }

    fn confirm_install(&self, _: Option<String>, callback: Box<dyn FnOnce(bool) + Send>) {
        self.confirmations.lock().unwrap().push(callback);
    }
}

#[derive(Default)]
struct FakeInstalledUpdate(AtomicUsize);

impl InstalledUpdate for FakeInstalledUpdate {
    fn finish(&self) -> Result<(), ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct SelfShutdownInstalled {
    runtime: Arc<Mutex<Option<UpdaterRuntime>>>,
    calls: Arc<AtomicUsize>,
}

impl InstalledUpdate for SelfShutdownInstalled {
    fn finish(&self) -> Result<(), ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.runtime
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .shutdown()
            .unwrap();
        Ok(())
    }
}

fn available(
    settings: SettingsService,
    now: u64,
    results: Vec<FakeCheck>,
) -> (
    UpdaterService,
    UpdaterRuntime,
    Arc<AtomicUsize>,
    Arc<FakePresentation>,
    Arc<FakeInstalledUpdate>,
) {
    available_with_log(
        settings,
        now,
        results,
        LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open),
    )
}

fn available_with_log(
    settings: SettingsService,
    now: u64,
    results: Vec<FakeCheck>,
    log: LocalLog,
) -> (
    UpdaterService,
    UpdaterRuntime,
    Arc<AtomicUsize>,
    Arc<FakePresentation>,
    Arc<FakeInstalledUpdate>,
) {
    let checks = Arc::new(AtomicUsize::new(0));
    let presentation = Arc::new(FakePresentation::default());
    let installed = Arc::new(FakeInstalledUpdate::default());
    let (service, runtime) = UpdaterService::available(
        settings,
        Arc::new(FakeClock(now)),
        Box::new(FakeBackend {
            checks: checks.clone(),
            results: results.into(),
        }),
        presentation.clone(),
        installed.clone(),
        log,
    )
    .unwrap();
    (service, runtime, checks, presentation, installed)
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn first_usable_startup_claims_the_cadence_before_automatic_discovery() {
    let directory = TestDirectory::new("first-start");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let (updater, mut runtime, checks, _, _) = available(settings.clone(), 1_000, vec![]);

    updater.activate();
    wait_until(|| checks.load(Ordering::SeqCst) == 1);

    assert_eq!(
        settings
            .snapshot()
            .last_automatic_update_check_unix_seconds(),
        Some(1_000)
    );
    runtime.shutdown().unwrap();
}

#[test]
fn automatic_cadence_save_failure_prevents_the_request() {
    let (updater, mut runtime, checks, _, _) = available(
        SettingsService::unavailable(unavailable_log()),
        1_000,
        vec![],
    );

    updater.activate();
    wait_until(|| {
        matches!(
            updater.snapshot().status,
            UpdateStatus::Failed {
                stage: UpdateFailureStage::Scheduling,
                trigger: UpdateTrigger::Automatic,
                ..
            }
        )
    });

    assert_eq!(checks.load(Ordering::SeqCst), 0);
    runtime.shutdown().unwrap();
}

#[test]
fn manual_check_bypasses_cadence_and_survives_cadence_save_failure() {
    let (updater, mut runtime, checks, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![],
    );

    updater.check_manually();
    wait_until(|| updater.snapshot().status == UpdateStatus::UpToDate);

    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(presentation.information.load(Ordering::SeqCst), 1);
    runtime.shutdown().unwrap();
}

#[test]
fn cadence_due_policy_handles_exact_interval_and_future_clock() {
    assert!(matches!(automatic_due(None, 100), AutomaticDue::Now));
    assert!(matches!(
        automatic_due(Some(100), 100 + 86_399),
        AutomaticDue::Later(_)
    ));
    assert!(matches!(
        automatic_due(Some(100), 100 + 86_400),
        AutomaticDue::Now
    ));
    assert!(matches!(
        automatic_due(Some(101), 100),
        AutomaticDue::Future
    ));
}

#[test]
fn future_cadence_is_normalized_without_an_immediate_request() {
    let directory = TestDirectory::new("future-cadence");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    settings
        .update(SettingsUpdate::LastAutomaticUpdateCheckUnixSeconds(Some(
            2_000,
        )))
        .unwrap();
    let (updater, mut runtime, checks, _, _) = available(settings.clone(), 1_000, vec![]);

    updater.activate();
    wait_until(|| {
        settings
            .snapshot()
            .last_automatic_update_check_unix_seconds()
            == Some(1_000)
    });

    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(updater.snapshot().status, UpdateStatus::Idle);
    runtime.shutdown().unwrap();
}

#[test]
fn automatic_failure_is_nonmodal_and_still_consumes_the_interval() {
    let directory = TestDirectory::new("automatic-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let (updater, mut runtime, checks, presentation, _) =
        available(settings.clone(), 3_000, vec![FakeCheck::Failed]);

    updater.activate();
    wait_until(|| matches!(updater.snapshot().status, UpdateStatus::Failed { .. }));
    updater.activate();
    updater.synchronize();

    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(presentation.errors.load(Ordering::SeqCst), 0);
    assert_eq!(presentation.information.load(Ordering::SeqCst), 0);
    assert_eq!(
        settings
            .snapshot()
            .last_automatic_update_check_unix_seconds(),
        Some(3_000)
    );
    runtime.shutdown().unwrap();
}

#[test]
fn enabling_automatic_checks_re_evaluates_due_work_from_persisted_preference() {
    let directory = TestDirectory::new("enable-automatic");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    settings
        .update(SettingsUpdate::AutomaticUpdateChecksEnabled(false))
        .unwrap();
    let (updater, mut runtime, checks, _, _) = available(settings, 4_000, vec![]);

    updater.activate();
    updater.synchronize();
    assert_eq!(checks.load(Ordering::SeqCst), 0);

    updater.toggle_automatic_checks();
    wait_until(|| checks.load(Ordering::SeqCst) == 1);

    assert!(updater.snapshot().automatic_checks_enabled);
    runtime.shutdown().unwrap();
}

#[test]
fn failed_preference_save_leaves_the_effective_checkmark_unchanged() {
    let settings = SettingsService::unavailable(unavailable_log());
    let (updater, mut runtime, _, _, _) = available(settings, 4_000, vec![]);

    updater.toggle_automatic_checks();
    updater.synchronize();

    assert!(updater.snapshot().automatic_checks_enabled);
    runtime.shutdown().unwrap();
}

#[test]
fn manual_update_requires_confirmation_and_cancel_retains_it() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![FakeCheck::Available("1.2.3", Ok(()), installs.clone())],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    updater.check_manually();
    presentation.answer(false);
    wait_until(|| {
        matches!(
            updater.snapshot().status,
            UpdateStatus::UpdateAvailable { .. }
        )
    });

    assert_eq!(installs.load(Ordering::SeqCst), 0);
    runtime.shutdown().unwrap();
}

#[test]
fn confirmed_update_installs_once_with_bounded_progress_and_finishes() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, installed) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![FakeCheck::Available("1.2.3", Ok(()), installs.clone())],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(true);
    wait_until(|| installed.0.load(Ordering::SeqCst) == 1);

    assert_eq!(installs.load(Ordering::SeqCst), 1);
    assert!(matches!(
        updater.snapshot().status,
        UpdateStatus::Installing { .. }
    ));
    runtime.shutdown().unwrap();
}

#[test]
fn failed_install_remains_retryable_with_fresh_confirmation() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![FakeCheck::Available(
            "1.2.3",
            Err(UpdateFailureStage::Install),
            installs.clone(),
        )],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(true);
    wait_until(|| matches!(updater.snapshot().status, UpdateStatus::Failed { .. }));
    updater.request_install();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);

    assert_eq!(installs.load(Ordering::SeqCst), 1);
    assert_eq!(presentation.errors.load(Ordering::SeqCst), 1);
    presentation.answer(false);
    runtime.shutdown().unwrap();
}

#[test]
fn verification_failure_never_reaches_install_and_remains_retryable() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![FakeCheck::Available(
            "1.2.3",
            Err(UpdateFailureStage::Verification),
            installs.clone(),
        )],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(true);
    wait_until(|| matches!(updater.snapshot().status, UpdateStatus::Failed { .. }));
    updater.request_install();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);

    assert_eq!(installs.load(Ordering::SeqCst), 0);
    assert_eq!(presentation.errors.load(Ordering::SeqCst), 1);
    presentation.answer(false);
    runtime.shutdown().unwrap();
}

#[test]
fn manual_recheck_failure_preserves_an_existing_pending_update() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![
            FakeCheck::Available("1.2.3", Ok(()), installs),
            FakeCheck::Failed,
        ],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(false);
    wait_until(|| {
        matches!(
            updater.snapshot().status,
            UpdateStatus::UpdateAvailable { .. }
        )
    });
    updater.check_manually();
    wait_until(|| matches!(updater.snapshot().status, UpdateStatus::Failed { .. }));

    assert!(matches!(
        updater.snapshot().status,
        UpdateStatus::Failed {
            pending: Some(PendingUpdateView::Version(ref version)),
            ..
        } if version == "1.2.3"
    ));
    runtime.shutdown().unwrap();
}

#[test]
fn manual_no_update_recheck_clears_an_existing_pending_update() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![
            FakeCheck::Available("1.2.3", Ok(()), installs),
            FakeCheck::Current,
        ],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(false);
    wait_until(|| {
        matches!(
            updater.snapshot().status,
            UpdateStatus::UpdateAvailable { .. }
        )
    });
    updater.check_manually();
    wait_until(|| updater.snapshot().status == UpdateStatus::UpToDate);
    updater.request_install();
    updater.synchronize();

    assert!(presentation.confirmations.lock().unwrap().is_empty());
    runtime.shutdown().unwrap();
}

#[test]
fn manual_check_trigger_and_result_are_logged_as_bounded_events() {
    let (log, lines) = crate::local_log::LocalLog::recording();
    let (updater, mut runtime, _, _, _) = available_with_log(
        SettingsService::unavailable(log.clone()),
        2_000,
        vec![FakeCheck::Current],
        log,
    );

    updater.check_manually();
    wait_until(|| updater.snapshot().status == UpdateStatus::UpToDate);
    wait_until(|| {
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("stage=up-to-date"))
    });

    let persisted = lines.lock().unwrap().join("");
    assert!(persisted.contains("updater action=manual-check outcome=succeeded stage=started"));
    assert!(persisted.contains("updater action=check outcome=succeeded stage=up-to-date"));
    runtime.shutdown().unwrap();
}

#[test]
fn shutdown_rejects_late_confirmation() {
    let installs = Arc::new(AtomicUsize::new(0));
    let (updater, mut runtime, _, presentation, _) = available(
        SettingsService::unavailable(unavailable_log()),
        2_000,
        vec![FakeCheck::Available("1.2.3", Ok(()), installs.clone())],
    );

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    updater.begin_shutdown();
    presentation.answer(true);
    runtime.shutdown().unwrap();

    assert_eq!(installs.load(Ordering::SeqCst), 0);
}

#[test]
fn shutdown_suppresses_a_late_check_completion_and_dialog() {
    let settings = SettingsService::unavailable(unavailable_log());
    let presentation = Arc::new(FakePresentation::default());
    let installed = Arc::new(FakeInstalledUpdate::default());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (updater, mut runtime) = UpdaterService::available(
        settings,
        Arc::new(FakeClock(2_000)),
        Box::new(BlockingBackend {
            entered: entered_tx,
            release: release_rx,
        }),
        presentation.clone(),
        installed,
        unavailable_log(),
    )
    .unwrap();

    updater.check_manually();
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    updater.begin_shutdown();
    release_tx.send(()).unwrap();
    runtime.shutdown().unwrap();

    assert_eq!(presentation.information.load(Ordering::SeqCst), 0);
    assert_eq!(presentation.errors.load(Ordering::SeqCst), 0);
    assert!(presentation.confirmations.lock().unwrap().is_empty());
}

#[test]
fn shutdown_during_download_prevents_install_and_restart() {
    let presentation = Arc::new(FakePresentation::default());
    let installed = Arc::new(FakeInstalledUpdate::default());
    let installs = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (updater, mut runtime) = UpdaterService::available(
        SettingsService::unavailable(unavailable_log()),
        Arc::new(FakeClock(2_000)),
        Box::new(BlockingUpdateBackend(Some(BlockingUpdate {
            entered: entered_tx,
            release: release_rx,
            installs: installs.clone(),
        }))),
        presentation.clone(),
        installed.clone(),
        unavailable_log(),
    )
    .unwrap();

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(true);
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    updater.begin_shutdown();
    release_tx.send(()).unwrap();
    runtime.shutdown().unwrap();

    assert_eq!(installs.load(Ordering::SeqCst), 0);
    assert_eq!(installed.0.load(Ordering::SeqCst), 0);
}

#[test]
fn updater_worker_shutdown_skips_only_its_own_join() {
    let installs = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(AtomicUsize::new(0));
    let presentation = Arc::new(FakePresentation::default());
    let runtime_owner = Arc::new(Mutex::new(None));
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let (updater, runtime) = UpdaterService::available(
        SettingsService::unavailable(unavailable_log()),
        Arc::new(FakeClock(2_000)),
        Box::new(FakeBackend {
            checks,
            results: vec![FakeCheck::Available("1.2.3", Ok(()), installs)].into(),
        }),
        presentation.clone(),
        Arc::new(SelfShutdownInstalled {
            runtime: runtime_owner.clone(),
            calls: finish_calls.clone(),
        }),
        unavailable_log(),
    )
    .unwrap();
    let worker_exit = runtime.worker_exit.clone();
    *runtime_owner.lock().unwrap() = Some(runtime);

    updater.check_manually();
    wait_until(|| presentation.confirmations.lock().unwrap().len() == 1);
    presentation.answer(true);
    wait_until(|| finish_calls.load(Ordering::SeqCst) == 1);
    wait_until(|| worker_exit.is_finished());

    assert!(!updater.accepting.load(Ordering::Acquire));
}

#[test]
fn unconfigured_capability_is_nonfatal_and_accepts_no_commands() {
    let settings = SettingsService::unavailable(unavailable_log());
    let updater = UpdaterService::unconfigured(&settings);

    updater.activate();
    updater.check_manually();
    updater.request_install();

    assert_eq!(
        updater.snapshot().availability,
        UpdateAvailability::Unconfigured
    );
    assert_eq!(updater.snapshot().status, UpdateStatus::Idle);
}

#[test]
fn updater_configuration_requires_complete_safe_release_inputs() {
    assert!(matches!(
        configured_updater(None).unwrap(),
        ConfiguredUpdater::Unconfigured
    ));
    assert!(configured_updater(Some(&serde_json::json!({"endpoints": []}))).is_err());
    assert!(
        configured_updater(Some(&serde_json::json!({
            "endpoints": ["http://example.test/latest.json"],
            "pubkey": "key"
        })))
        .is_err()
    );
    assert!(
        configured_updater(Some(&serde_json::json!({
            "endpoints": ["https://example.test/latest.json"],
            "pubkey": "key",
            "dangerousAcceptInvalidCerts": true
        })))
        .is_err()
    );
    assert!(matches!(
        configured_updater(Some(&serde_json::json!({
            "endpoints": ["https://example.test/latest.json"],
            "pubkey": "key"
        })))
        .unwrap(),
        ConfiguredUpdater::Configured
    ));
}

#[test]
fn shipped_updater_configuration_uses_the_prerelease_metadata_channel() {
    let configuration: serde_json::Value =
        serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
    let updater = configuration
        .get("plugins")
        .and_then(|plugins| plugins.get("updater"));

    assert!(matches!(
        configured_updater(updater).unwrap(),
        ConfiguredUpdater::Configured
    ));
    assert_eq!(
        configuration
            .pointer("/bundle/createUpdaterArtifacts")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        updater
            .and_then(|updater| updater.get("endpoints"))
            .and_then(serde_json::Value::as_array)
            .map(|endpoints| endpoints.as_slice()),
        Some(
            [serde_json::Value::String(
                "https://github.com/Ma1kovich/chiu/releases/download/updater/latest.json"
                    .to_owned()
            )]
            .as_slice()
        )
    );
}

#[test]
fn remote_version_and_progress_are_bounded_for_presentation() {
    assert_eq!(
        display_version("1.2.3-beta+4"),
        Some("1.2.3-beta+4".to_owned())
    );
    assert_eq!(display_version("<script>"), None);
    assert_eq!(display_version(&"1".repeat(65)), None);
    assert_eq!(bounded_percent(5, Some(10)), Some(50));
    assert_eq!(bounded_percent(20, Some(10)), Some(100));
    assert_eq!(bounded_percent(5, Some(0)), None);
    assert_eq!(bounded_percent(5, None), None);
}
