use super::{PowerSource, PowerSourceFailure, PowerSourceObserver, PowerSourceOperation};
use std::{
    collections::HashMap,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, HANDLE, WIN32_ERROR},
    System::{
        Power::{
            DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, POWERBROADCAST_SETTING,
            PowerSettingRegisterNotification, PowerSettingUnregisterNotification,
        },
        SystemServices::GUID_ACDC_POWER_SOURCE,
    },
    UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE, REGISTER_NOTIFICATION_FLAGS,
    },
};
use windows_sys::core::GUID;

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
static OBSERVERS: OnceLock<Mutex<HashMap<usize, PowerSourceObserver>>> = OnceLock::new();

fn observers() -> &'static Mutex<HashMap<usize, PowerSourceObserver>> {
    OBSERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_token() -> usize {
    loop {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

trait PowerApi: Send + Sync {
    unsafe fn register(
        &self,
        setting_guid: *const GUID,
        flags: REGISTER_NOTIFICATION_FLAGS,
        recipient: HANDLE,
        registration: *mut *mut c_void,
    ) -> WIN32_ERROR;
    unsafe fn unregister(&self, registration: HPOWERNOTIFY) -> WIN32_ERROR;
}

struct SystemPowerApi;

impl PowerApi for SystemPowerApi {
    unsafe fn register(
        &self,
        setting_guid: *const GUID,
        flags: REGISTER_NOTIFICATION_FLAGS,
        recipient: HANDLE,
        registration: *mut *mut c_void,
    ) -> WIN32_ERROR {
        unsafe { PowerSettingRegisterNotification(setting_guid, flags, recipient, registration) }
    }

    unsafe fn unregister(&self, registration: HPOWERNOTIFY) -> WIN32_ERROR {
        unsafe { PowerSettingUnregisterNotification(registration) }
    }
}

pub(super) fn start(
    observer: PowerSourceObserver,
) -> Result<WindowsPowerSourceRuntime, PowerSourceFailure> {
    start_with_api(observer, Arc::new(SystemPowerApi))
}

fn start_with_api(
    observer: PowerSourceObserver,
    api: Arc<dyn PowerApi>,
) -> Result<WindowsPowerSourceRuntime, PowerSourceFailure> {
    let token = next_token();
    observers()
        .lock()
        .expect("power source callback registry lock poisoned")
        .insert(token, observer.clone());

    let mut parameters = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(power_setting_callback),
        Context: token as *mut c_void,
    };
    let mut registration = ptr::null_mut();
    let result = unsafe {
        api.register(
            &GUID_ACDC_POWER_SOURCE,
            DEVICE_NOTIFY_CALLBACK,
            (&mut parameters as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS).cast(),
            &mut registration,
        )
    };
    if result != ERROR_SUCCESS {
        observers()
            .lock()
            .expect("power source callback registry lock poisoned")
            .remove(&token);
        let failure = PowerSourceFailure::new(PowerSourceOperation::Registration, Some(result));
        observer.publish(Err(failure.clone()));
        return Err(failure);
    }

    Ok(WindowsPowerSourceRuntime {
        registration: registration as HPOWERNOTIFY,
        token,
        api,
    })
}

unsafe extern "system" fn power_setting_callback(
    context: *const c_void,
    event_type: u32,
    setting: *const c_void,
) -> u32 {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let token = context as usize;
        if token == 0 {
            return;
        }
        let observer = observers()
            .lock()
            .expect("power source callback registry lock poisoned")
            .get(&token)
            .cloned();
        if let Some(observer) = observer {
            observer.publish(parse_notification(event_type, setting));
        }
    }));
    ERROR_SUCCESS
}

fn parse_notification(
    event_type: u32,
    setting: *const c_void,
) -> Result<PowerSource, PowerSourceFailure> {
    if event_type != PBT_POWERSETTINGCHANGE || setting.is_null() {
        return Err(PowerSourceFailure::new(PowerSourceOperation::Sample, None));
    }
    let setting = setting.cast::<POWERBROADCAST_SETTING>();
    let header = unsafe { ptr::read_unaligned(setting) };
    if !guid_eq(header.PowerSetting, GUID_ACDC_POWER_SOURCE)
        || header.DataLength != size_of::<u32>() as u32
    {
        return Err(PowerSourceFailure::new(PowerSourceOperation::Sample, None));
    }
    let value = unsafe { ptr::read_unaligned(ptr::addr_of!((*setting).Data).cast::<u32>()) };
    Ok(map_power_condition(value))
}

fn guid_eq(left: GUID, right: GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

fn map_power_condition(condition: u32) -> PowerSource {
    match condition {
        0 => PowerSource::External,
        1 | 2 => PowerSource::Limited,
        _ => PowerSource::Unknown,
    }
}

pub(super) struct WindowsPowerSourceRuntime {
    registration: HPOWERNOTIFY,
    token: usize,
    api: Arc<dyn PowerApi>,
}

unsafe impl Send for WindowsPowerSourceRuntime {}

impl WindowsPowerSourceRuntime {
    pub(super) fn shutdown(self) -> Result<(), PowerSourceFailure> {
        let result = unsafe { self.api.unregister(self.registration) };
        observers()
            .lock()
            .expect("power source callback registry lock poisoned")
            .remove(&self.token);
        if result == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(PowerSourceFailure::new(
                PowerSourceOperation::Unregistration,
                Some(result),
            ))
        }
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

    #[derive(Clone, Copy)]
    struct CapturedRegistration {
        guid: GUID,
        flags: REGISTER_NOTIFICATION_FLAGS,
        callback: windows_sys::Win32::System::Power::PDEVICE_NOTIFY_CALLBACK_ROUTINE,
        context: usize,
    }

    #[derive(Default)]
    struct FakeState {
        registration: Option<CapturedRegistration>,
        unregister_calls: usize,
    }

    struct FakeApi {
        register_result: WIN32_ERROR,
        unregister_result: WIN32_ERROR,
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeApi {
        fn new(register_result: WIN32_ERROR, unregister_result: WIN32_ERROR) -> Self {
            Self {
                register_result,
                unregister_result,
                state: Arc::new(Mutex::new(FakeState::default())),
            }
        }

        fn registration(&self) -> CapturedRegistration {
            self.state.lock().unwrap().registration.unwrap()
        }

        fn unregister_calls(&self) -> usize {
            self.state.lock().unwrap().unregister_calls
        }
    }

    impl PowerApi for FakeApi {
        unsafe fn register(
            &self,
            setting_guid: *const GUID,
            flags: REGISTER_NOTIFICATION_FLAGS,
            recipient: HANDLE,
            registration: *mut *mut c_void,
        ) -> WIN32_ERROR {
            let parameters = unsafe {
                ptr::read_unaligned(recipient.cast::<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>())
            };
            self.state.lock().unwrap().registration = Some(CapturedRegistration {
                guid: unsafe { ptr::read_unaligned(setting_guid) },
                flags,
                callback: parameters.Callback,
                context: parameters.Context as usize,
            });
            if self.register_result == ERROR_SUCCESS {
                unsafe { registration.write(1usize as *mut c_void) };
            }
            self.register_result
        }

        unsafe fn unregister(&self, _registration: HPOWERNOTIFY) -> WIN32_ERROR {
            self.state.lock().unwrap().unregister_calls += 1;
            self.unregister_result
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NotificationPayload {
        guid: GUID,
        length: u32,
        value: u32,
    }

    fn payload(value: u32) -> NotificationPayload {
        NotificationPayload {
            guid: GUID_ACDC_POWER_SOURCE,
            length: size_of::<u32>() as u32,
            value,
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
    fn maps_all_documented_power_conditions_and_unknown_values() {
        assert_eq!(map_power_condition(0), PowerSource::External);
        assert_eq!(map_power_condition(1), PowerSource::Limited);
        assert_eq!(map_power_condition(2), PowerSource::Limited);
        assert_eq!(map_power_condition(3), PowerSource::Unknown);
    }

    #[test]
    fn registration_uses_the_acdc_callback_contract() {
        let api = Arc::new(FakeApi::new(ERROR_SUCCESS, ERROR_SUCCESS));
        let runtime = start_with_api(observer(), api.clone()).unwrap();

        let registration = api.registration();
        assert!(guid_eq(registration.guid, GUID_ACDC_POWER_SOURCE));
        assert_eq!(registration.flags, DEVICE_NOTIFY_CALLBACK);
        assert!(registration.callback.is_some());
        assert_ne!(registration.context, 0);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn parses_documented_and_unaligned_payloads() {
        for (value, expected) in [
            (0, PowerSource::External),
            (1, PowerSource::Limited),
            (2, PowerSource::Limited),
            (3, PowerSource::Unknown),
        ] {
            let payload = payload(value);
            assert_eq!(
                parse_notification(
                    PBT_POWERSETTINGCHANGE,
                    (&payload as *const NotificationPayload).cast(),
                )
                .unwrap(),
                expected
            );
        }

        let mut unaligned = [0_u8; size_of::<NotificationPayload>() + 1];
        let pointer = unsafe { unaligned.as_mut_ptr().add(1).cast::<NotificationPayload>() };
        unsafe { ptr::write_unaligned(pointer, payload(0)) };
        assert_eq!(
            parse_notification(PBT_POWERSETTINGCHANGE, pointer.cast()).unwrap(),
            PowerSource::External
        );
    }

    #[test]
    fn malformed_notifications_are_rejected_conservatively() {
        assert!(parse_notification(PBT_POWERSETTINGCHANGE, ptr::null()).is_err());
        let valid = payload(0);
        assert!(parse_notification(0, (&valid as *const NotificationPayload).cast()).is_err());

        let wrong_guid = NotificationPayload {
            guid: GUID::from_u128(1),
            ..payload(0)
        };
        assert!(
            parse_notification(
                PBT_POWERSETTINGCHANGE,
                (&wrong_guid as *const NotificationPayload).cast(),
            )
            .is_err()
        );
        let wrong_length = NotificationPayload {
            length: 1,
            ..payload(0)
        };
        assert!(
            parse_notification(
                PBT_POWERSETTINGCHANGE,
                (&wrong_length as *const NotificationPayload).cast(),
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_callbacks_are_safe_and_late_callbacks_are_no_ops() {
        let observer = observer();
        let api = Arc::new(FakeApi::new(ERROR_SUCCESS, ERROR_SUCCESS));
        let runtime = start_with_api(observer.clone(), api.clone()).unwrap();
        let registration = api.registration();
        let external = payload(0);

        unsafe {
            registration.callback.unwrap()(
                registration.context as *const c_void,
                PBT_POWERSETTINGCHANGE,
                (&external as *const NotificationPayload).cast(),
            );
            registration.callback.unwrap()(
                registration.context as *const c_void,
                PBT_POWERSETTINGCHANGE,
                (&external as *const NotificationPayload).cast(),
            );
        }
        assert_eq!(observer.snapshot().source(), PowerSource::External);

        runtime.shutdown().unwrap();
        let limited = payload(1);
        unsafe {
            registration.callback.unwrap()(
                registration.context as *const c_void,
                PBT_POWERSETTINGCHANGE,
                (&limited as *const NotificationPayload).cast(),
            );
        }
        assert_eq!(observer.snapshot().source(), PowerSource::External);
    }

    #[test]
    fn registration_failure_removes_the_callback_token() {
        let observer = observer();
        let before = observers().lock().unwrap().len();

        let result = start_with_api(observer, Arc::new(FakeApi::new(5, ERROR_SUCCESS)));

        assert!(result.is_err());
        assert_eq!(observers().lock().unwrap().len(), before);
    }

    #[test]
    fn unregistration_failure_still_removes_the_callback_token() {
        let observer = observer();
        let api = Arc::new(FakeApi::new(ERROR_SUCCESS, 6));
        let runtime = start_with_api(observer, api.clone()).unwrap();
        let token = runtime.token;

        assert!(runtime.shutdown().is_err());
        assert!(!observers().lock().unwrap().contains_key(&token));
        assert_eq!(api.unregister_calls(), 1);
    }
}
