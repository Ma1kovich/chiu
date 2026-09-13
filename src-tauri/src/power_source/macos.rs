use super::{PowerSource, PowerSourceFailure, PowerSourceObserver, PowerSourceOperation};
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType},
    runloop::{
        CFRunLoop, CFRunLoopSource, CFRunLoopSourceInvalidate, CFRunLoopSourceRef,
        kCFRunLoopDefaultMode,
    },
    string::{CFString, CFStringRef},
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::atomic::{AtomicUsize, Ordering},
    thread::{self, ThreadId},
};

const AC_POWER: &str = "AC Power";
const BATTERY_POWER: &str = "Battery Power";
const UPS_POWER: &str = "UPS Power";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(info: CFTypeRef) -> CFStringRef;
    fn IOPSNotificationCreateRunLoopSource(
        callback: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> CFRunLoopSourceRef;
}

trait MacPowerApi: Clone + 'static {
    type RunLoop;
    type Source;

    fn create_source(
        &self,
        callback: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> Option<Self::Source>;
    fn current_run_loop(&self) -> Self::RunLoop;
    fn add_source(&self, run_loop: &Self::RunLoop, source: &Self::Source);
    fn remove_source(&self, run_loop: &Self::RunLoop, source: &Self::Source);
    fn invalidate_source(&self, source: &Self::Source);
    fn sample(&self) -> Result<PowerSource, PowerSourceFailure>;
}

#[derive(Clone, Copy)]
struct SystemMacPowerApi;

impl MacPowerApi for SystemMacPowerApi {
    type RunLoop = CFRunLoop;
    type Source = CFRunLoopSource;

    fn create_source(
        &self,
        callback: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> Option<Self::Source> {
        let source = unsafe { IOPSNotificationCreateRunLoopSource(callback, context) };
        if source.is_null() {
            None
        } else {
            // SAFETY: IOPSNotificationCreateRunLoopSource returns a create-owned source.
            Some(unsafe { CFRunLoopSource::wrap_under_create_rule(source) })
        }
    }

    fn current_run_loop(&self) -> Self::RunLoop {
        CFRunLoop::get_current()
    }

    fn add_source(&self, run_loop: &Self::RunLoop, source: &Self::Source) {
        run_loop.add_source(source, unsafe { kCFRunLoopDefaultMode });
    }

    fn remove_source(&self, run_loop: &Self::RunLoop, source: &Self::Source) {
        run_loop.remove_source(source, unsafe { kCFRunLoopDefaultMode });
    }

    fn invalidate_source(&self, source: &Self::Source) {
        unsafe { CFRunLoopSourceInvalidate(source.as_concrete_TypeRef()) };
    }

    fn sample(&self) -> Result<PowerSource, PowerSourceFailure> {
        sample_system_power_source()
    }
}

struct CallbackContext<A: MacPowerApi> {
    observer: PowerSourceObserver,
    api: A,
}

extern "C" fn power_source_changed<A: MacPowerApi>(context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() {
            return;
        }
        // SAFETY: the context remains owned until after source removal and invalidation.
        let context = unsafe { &*(context.cast::<CallbackContext<A>>()) };
        context.observer.publish(context.api.sample());
    }));
}

pub(super) fn start(
    observer: PowerSourceObserver,
) -> Result<MacPowerSourceRuntime, PowerSourceFailure> {
    let runtime = start_with_api(observer, SystemMacPowerApi)?;
    let token = next_runtime_token();
    MAC_RUNTIMES.with(|runtimes| {
        runtimes.borrow_mut().insert(token, runtime);
    });
    Ok(MacPowerSourceRuntime {
        token,
        owner_thread: thread::current().id(),
    })
}

fn start_with_api<A: MacPowerApi>(
    observer: PowerSourceObserver,
    api: A,
) -> Result<MacRuntime<A>, PowerSourceFailure> {
    let mut context = Box::new(CallbackContext {
        observer: observer.clone(),
        api: api.clone(),
    });
    let source = match api.create_source(
        power_source_changed::<A>,
        (&mut *context as *mut CallbackContext<A>).cast(),
    ) {
        Some(source) => source,
        None => {
            let failure = PowerSourceFailure::new(PowerSourceOperation::Registration, None);
            observer.publish(Err(failure.clone()));
            return Err(failure);
        }
    };

    let run_loop = api.current_run_loop();
    api.add_source(&run_loop, &source);
    observer.publish(api.sample());

    Ok(MacRuntime {
        api,
        run_loop: Some(run_loop),
        source: Some(source),
        context: Some(context),
    })
}

fn sample_system_power_source() -> Result<PowerSource, PowerSourceFailure> {
    let info_ref = unsafe { IOPSCopyPowerSourcesInfo() };
    if info_ref.is_null() {
        return Err(PowerSourceFailure::new(PowerSourceOperation::Sample, None));
    }
    // SAFETY: IOPSCopyPowerSourcesInfo follows the create rule.
    let info = unsafe { CFType::wrap_under_create_rule(info_ref) };
    let kind_ref = unsafe { IOPSGetProvidingPowerSourceType(info.as_CFTypeRef()) };
    if kind_ref.is_null() {
        return Err(PowerSourceFailure::new(PowerSourceOperation::Sample, None));
    }
    // SAFETY: the returned string is borrowed from the retained power-source info object.
    let kind = unsafe { CFString::wrap_under_get_rule(kind_ref) }.to_string();
    Ok(map_power_source_type(&kind))
}

fn map_power_source_type(kind: &str) -> PowerSource {
    match kind {
        AC_POWER => PowerSource::External,
        BATTERY_POWER | UPS_POWER => PowerSource::Limited,
        _ => PowerSource::Unknown,
    }
}

static NEXT_RUNTIME_TOKEN: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    static MAC_RUNTIMES: RefCell<HashMap<usize, MacRuntime<SystemMacPowerApi>>> =
        RefCell::new(HashMap::new());
}

fn next_runtime_token() -> usize {
    loop {
        let token = NEXT_RUNTIME_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

pub(super) struct MacPowerSourceRuntime {
    token: usize,
    owner_thread: ThreadId,
}

impl MacPowerSourceRuntime {
    pub(super) fn shutdown(self) -> Result<(), PowerSourceFailure> {
        if thread::current().id() != self.owner_thread {
            return Err(PowerSourceFailure::new(
                PowerSourceOperation::Unregistration,
                None,
            ));
        }
        MAC_RUNTIMES.with(|runtimes| {
            runtimes
                .borrow_mut()
                .remove(&self.token)
                .ok_or_else(|| PowerSourceFailure::new(PowerSourceOperation::Unregistration, None))?
                .shutdown()
        })
    }
}

struct MacRuntime<A: MacPowerApi> {
    api: A,
    run_loop: Option<A::RunLoop>,
    source: Option<A::Source>,
    context: Option<Box<CallbackContext<A>>>,
}

impl<A: MacPowerApi> MacRuntime<A> {
    fn shutdown(mut self) -> Result<(), PowerSourceFailure> {
        self.cleanup();
        Ok(())
    }

    fn cleanup(&mut self) {
        if let (Some(run_loop), Some(source)) = (&self.run_loop, &self.source) {
            self.api.invalidate_source(source);
            self.api.remove_source(run_loop, source);
        }
        self.source.take();
        self.context.take();
        self.run_loop.take();
    }
}

impl<A: MacPowerApi> Drop for MacRuntime<A> {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        automatic_protection::{AutomaticProtectionController, AutomaticProtectionSettings},
        local_log::{LocalLog, LoggingFailureStage},
        wake_coordinator::WakeCoordinator,
    };
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct FakeApi {
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        create_succeeds: bool,
        callback: Option<(extern "C" fn(*mut c_void), usize)>,
        samples: VecDeque<Result<PowerSource, PowerSourceFailure>>,
        calls: Vec<&'static str>,
    }

    impl FakeApi {
        fn new(
            create_succeeds: bool,
            samples: impl IntoIterator<Item = Result<PowerSource, PowerSourceFailure>>,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    create_succeeds,
                    callback: None,
                    samples: samples.into_iter().collect(),
                    calls: Vec::new(),
                })),
            }
        }

        fn notify(&self) {
            let (callback, context) = self.state.lock().unwrap().callback.unwrap();
            callback(context as *mut c_void);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl MacPowerApi for FakeApi {
        type RunLoop = ();
        type Source = ();

        fn create_source(
            &self,
            callback: extern "C" fn(*mut c_void),
            context: *mut c_void,
        ) -> Option<Self::Source> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("create");
            state.callback = Some((callback, context as usize));
            state.create_succeeds.then_some(())
        }

        fn current_run_loop(&self) -> Self::RunLoop {
            self.state.lock().unwrap().calls.push("current-run-loop");
        }

        fn add_source(&self, _run_loop: &Self::RunLoop, _source: &Self::Source) {
            self.state.lock().unwrap().calls.push("add");
        }

        fn remove_source(&self, _run_loop: &Self::RunLoop, _source: &Self::Source) {
            self.state.lock().unwrap().calls.push("remove");
        }

        fn invalidate_source(&self, _source: &Self::Source) {
            self.state.lock().unwrap().calls.push("invalidate");
        }

        fn sample(&self) -> Result<PowerSource, PowerSourceFailure> {
            let sample = {
                let mut state = self.state.lock().unwrap();
                state.calls.push("sample");
                state.samples.pop_front()
            };
            sample.unwrap()
        }
    }

    fn observer() -> PowerSourceObserver {
        let log = LocalLog::unavailable(LoggingFailureStage::Open);
        PowerSourceObserver::new(
            AutomaticProtectionController::new(
                AutomaticProtectionSettings::new(true, false),
                WakeCoordinator::new(log.clone()),
                log.clone(),
            ),
            log,
        )
    }

    #[test]
    fn maps_iokit_provider_strings_conservatively() {
        assert_eq!(map_power_source_type(AC_POWER), PowerSource::External);
        assert_eq!(map_power_source_type(BATTERY_POWER), PowerSource::Limited);
        assert_eq!(map_power_source_type(UPS_POWER), PowerSource::Limited);
        assert_eq!(map_power_source_type("Future Source"), PowerSource::Unknown);
    }

    #[test]
    fn registration_precedes_initial_sample_and_callback_resamples() {
        let observer = observer();
        let api = FakeApi::new(true, [Ok(PowerSource::External), Ok(PowerSource::Limited)]);
        let runtime = start_with_api(observer.clone(), api.clone()).unwrap();

        assert_eq!(observer.snapshot().source(), PowerSource::External);
        assert_eq!(api.calls(), ["create", "current-run-loop", "add", "sample"]);

        api.notify();
        assert_eq!(observer.snapshot().source(), PowerSource::Limited);
        assert_eq!(
            api.calls(),
            ["create", "current-run-loop", "add", "sample", "sample"]
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn creation_failure_is_degraded_and_does_not_sample() {
        let observer = observer();
        let api = FakeApi::new(false, []);

        assert!(start_with_api(observer.clone(), api.clone()).is_err());

        assert_eq!(
            observer.snapshot().health(),
            super::super::PowerSourceObserverHealth::Degraded
        );
        assert_eq!(api.calls(), ["create"]);
    }

    #[test]
    fn cleanup_invalidates_then_removes_exactly_once() {
        let api = FakeApi::new(true, [Ok(PowerSource::Unknown)]);
        let runtime = start_with_api(observer(), api.clone()).unwrap();

        runtime.shutdown().unwrap();

        assert_eq!(
            api.calls(),
            [
                "create",
                "current-run-loop",
                "add",
                "sample",
                "invalidate",
                "remove"
            ]
        );
    }

    #[test]
    fn callback_contains_panics_from_the_observation_target() {
        let observer = observer();
        let api = FakeApi::new(true, [Ok(PowerSource::External)]);
        let runtime = start_with_api(observer.clone(), api.clone()).unwrap();

        api.notify();

        assert_eq!(observer.snapshot().source(), PowerSource::External);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn sendable_cleanup_token_rejects_a_different_thread() {
        let runtime = MacPowerSourceRuntime {
            token: usize::MAX,
            owner_thread: thread::current().id(),
        };

        let result = thread::spawn(move || runtime.shutdown()).join().unwrap();

        assert!(result.is_err());
    }
}
