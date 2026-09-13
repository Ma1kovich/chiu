use super::*;

fn known_match(snapshot: &LaunchAtLoginSnapshot) -> bool {
    matches!(
        (snapshot.desired(), snapshot.observed()),
        (true, Registration::Enabled) | (false, Registration::Disabled)
    )
}
use crate::settings::{SettingsService, SettingsUpdate};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
};

fn unavailable_log() -> LocalLog {
    LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AdapterCall {
    Observe,
    SetEnabled(bool),
}

#[derive(Clone)]
struct FakeAdapter {
    observed: Arc<Mutex<Vec<Result<bool, String>>>>,
    set_results: Arc<Mutex<Vec<Result<(), String>>>>,
    calls: Arc<Mutex<Vec<AdapterCall>>>,
}

impl FakeAdapter {
    fn new(observed: Vec<Result<bool, String>>) -> Self {
        Self {
            observed: Arc::new(Mutex::new(observed.into_iter().rev().collect())),
            set_results: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_set_results(mut self, results: Vec<Result<(), String>>) -> Self {
        self.set_results = Arc::new(Mutex::new(results.into_iter().rev().collect()));
        self
    }

    fn calls(&self) -> Vec<AdapterCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl LaunchAtLoginAdapter for FakeAdapter {
    fn observe(&self) -> Result<bool, String> {
        self.calls.lock().unwrap().push(AdapterCall::Observe);
        self.observed
            .lock()
            .unwrap()
            .pop()
            .expect("fake observation exhausted")
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(AdapterCall::SetEnabled(enabled));
        self.set_results.lock().unwrap().pop().unwrap_or(Ok(()))
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "chiu-launch-at-login-{name}-{}",
            std::process::id()
        ));
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
fn default_preference_enables_disabled_registration_and_verifies() {
    let directory = TestDirectory::new("enable-default");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Ok(true)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let snapshot = launch_at_login.reconcile().unwrap();

    assert!(snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Enabled);
    assert!(snapshot.available());
    assert!(known_match(&snapshot));
    assert!(snapshot.latest_failure().is_none());
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn reconciliation_outcome_is_logged_without_native_error_text() {
    let directory = TestDirectory::new("logged-failure");
    let (log, lines) = crate::local_log::LocalLog::recording();
    let settings = SettingsService::load(directory.0.clone(), log.clone());
    let adapter = FakeAdapter::new(vec![Ok(false), Err("/Users/alice/private".to_owned())]);
    let launch_at_login = LaunchAtLoginService::available(settings, adapter, log);

    assert!(launch_at_login.reconcile().is_err());

    let persisted = lines.lock().unwrap().join("");
    assert!(persisted.contains("launch-at-login action=reconcile outcome=failed stage=verify"));
    assert!(!persisted.contains("/Users/alice/private"));
}

#[test]
fn persisted_false_disables_enabled_registration_and_verifies() {
    let directory = TestDirectory::new("disable-persisted");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    settings
        .update(SettingsUpdate::LaunchAtLogin(false))
        .unwrap();
    let adapter = FakeAdapter::new(vec![Ok(true), Ok(false)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let snapshot = launch_at_login.reconcile().unwrap();

    assert!(!snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Disabled);
    assert!(known_match(&snapshot));
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(false),
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn matching_registration_does_not_mutate_the_platform() {
    let directory = TestDirectory::new("already-matching");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(true)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let snapshot = launch_at_login.reconcile().unwrap();

    assert!(known_match(&snapshot));
    assert_eq!(adapter.calls(), vec![AdapterCall::Observe]);
}

#[test]
fn failed_initial_inspection_applies_once_and_verifies_once() {
    let directory = TestDirectory::new("inspect-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Err("inspection failed".to_owned()), Ok(true)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let snapshot = launch_at_login.reconcile().unwrap();

    assert!(known_match(&snapshot));
    assert!(snapshot.latest_failure().is_none());
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn failed_enable_still_verifies_and_keeps_truthful_observation() {
    let directory = TestDirectory::new("enable-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Ok(false)])
        .with_set_results(vec![Err("enable failed".to_owned())]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let failure = launch_at_login.reconcile().unwrap_err();
    let snapshot = launch_at_login.snapshot();

    assert_eq!(failure.stage(), LaunchAtLoginFailureStage::Enable);
    assert_eq!(
        failure.to_string(),
        "launch-at-login Enable failed: enable failed"
    );
    assert_eq!(snapshot.observed(), Registration::Disabled);
    assert!(!known_match(&snapshot));
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn apply_failure_remains_primary_when_verification_also_fails() {
    let directory = TestDirectory::new("apply-and-verify-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Err("verification failed".to_owned())])
        .with_set_results(vec![Err("enable failed".to_owned())]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let failure = launch_at_login.reconcile().unwrap_err();
    let snapshot = launch_at_login.snapshot();

    assert_eq!(failure.stage(), LaunchAtLoginFailureStage::Enable);
    assert_eq!(snapshot.observed(), Registration::Unknown);
    assert_eq!(
        snapshot.latest_failure().unwrap().stage(),
        LaunchAtLoginFailureStage::Enable
    );
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn verification_mismatch_is_reported_and_later_success_clears_it() {
    let directory = TestDirectory::new("verification-recovery");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Ok(false), Ok(true)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let failure = launch_at_login.reconcile().unwrap_err();
    assert_eq!(failure.stage(), LaunchAtLoginFailureStage::Verify);
    let failed = launch_at_login.snapshot();
    assert_eq!(failed.observed(), Registration::Disabled);
    assert!(!known_match(&failed));

    let recovered = launch_at_login.reconcile().unwrap();
    assert!(known_match(&recovered));
    assert!(recovered.latest_failure().is_none());
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
            AdapterCall::Observe,
        ]
    );
}

#[test]
fn failed_verification_records_unknown_registration() {
    let directory = TestDirectory::new("verification-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Err("verification failed".to_owned())]);
    let launch_at_login = LaunchAtLoginService::available(settings, adapter, unavailable_log());

    let failure = launch_at_login.reconcile().unwrap_err();
    let snapshot = launch_at_login.snapshot();

    assert_eq!(failure.stage(), LaunchAtLoginFailureStage::Verify);
    assert_eq!(snapshot.observed(), Registration::Unknown);
    assert!(!known_match(&snapshot));
}

#[test]
fn failed_preference_persistence_makes_no_native_call_and_keeps_intent() {
    let settings = SettingsService::unavailable(unavailable_log());
    let adapter = FakeAdapter::new(Vec::new());
    let launch_at_login =
        LaunchAtLoginService::available(settings.clone(), adapter.clone(), unavailable_log());

    let result = launch_at_login.toggle();

    assert!(matches!(
        result,
        Err(LaunchAtLoginCommandError::Settings(_))
    ));
    assert!(settings.snapshot().launch_at_login());
    assert!(launch_at_login.snapshot().desired());
    assert!(adapter.calls().is_empty());
}

#[test]
fn startup_does_not_apply_an_unpersistable_default_preference() {
    let settings = SettingsService::unavailable(unavailable_log());
    let adapter = FakeAdapter::new(Vec::new());
    let launch_at_login =
        LaunchAtLoginService::available(settings, adapter.clone(), unavailable_log());

    let failure = launch_at_login.reconcile().unwrap_err();
    let snapshot = launch_at_login.snapshot();

    assert_eq!(failure.stage(), LaunchAtLoginFailureStage::Verify);
    assert!(snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Unknown);
    assert!(snapshot.available());
    assert!(adapter.calls().is_empty());
}

#[test]
fn toggle_persists_before_applying_and_verifying_the_target() {
    let directory = TestDirectory::new("toggle-success");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings.clone(), adapter.clone(), unavailable_log());

    let snapshot = launch_at_login.toggle().unwrap();

    assert!(!settings.snapshot().launch_at_login());
    assert!(!snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Disabled);
    assert!(known_match(&snapshot));
    assert_eq!(
        adapter.calls(),
        vec![AdapterCall::SetEnabled(false), AdapterCall::Observe]
    );
}

#[test]
fn snapshot_reads_persisted_intent_while_native_toggle_is_in_flight() {
    struct BlockingAdapter {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl LaunchAtLoginAdapter for BlockingAdapter {
        fn observe(&self) -> Result<bool, String> {
            Ok(false)
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), String> {
            assert!(!enabled);
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(())
        }
    }

    let directory = TestDirectory::new("snapshot-during-toggle");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let launch_at_login = LaunchAtLoginService::available(
        settings.clone(),
        BlockingAdapter {
            entered: entered_sender,
            release: Mutex::new(release_receiver),
        },
        unavailable_log(),
    );
    let worker_service = launch_at_login.clone();
    let worker = thread::spawn(move || worker_service.toggle().unwrap());

    entered_receiver.recv().unwrap();

    assert!(!settings.snapshot().launch_at_login());
    assert!(!launch_at_login.snapshot().desired());

    release_sender.send(()).unwrap();
    assert!(known_match(&worker.join().unwrap()));
}

#[test]
fn native_toggle_failure_keeps_persisted_intent_and_truthful_observation() {
    let directory = TestDirectory::new("toggle-native-failure");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter =
        FakeAdapter::new(vec![Ok(true)]).with_set_results(vec![Err("disable failed".to_owned())]);
    let launch_at_login =
        LaunchAtLoginService::available(settings.clone(), adapter, unavailable_log());

    let result = launch_at_login.toggle();
    let snapshot = launch_at_login.snapshot();

    assert!(matches!(
        result,
        Err(LaunchAtLoginCommandError::Native(ref failure))
            if failure.stage() == LaunchAtLoginFailureStage::Disable
    ));
    assert!(!settings.snapshot().launch_at_login());
    assert!(!snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Enabled);
    assert!(!known_match(&snapshot));
}

#[test]
fn unavailable_integration_keeps_a_truthful_non_mutating_facade() {
    let directory = TestDirectory::new("unavailable");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let launch_at_login = LaunchAtLoginService::unavailable(
        settings.clone(),
        LaunchAtLoginFailure::initialize("plugin registration failed"),
        unavailable_log(),
    );

    let snapshot = launch_at_login.snapshot();

    assert!(snapshot.desired());
    assert_eq!(snapshot.observed(), Registration::Unknown);
    assert!(!snapshot.available());
    assert_eq!(
        snapshot.latest_failure().unwrap().stage(),
        LaunchAtLoginFailureStage::Initialize
    );
    assert_eq!(
        snapshot.latest_failure().unwrap().to_string(),
        "launch-at-login Initialize failed: plugin registration failed"
    );
    assert!(matches!(
        launch_at_login.toggle(),
        Err(LaunchAtLoginCommandError::Unavailable)
    ));
    assert!(settings.snapshot().launch_at_login());
}

#[test]
fn concurrent_toggles_serialize_complete_transactions_and_converge() {
    let directory = TestDirectory::new("concurrent-toggles");
    let settings = SettingsService::load(directory.0.clone(), unavailable_log());
    let adapter = FakeAdapter::new(vec![Ok(false), Ok(true), Ok(false), Ok(true)]);
    let launch_at_login =
        LaunchAtLoginService::available(settings.clone(), adapter.clone(), unavailable_log());

    let workers = (0..4)
        .map(|_| {
            let launch_at_login = launch_at_login.clone();
            thread::spawn(move || launch_at_login.toggle().unwrap())
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }

    assert!(settings.snapshot().launch_at_login());
    assert!(known_match(&launch_at_login.snapshot()));
    assert_eq!(
        adapter.calls(),
        vec![
            AdapterCall::SetEnabled(false),
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
            AdapterCall::SetEnabled(false),
            AdapterCall::Observe,
            AdapterCall::SetEnabled(true),
            AdapterCall::Observe,
        ]
    );
}
