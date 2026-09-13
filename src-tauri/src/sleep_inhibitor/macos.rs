use super::{
    InhibitorError, InhibitorState, NativeErrorCode, NativeFailure, NativeInhibitor,
    NativeOperation,
};
use core_foundation::{
    base::TCFType,
    string::{CFString, CFStringRef},
};

const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";
const ASSERTION_LEVEL_ON: u32 = 255;
const ASSERTION_NAME: &str = "Chiù idle sleep protection";
const IO_RETURN_SUCCESS: i32 = 0;

type IOPMAssertionId = u32;
type IOPMAssertionLevel = u32;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: IOPMAssertionLevel,
        assertion_name: CFStringRef,
        assertion_id: *mut IOPMAssertionId,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionId) -> i32;
}

trait IokitApi: Send {
    fn create_assertion(
        &mut self,
        assertion_type: &'static str,
        assertion_level: u32,
        assertion_name: &'static str,
    ) -> Result<IOPMAssertionId, i32>;

    fn release_assertion(&mut self, assertion_id: IOPMAssertionId) -> Result<(), i32>;
}

struct SystemIokit;

impl IokitApi for SystemIokit {
    fn create_assertion(
        &mut self,
        assertion_type: &'static str,
        assertion_level: u32,
        assertion_name: &'static str,
    ) -> Result<IOPMAssertionId, i32> {
        let assertion_type = CFString::from_static_string(assertion_type);
        let assertion_name = CFString::from_static_string(assertion_name);
        let mut assertion_id = 0;
        // SAFETY: Both CFStrings live across the call, the output pointer is valid, and the
        // signature matches the macOS 14 IOKit declaration.
        let result = unsafe {
            IOPMAssertionCreateWithName(
                assertion_type.as_concrete_TypeRef(),
                assertion_level,
                assertion_name.as_concrete_TypeRef(),
                &mut assertion_id,
            )
        };
        if result == IO_RETURN_SUCCESS {
            Ok(assertion_id)
        } else {
            Err(result)
        }
    }

    fn release_assertion(&mut self, assertion_id: IOPMAssertionId) -> Result<(), i32> {
        // SAFETY: The assertion ID was returned by IOPMAssertionCreateWithName for this process.
        let result = unsafe { IOPMAssertionRelease(assertion_id) };
        if result == IO_RETURN_SUCCESS {
            Ok(())
        } else {
            Err(result)
        }
    }
}

pub(super) struct MacOsInhibitor<A> {
    api: A,
    assertion_id: Option<IOPMAssertionId>,
}

impl<A> MacOsInhibitor<A> {
    fn new(api: A) -> Self {
        Self {
            api,
            assertion_id: None,
        }
    }
}

impl<A: IokitApi> NativeInhibitor for MacOsInhibitor<A> {
    fn state(&self) -> InhibitorState {
        if self.assertion_id.is_some() {
            InhibitorState::Active
        } else {
            InhibitorState::Inactive
        }
    }

    fn acquire(&mut self) -> Result<(), InhibitorError> {
        match self
            .api
            .create_assertion(ASSERTION_TYPE, ASSERTION_LEVEL_ON, ASSERTION_NAME)
        {
            Ok(assertion_id) => {
                self.assertion_id = Some(assertion_id);
                Ok(())
            }
            Err(code) => Err(InhibitorError::new(NativeFailure::new(
                NativeOperation::Acquire,
                NativeErrorCode::MacOs(code),
            ))),
        }
    }

    fn release(&mut self) -> Result<(), InhibitorError> {
        let assertion_id = self
            .assertion_id
            .expect("active macOS inhibitor must own an assertion ID");
        match self.api.release_assertion(assertion_id) {
            Ok(()) => {
                self.assertion_id = None;
                Ok(())
            }
            Err(code) => Err(InhibitorError::new(NativeFailure::new(
                NativeOperation::Release,
                NativeErrorCode::MacOs(code),
            ))),
        }
    }

    fn cleanup_on_drop(&mut self) {
        if let Some(assertion_id) = self.assertion_id.take() {
            let _ = self.api.release_assertion(assertion_id);
        }
    }
}

pub(super) fn system() -> impl NativeInhibitor {
    MacOsInhibitor::new(SystemIokit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type CreateCall = (&'static str, u32, &'static str);

    struct RecordingIokit {
        create_calls: Arc<Mutex<Vec<CreateCall>>>,
        release_calls: Arc<Mutex<Vec<IOPMAssertionId>>>,
        assertion_id: IOPMAssertionId,
    }

    impl IokitApi for RecordingIokit {
        fn create_assertion(
            &mut self,
            assertion_type: &'static str,
            assertion_level: u32,
            assertion_name: &'static str,
        ) -> Result<IOPMAssertionId, i32> {
            self.create_calls.lock().unwrap().push((
                assertion_type,
                assertion_level,
                assertion_name,
            ));
            Ok(self.assertion_id)
        }

        fn release_assertion(&mut self, assertion_id: IOPMAssertionId) -> Result<(), i32> {
            self.release_calls.lock().unwrap().push(assertion_id);
            Ok(())
        }
    }

    #[test]
    fn acquisition_uses_only_the_idle_system_sleep_assertion() {
        let create_calls = Arc::new(Mutex::new(Vec::new()));
        let mut native = MacOsInhibitor::new(RecordingIokit {
            create_calls: create_calls.clone(),
            release_calls: Arc::new(Mutex::new(Vec::new())),
            assertion_id: 73,
        });

        native.acquire().expect("acquisition should succeed");

        assert_eq!(
            *create_calls.lock().unwrap(),
            [(ASSERTION_TYPE, ASSERTION_LEVEL_ON, ASSERTION_NAME)]
        );
    }

    #[test]
    fn release_uses_the_exact_owned_assertion_id() {
        let release_calls = Arc::new(Mutex::new(Vec::new()));
        let mut native = MacOsInhibitor::new(RecordingIokit {
            create_calls: Arc::new(Mutex::new(Vec::new())),
            release_calls: release_calls.clone(),
            assertion_id: 73,
        });
        native.acquire().expect("acquisition should succeed");

        native.release().expect("release should succeed");

        assert_eq!(*release_calls.lock().unwrap(), [73]);
    }

    struct ReleaseFailsOnceIokit {
        release_calls: Arc<Mutex<Vec<IOPMAssertionId>>>,
        failed: bool,
    }

    impl IokitApi for ReleaseFailsOnceIokit {
        fn create_assertion(
            &mut self,
            _assertion_type: &'static str,
            _assertion_level: u32,
            _assertion_name: &'static str,
        ) -> Result<IOPMAssertionId, i32> {
            Ok(73)
        }

        fn release_assertion(&mut self, assertion_id: IOPMAssertionId) -> Result<(), i32> {
            self.release_calls.lock().unwrap().push(assertion_id);
            if self.failed {
                Ok(())
            } else {
                self.failed = true;
                Err(-536_870_206)
            }
        }
    }

    #[test]
    fn failed_release_preserves_the_assertion_for_retry() {
        let release_calls = Arc::new(Mutex::new(Vec::new()));
        let mut native = MacOsInhibitor::new(ReleaseFailsOnceIokit {
            release_calls: release_calls.clone(),
            failed: false,
        });
        native.acquire().expect("acquisition should succeed");

        let error = native.release().expect_err("first release should fail");
        native.release().expect("release retry should succeed");

        assert_eq!(
            error.primary(),
            NativeFailure::new(
                NativeOperation::Release,
                NativeErrorCode::MacOs(-536_870_206)
            )
        );
        assert_eq!(*release_calls.lock().unwrap(), [73, 73]);
    }

    struct FailingCreateIokit;

    impl IokitApi for FailingCreateIokit {
        fn create_assertion(
            &mut self,
            _assertion_type: &'static str,
            _assertion_level: u32,
            _assertion_name: &'static str,
        ) -> Result<IOPMAssertionId, i32> {
            Err(-536_870_212)
        }

        fn release_assertion(&mut self, _assertion_id: IOPMAssertionId) -> Result<(), i32> {
            unreachable!("a failed create cannot own an assertion")
        }
    }

    #[test]
    fn failed_creation_preserves_the_iokit_error() {
        let mut native = MacOsInhibitor::new(FailingCreateIokit);

        let error = native.acquire().expect_err("creation should fail");

        assert_eq!(
            error.primary(),
            NativeFailure::new(
                NativeOperation::Acquire,
                NativeErrorCode::MacOs(-536_870_212)
            )
        );
        assert_eq!(error.cleanup(), None);
    }

    #[test]
    fn drop_releases_only_an_active_assertion() {
        use crate::sleep_inhibitor::IdleSleepInhibitor;

        let release_calls = Arc::new(Mutex::new(Vec::new()));
        let inactive = IdleSleepInhibitor::with_native(MacOsInhibitor::new(RecordingIokit {
            create_calls: Arc::new(Mutex::new(Vec::new())),
            release_calls: release_calls.clone(),
            assertion_id: 73,
        }));
        drop(inactive);
        assert!(release_calls.lock().unwrap().is_empty());

        let mut active = IdleSleepInhibitor::with_native(MacOsInhibitor::new(RecordingIokit {
            create_calls: Arc::new(Mutex::new(Vec::new())),
            release_calls: release_calls.clone(),
            assertion_id: 73,
        }));
        active.acquire().expect("acquisition should succeed");
        drop(active);

        assert_eq!(*release_calls.lock().unwrap(), [73]);
    }
}
