use super::*;
use std::sync::OnceLock;

fn shutdown_callback_runtime(callback: Arc<dyn Fn() + Send + Sync>) -> UpdaterRuntime {
    let accepting = Arc::new(AtomicBool::new(true));
    let worker_exit = Arc::new(WorkerExit::running());
    let (sender, receiver) = mpsc::channel();
    let completion = WorkerCompletion(worker_exit.clone());
    let native_exit = worker_exit.clone();
    let worker = thread::Builder::new()
        .name("chiu-updater-shutdown-test".to_owned())
        .spawn(move || {
            let _completion = completion;
            while let Ok(command) = receiver.recv() {
                if matches!(command, WorkerCommand::Stop) {
                    native_exit.mark_native_exit_pending();
                    callback();
                    break;
                }
            }
        })
        .expect("shutdown callback test worker starts");
    UpdaterRuntime {
        accepting,
        sender: Some(sender),
        worker: Some(worker),
        worker_exit,
    }
}

fn activation_callback_registration(
    settings: &SettingsService,
    callback: Arc<dyn Fn() + Send + Sync>,
) -> (UpdaterService, UpdaterRuntime) {
    let snapshot = Arc::new(Mutex::new(UpdaterSnapshot {
        availability: UpdateAvailability::Available,
        automatic_checks_enabled: settings.snapshot().automatic_update_checks_enabled(),
        status: UpdateStatus::Idle,
    }));
    let accepting = Arc::new(AtomicBool::new(true));
    let operation_claimed = Arc::new(AtomicBool::new(false));
    let worker_exit = Arc::new(WorkerExit::running());
    let (sender, receiver) = mpsc::channel();
    let completion = WorkerCompletion(worker_exit.clone());
    let native_exit = worker_exit.clone();
    let worker = thread::Builder::new()
        .name("chiu-updater-callback-test".to_owned())
        .spawn(move || {
            let _completion = completion;
            while let Ok(command) = receiver.recv() {
                match command {
                    WorkerCommand::Activate => {
                        native_exit.mark_native_exit_pending();
                        callback();
                    }
                    WorkerCommand::Stop => break,
                    _ => {}
                }
            }
        })
        .expect("activation callback test worker starts");
    (
        UpdaterService {
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
    )
}

#[test]
fn updater_worker_panic_completes_exit_wait_and_is_reported() {
    let worker_exit = Arc::new(WorkerExit::running());
    let completion = WorkerCompletion(worker_exit.clone());
    let worker = thread::Builder::new()
        .name("chiu-updater-panic-test".to_owned())
        .spawn(move || {
            let _completion = completion;
            panic!("scripted updater worker panic");
        })
        .expect("panic test worker starts");
    let mut runtime = UpdaterRuntime {
        accepting: Arc::new(AtomicBool::new(false)),
        sender: None,
        worker: Some(worker),
        worker_exit,
    };

    let error = runtime.shutdown().unwrap_err();

    assert_eq!(error.to_string(), "updater worker panicked");
}

#[test]
fn updater_callback_racing_normal_finish_cannot_block_lifecycle_join() {
    let bridge: crate::ShutdownBridge = Arc::new(OnceLock::new());
    let callback = crate::updater_shutdown_callback(Arc::downgrade(&bridge));
    assert_eq!(Arc::strong_count(&bridge), 1);

    let (log, lines) = LocalLog::recording();
    let updater_callback = callback.clone();
    let runtime = crate::product_composition::ProductRuntime::build(
        None,
        log.clone(),
        crate::test_launch_at_login,
        move |settings, _| UpdaterRegistration {
            service: UpdaterService::unconfigured(&settings),
            runtime: shutdown_callback_runtime(updater_callback),
            failure: None,
        },
    );
    let owner: crate::ProductOwner = Arc::new(Mutex::new(Some(runtime)));
    let shutdown = crate::ShutdownCoordinator::new(owner.clone(), Arc::new(Mutex::new(None)), log);
    assert!(bridge.set(shutdown.clone()).is_ok());
    assert_eq!(Arc::strong_count(&bridge), 1);
    owner.lock().unwrap().as_mut().unwrap().start().unwrap();

    let normal_shutdown = shutdown.clone();
    let (finished, completion) = mpsc::channel();
    thread::spawn(move || {
        normal_shutdown.finish();
        finished.send(()).unwrap();
    });

    completion
        .recv_timeout(Duration::from_secs(1))
        .expect("normal shutdown must not deadlock joining the updater callback");
    assert!(owner.lock().unwrap().is_none());
    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted.matches("application.shutdown-requested").count(),
        1
    );
    assert_eq!(persisted.matches("process.stopped").count(), 1);

    drop(shutdown);
    drop(bridge);
    callback();
}

#[test]
fn normal_finish_waits_for_updater_owned_cleanup_to_complete() {
    let bridge: crate::ShutdownBridge = Arc::new(OnceLock::new());
    let callback = crate::updater_shutdown_callback(Arc::downgrade(&bridge));
    let (cleanup_entered, entered) = mpsc::channel();
    let (release_cleanup, release) = mpsc::channel();
    let updater_callback = callback.clone();
    let (log, lines) = LocalLog::recording();
    let runtime = crate::product_composition::ProductRuntime::build(
        None,
        log.clone(),
        crate::test_launch_at_login,
        move |settings, _| {
            let (service, runtime) = activation_callback_registration(&settings, updater_callback);
            UpdaterRegistration {
                service,
                runtime,
                failure: None,
            }
        },
    );
    let owner: crate::ProductOwner = Arc::new(Mutex::new(Some(runtime)));
    let presentation = Arc::new(Mutex::new(Some(
        crate::tray_host::PresentationRuntime::for_shutdown_test(move || {
            cleanup_entered.send(()).unwrap();
            release.recv().unwrap();
        }),
    )));
    let shutdown = crate::ShutdownCoordinator::new(owner.clone(), presentation, log);
    assert!(bridge.set(shutdown.clone()).is_ok());
    owner.lock().unwrap().as_mut().unwrap().start().unwrap();
    entered
        .recv_timeout(Duration::from_secs(1))
        .expect("updater callback must claim shutdown and reach later cleanup");

    let normal_shutdown = shutdown.clone();
    let (finished, completion) = mpsc::channel();
    thread::spawn(move || {
        normal_shutdown.finish();
        finished.send(()).unwrap();
    });
    let early = completion.recv_timeout(Duration::from_millis(50));
    release_cleanup.send(()).unwrap();
    assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
    completion
        .recv_timeout(Duration::from_secs(1))
        .expect("normal shutdown must return after updater-owned cleanup completes");

    assert!(owner.lock().unwrap().is_none());
    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted.matches("application.shutdown-requested").count(),
        1
    );
    assert_eq!(persisted.matches("process.stopped").count(), 1);
}

#[test]
fn windows_updater_hook_waits_for_normal_owned_cleanup_to_complete() {
    let bridge: crate::ShutdownBridge = Arc::new(OnceLock::new());
    let callback = crate::updater_shutdown_callback(Arc::downgrade(&bridge));
    let (hook_entered, entered) = mpsc::channel();
    let (hook_returned, returned) = mpsc::channel();
    let updater_callback = callback.clone();
    let registered_updater = Arc::new(Mutex::new(None));
    let capture_updater = registered_updater.clone();
    let (log, lines) = LocalLog::recording();
    let runtime = crate::product_composition::ProductRuntime::build(
        None,
        log.clone(),
        crate::test_launch_at_login,
        move |settings, _| {
            let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                hook_entered.send(()).unwrap();
                updater_callback();
                hook_returned.send(()).unwrap();
            });
            let (service, runtime) = activation_callback_registration(&settings, callback);
            *capture_updater.lock().unwrap() = Some(service.clone());
            UpdaterRegistration {
                service,
                runtime,
                failure: None,
            }
        },
    );
    let owner: crate::ProductOwner = Arc::new(Mutex::new(Some(runtime)));
    let shutdown = crate::ShutdownCoordinator::new(owner.clone(), Arc::new(Mutex::new(None)), log);
    assert!(bridge.set(shutdown.clone()).is_ok());
    owner.lock().unwrap().as_mut().unwrap().start().unwrap();
    let updater = registered_updater.lock().unwrap().take().unwrap();

    let product = owner.lock().unwrap();
    let normal_shutdown = shutdown.clone();
    let (normal_finished, normal_completion) = mpsc::channel();
    thread::spawn(move || {
        normal_shutdown.finish();
        normal_finished.send(()).unwrap();
    });
    while !shutdown.shutdown_logged.load(Ordering::Acquire) {
        thread::yield_now();
    }
    assert!(matches!(
        *shutdown.finishing.0.lock().unwrap(),
        crate::FinishState::Running
    ));

    updater.activate();
    entered
        .recv_timeout(Duration::from_secs(1))
        .expect("Windows updater hook must start while normal cleanup is pending");
    let early_hook_return = returned.recv_timeout(Duration::from_millis(50));
    assert!(matches!(
        early_hook_return,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    drop(product);
    normal_completion
        .recv_timeout(Duration::from_secs(1))
        .expect("normal cleanup must complete without joining the waiting updater hook");
    returned
        .recv_timeout(Duration::from_secs(1))
        .expect("Windows updater hook returns only after normal cleanup completes");
    assert!(owner.lock().unwrap().is_none());
    let persisted = lines.lock().unwrap().join("");
    assert_eq!(
        persisted.matches("application.shutdown-requested").count(),
        1
    );
    assert_eq!(persisted.matches("process.stopped").count(), 1);
}
