use super::{
    InhibitorError, InhibitorState, NativeErrorCode, NativeFailure, NativeInhibitor,
    NativeOperation,
};
use std::ffi::c_void;

const REQUEST_REASON: &str = "Chiù idle sleep protection";
#[cfg(target_os = "windows")]
const SYSTEM_REQUIRED: i32 = windows_sys::Win32::System::Power::PowerRequestSystemRequired;
#[cfg(not(target_os = "windows"))]
const SYSTEM_REQUIRED: i32 = 1;

#[derive(Debug, Eq, PartialEq)]
struct PowerRequestHandle(*mut c_void);

// SAFETY: Power request objects are process-scoped kernel handles, not thread-affine
// resources. All mutation remains serialized through `&mut self`.
unsafe impl Send for PowerRequestHandle {}

trait PowerApi: Send {
    fn create_request(&mut self, reason: &'static str) -> Result<PowerRequestHandle, u32>;
    fn set_request(&mut self, handle: &PowerRequestHandle, request_type: i32) -> Result<(), u32>;
    fn clear_request(&mut self, handle: &PowerRequestHandle, request_type: i32) -> Result<(), u32>;
    fn close_request(
        &mut self,
        handle: PowerRequestHandle,
    ) -> Result<(), (u32, PowerRequestHandle)>;
}

#[cfg(target_os = "windows")]
struct SystemPowerApi;

#[cfg(target_os = "windows")]
impl SystemPowerApi {
    fn last_error() -> u32 {
        // SAFETY: GetLastError has no preconditions and is called immediately after failure.
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    }
}

#[cfg(target_os = "windows")]
impl PowerApi for SystemPowerApi {
    fn create_request(&mut self, reason: &'static str) -> Result<PowerRequestHandle, u32> {
        use windows_sys::{
            Win32::{
                Foundation::INVALID_HANDLE_VALUE,
                System::{
                    Power::PowerCreateRequest,
                    Threading::{
                        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
                    },
                },
            },
            core::PWSTR,
        };

        let mut reason = reason
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let context = REASON_CONTEXT {
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: reason.as_mut_ptr() as PWSTR,
            },
        };
        // SAFETY: The reason context and its null-terminated UTF-16 string live across the call.
        let handle = unsafe { PowerCreateRequest(&context) };
        if handle == INVALID_HANDLE_VALUE {
            Err(Self::last_error())
        } else {
            Ok(PowerRequestHandle(handle.cast()))
        }
    }

    fn set_request(&mut self, handle: &PowerRequestHandle, request_type: i32) -> Result<(), u32> {
        // SAFETY: The handle is owned by this adapter and the request type is a Win32 constant.
        let succeeded = unsafe {
            windows_sys::Win32::System::Power::PowerSetRequest(handle.0.cast(), request_type)
        };
        if succeeded == 0 {
            Err(Self::last_error())
        } else {
            Ok(())
        }
    }

    fn clear_request(&mut self, handle: &PowerRequestHandle, request_type: i32) -> Result<(), u32> {
        // SAFETY: The handle is owned by this adapter and currently has this request set.
        let succeeded = unsafe {
            windows_sys::Win32::System::Power::PowerClearRequest(handle.0.cast(), request_type)
        };
        if succeeded == 0 {
            Err(Self::last_error())
        } else {
            Ok(())
        }
    }

    fn close_request(
        &mut self,
        handle: PowerRequestHandle,
    ) -> Result<(), (u32, PowerRequestHandle)> {
        // SAFETY: Ownership of the handle is consumed by this call. A failed close returns
        // ownership to the caller so it can retry.
        let succeeded = unsafe { windows_sys::Win32::Foundation::CloseHandle(handle.0.cast()) };
        if succeeded == 0 {
            Err((Self::last_error(), handle))
        } else {
            Ok(())
        }
    }
}

enum WindowsOwnership {
    Inactive,
    Active(PowerRequestHandle),
    CleanupDebt(PowerRequestHandle),
}

pub(super) struct WindowsInhibitor<A> {
    api: A,
    ownership: WindowsOwnership,
}

impl<A> WindowsInhibitor<A> {
    fn new(api: A) -> Self {
        Self {
            api,
            ownership: WindowsOwnership::Inactive,
        }
    }
}

impl<A: PowerApi> NativeInhibitor for WindowsInhibitor<A> {
    fn state(&self) -> InhibitorState {
        match self.ownership {
            WindowsOwnership::Active(_) => InhibitorState::Active,
            WindowsOwnership::Inactive | WindowsOwnership::CleanupDebt(_) => {
                InhibitorState::Inactive
            }
        }
    }

    fn acquire(&mut self) -> Result<(), InhibitorError> {
        let ownership = std::mem::replace(&mut self.ownership, WindowsOwnership::Inactive);
        match ownership {
            WindowsOwnership::Active(handle) => {
                self.ownership = WindowsOwnership::Active(handle);
                return Ok(());
            }
            WindowsOwnership::CleanupDebt(handle) => {
                if let Err((code, handle)) = self.api.close_request(handle) {
                    self.ownership = WindowsOwnership::CleanupDebt(handle);
                    return Err(InhibitorError::new(NativeFailure::new(
                        NativeOperation::Cleanup,
                        NativeErrorCode::Windows(code),
                    )));
                }
            }
            WindowsOwnership::Inactive => {}
        }

        let handle = self.api.create_request(REQUEST_REASON).map_err(|code| {
            InhibitorError::new(NativeFailure::new(
                NativeOperation::Acquire,
                NativeErrorCode::Windows(code),
            ))
        })?;

        if let Err(acquire_code) = self.api.set_request(&handle, SYSTEM_REQUIRED) {
            let acquire_failure = NativeFailure::new(
                NativeOperation::Acquire,
                NativeErrorCode::Windows(acquire_code),
            );
            return match self.api.close_request(handle) {
                Ok(()) => Err(InhibitorError::new(acquire_failure)),
                Err((cleanup_code, handle)) => {
                    self.ownership = WindowsOwnership::CleanupDebt(handle);
                    Err(InhibitorError::with_cleanup(
                        acquire_failure,
                        NativeFailure::new(
                            NativeOperation::Cleanup,
                            NativeErrorCode::Windows(cleanup_code),
                        ),
                    ))
                }
            };
        }

        self.ownership = WindowsOwnership::Active(handle);
        Ok(())
    }

    fn release(&mut self) -> Result<(), InhibitorError> {
        let WindowsOwnership::Active(handle) =
            std::mem::replace(&mut self.ownership, WindowsOwnership::Inactive)
        else {
            unreachable!("release is called only for an active Windows inhibitor");
        };
        if let Err(code) = self.api.clear_request(&handle, SYSTEM_REQUIRED) {
            self.ownership = WindowsOwnership::Active(handle);
            return Err(InhibitorError::new(NativeFailure::new(
                NativeOperation::Release,
                NativeErrorCode::Windows(code),
            )));
        }

        if let Err((code, handle)) = self.api.close_request(handle) {
            self.ownership = WindowsOwnership::CleanupDebt(handle);
            return Err(InhibitorError::new(NativeFailure::new(
                NativeOperation::Cleanup,
                NativeErrorCode::Windows(code),
            )));
        }

        Ok(())
    }

    fn cleanup_on_drop(&mut self) {
        match std::mem::replace(&mut self.ownership, WindowsOwnership::Inactive) {
            WindowsOwnership::Active(handle) => {
                let _ = self.api.clear_request(&handle, SYSTEM_REQUIRED);
                let _ = self.api.close_request(handle);
            }
            WindowsOwnership::CleanupDebt(handle) => {
                let _ = self.api.close_request(handle);
            }
            WindowsOwnership::Inactive => {}
        }
    }
}

#[cfg(target_os = "windows")]
pub(super) fn system() -> impl NativeInhibitor {
    WindowsInhibitor::new(SystemPowerApi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Create(&'static str),
        Set(usize, i32),
        Clear(usize, i32),
        Close(usize),
    }

    struct RecordingPowerApi {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    struct ScriptedPowerApi {
        calls: Arc<Mutex<Vec<Call>>>,
        create_error: Option<u32>,
        set_results: VecDeque<Result<(), u32>>,
        clear_results: VecDeque<Result<(), u32>>,
        close_results: VecDeque<Result<(), u32>>,
        next_handle_id: usize,
    }

    impl ScriptedPowerApi {
        fn new(calls: Arc<Mutex<Vec<Call>>>) -> Self {
            Self {
                calls,
                create_error: None,
                set_results: VecDeque::new(),
                clear_results: VecDeque::new(),
                close_results: VecDeque::new(),
                next_handle_id: 1,
            }
        }
    }

    fn handle_id(handle: &PowerRequestHandle) -> usize {
        handle.0.addr()
    }

    impl PowerApi for ScriptedPowerApi {
        fn create_request(&mut self, reason: &'static str) -> Result<PowerRequestHandle, u32> {
            self.calls.lock().unwrap().push(Call::Create(reason));
            match self.create_error.take() {
                Some(code) => Err(code),
                None => {
                    let handle =
                        PowerRequestHandle(std::ptr::without_provenance_mut(self.next_handle_id));
                    self.next_handle_id += 1;
                    Ok(handle)
                }
            }
        }

        fn set_request(
            &mut self,
            handle: &PowerRequestHandle,
            request_type: i32,
        ) -> Result<(), u32> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Set(handle_id(handle), request_type));
            self.set_results.pop_front().unwrap_or(Ok(()))
        }

        fn clear_request(
            &mut self,
            handle: &PowerRequestHandle,
            request_type: i32,
        ) -> Result<(), u32> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Clear(handle_id(handle), request_type));
            self.clear_results.pop_front().unwrap_or(Ok(()))
        }

        fn close_request(
            &mut self,
            handle: PowerRequestHandle,
        ) -> Result<(), (u32, PowerRequestHandle)> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Close(handle_id(&handle)));
            match self.close_results.pop_front().unwrap_or(Ok(())) {
                Ok(()) => Ok(()),
                Err(code) => Err((code, handle)),
            }
        }
    }

    impl PowerApi for RecordingPowerApi {
        fn create_request(&mut self, reason: &'static str) -> Result<PowerRequestHandle, u32> {
            self.calls.lock().unwrap().push(Call::Create(reason));
            Ok(PowerRequestHandle(std::ptr::dangling_mut()))
        }

        fn set_request(
            &mut self,
            handle: &PowerRequestHandle,
            request_type: i32,
        ) -> Result<(), u32> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Set(handle_id(handle), request_type));
            Ok(())
        }

        fn clear_request(
            &mut self,
            handle: &PowerRequestHandle,
            request_type: i32,
        ) -> Result<(), u32> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Clear(handle_id(handle), request_type));
            Ok(())
        }

        fn close_request(
            &mut self,
            handle: PowerRequestHandle,
        ) -> Result<(), (u32, PowerRequestHandle)> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Close(handle_id(&handle)));
            Ok(())
        }
    }

    #[test]
    fn acquisition_sets_only_system_required_with_a_stable_reason() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut native = WindowsInhibitor::new(RecordingPowerApi {
            calls: calls.clone(),
        });

        native.acquire().expect("acquisition should succeed");

        assert_eq!(
            *calls.lock().unwrap(),
            [Call::Create(REQUEST_REASON), Call::Set(1, SYSTEM_REQUIRED)]
        );
    }

    #[test]
    fn request_creation_failure_stops_before_setting_a_request() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.create_error = Some(5);
        let mut native = WindowsInhibitor::new(api);

        let error = native.acquire().expect_err("creation should fail");

        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Acquire, NativeErrorCode::Windows(5))
        );
        assert_eq!(error.cleanup(), None);
        assert_eq!(*calls.lock().unwrap(), [Call::Create(REQUEST_REASON)]);
    }

    #[test]
    fn failed_set_closes_the_partial_request_and_preserves_the_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.set_results.push_back(Err(5));
        let mut native = WindowsInhibitor::new(api);

        let error = native.acquire().expect_err("set should fail");

        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Acquire, NativeErrorCode::Windows(5))
        );
        assert_eq!(error.cleanup(), None);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Close(1)
            ]
        );
    }

    #[test]
    fn set_and_close_failures_are_both_preserved_before_reacquisition() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.set_results.extend([Err(5), Ok(())]);
        api.close_results.extend([Err(6), Ok(())]);
        let mut inhibitor = IdleSleepInhibitor::with_native(WindowsInhibitor::new(api));

        let error = inhibitor.acquire().expect_err("set and close should fail");
        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Acquire, NativeErrorCode::Windows(5))
        );
        assert_eq!(
            error.cleanup(),
            Some(NativeFailure::new(
                NativeOperation::Cleanup,
                NativeErrorCode::Windows(6)
            ))
        );
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);

        inhibitor.acquire().expect("reacquisition should recover");
        assert_eq!(inhibitor.state(), InhibitorState::Active);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Close(1),
                Call::Close(1),
                Call::Create(REQUEST_REASON),
                Call::Set(2, SYSTEM_REQUIRED),
            ]
        );
    }

    #[test]
    fn reacquisition_does_not_create_a_request_while_cleanup_still_fails() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.set_results.push_back(Err(5));
        api.close_results.extend([Err(6), Err(7)]);
        let mut inhibitor = IdleSleepInhibitor::with_native(WindowsInhibitor::new(api));
        inhibitor.acquire().expect_err("set and close should fail");

        let error = inhibitor.acquire().expect_err("cleanup retry should fail");

        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Cleanup, NativeErrorCode::Windows(7))
        );
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Close(1),
                Call::Close(1),
            ]
        );
    }

    #[test]
    fn failed_clear_remains_active_and_retries_the_same_request() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.clear_results.extend([Err(8), Ok(())]);
        let mut inhibitor = IdleSleepInhibitor::with_native(WindowsInhibitor::new(api));
        inhibitor.acquire().expect("acquisition should succeed");

        let error = inhibitor.release().expect_err("clear should fail");
        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Release, NativeErrorCode::Windows(8))
        );
        assert_eq!(inhibitor.state(), InhibitorState::Active);

        inhibitor.release().expect("clear retry should succeed");
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Clear(1, SYSTEM_REQUIRED),
                Call::Clear(1, SYSTEM_REQUIRED),
                Call::Close(1),
            ]
        );
    }

    #[test]
    fn close_failure_after_clear_is_inactive_and_recovers_before_reacquisition() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut api = ScriptedPowerApi::new(calls.clone());
        api.close_results.extend([Err(9), Ok(())]);
        let mut inhibitor = IdleSleepInhibitor::with_native(WindowsInhibitor::new(api));
        inhibitor.acquire().expect("acquisition should succeed");

        let error = inhibitor.release().expect_err("close should fail");
        assert_eq!(
            error.primary(),
            NativeFailure::new(NativeOperation::Cleanup, NativeErrorCode::Windows(9))
        );
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);

        inhibitor
            .release()
            .expect("duplicate release should be a no-op");
        inhibitor.acquire().expect("reacquisition should recover");
        assert_eq!(inhibitor.state(), InhibitorState::Active);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Clear(1, SYSTEM_REQUIRED),
                Call::Close(1),
                Call::Close(1),
                Call::Create(REQUEST_REASON),
                Call::Set(2, SYSTEM_REQUIRED),
            ]
        );
    }

    #[test]
    fn drop_cleans_only_owned_windows_resources() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let inactive_calls = Arc::new(Mutex::new(Vec::new()));
        let inactive = IdleSleepInhibitor::with_native(WindowsInhibitor::new(
            ScriptedPowerApi::new(inactive_calls.clone()),
        ));
        drop(inactive);
        assert!(inactive_calls.lock().unwrap().is_empty());

        let active_calls = Arc::new(Mutex::new(Vec::new()));
        let mut active = IdleSleepInhibitor::with_native(WindowsInhibitor::new(
            ScriptedPowerApi::new(active_calls.clone()),
        ));
        active.acquire().expect("acquisition should succeed");
        drop(active);
        assert_eq!(
            *active_calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Clear(1, SYSTEM_REQUIRED),
                Call::Close(1),
            ]
        );

        let debt_calls = Arc::new(Mutex::new(Vec::new()));
        let mut debt_api = ScriptedPowerApi::new(debt_calls.clone());
        debt_api.set_results.push_back(Err(5));
        debt_api.close_results.push_back(Err(6));
        let mut cleanup_debt = IdleSleepInhibitor::with_native(WindowsInhibitor::new(debt_api));
        cleanup_debt
            .acquire()
            .expect_err("set and close should fail");
        drop(cleanup_debt);
        assert_eq!(
            *debt_calls.lock().unwrap(),
            [
                Call::Create(REQUEST_REASON),
                Call::Set(1, SYSTEM_REQUIRED),
                Call::Close(1),
                Call::Close(1),
            ]
        );
    }
}
