#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", test))]
mod windows;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InhibitorState {
    Inactive,
    Active,
}

trait NativeInhibitor: Send {
    fn state(&self) -> InhibitorState;
    fn acquire(&mut self) -> Result<(), InhibitorError>;
    fn release(&mut self) -> Result<(), InhibitorError>;
    fn cleanup_on_drop(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeOperation {
    Acquire,
    Release,
    #[cfg_attr(
        not(target_os = "windows"),
        allow(dead_code, reason = "Windows cleanup failures preserve this operation")
    )]
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeErrorCode {
    #[cfg_attr(
        not(target_os = "windows"),
        allow(dead_code, reason = "Windows builds preserve native error codes")
    )]
    Windows(u32),
    MacOs(i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeFailure {
    operation: NativeOperation,
    code: NativeErrorCode,
}

impl NativeFailure {
    const fn new(operation: NativeOperation, code: NativeErrorCode) -> Self {
        Self { operation, code }
    }

    #[cfg(all(test, target_os = "windows"))]
    pub(crate) const fn operation(&self) -> NativeOperation {
        self.operation
    }

    #[cfg(all(test, target_os = "windows"))]
    pub(crate) const fn code(&self) -> NativeErrorCode {
        self.code
    }
}

#[derive(Debug)]
pub(crate) struct InhibitorError {
    primary: NativeFailure,
    cleanup: Option<NativeFailure>,
}

impl InhibitorError {
    const fn new(primary: NativeFailure) -> Self {
        Self {
            primary,
            cleanup: None,
        }
    }

    #[cfg_attr(
        not(target_os = "windows"),
        allow(
            dead_code,
            reason = "Windows preserves partial-acquisition cleanup failures"
        )
    )]
    const fn with_cleanup(primary: NativeFailure, cleanup: NativeFailure) -> Self {
        Self {
            primary,
            cleanup: Some(cleanup),
        }
    }

    #[cfg(test)]
    pub(crate) const fn primary(&self) -> NativeFailure {
        self.primary
    }

    #[cfg(test)]
    pub(crate) const fn cleanup(&self) -> Option<NativeFailure> {
        self.cleanup
    }
}

impl std::fmt::Display for InhibitorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "idle-sleep inhibitor {:?} failed with {:?}",
            self.primary.operation, self.primary.code
        )?;
        if let Some(cleanup) = self.cleanup {
            write!(
                formatter,
                "; cleanup {:?} failed with {:?}",
                cleanup.operation, cleanup.code
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for InhibitorError {}

pub(crate) struct IdleSleepInhibitor {
    native: Box<dyn NativeInhibitor>,
}

impl IdleSleepInhibitor {
    pub(crate) fn new() -> Self {
        #[cfg(target_os = "macos")]
        let native = macos::system();
        #[cfg(target_os = "windows")]
        let native = windows::system();

        Self {
            native: Box::new(native),
        }
    }

    #[cfg(test)]
    fn with_native(native: impl NativeInhibitor + 'static) -> Self {
        Self {
            native: Box::new(native),
        }
    }

    pub(crate) fn state(&self) -> InhibitorState {
        self.native.state()
    }

    pub(crate) fn acquire(&mut self) -> Result<(), InhibitorError> {
        if self.native.state() == InhibitorState::Active {
            return Ok(());
        }

        self.native.acquire()
    }

    pub(crate) fn release(&mut self) -> Result<(), InhibitorError> {
        if self.native.state() == InhibitorState::Inactive {
            return Ok(());
        }

        self.native.release()
    }
}

impl Drop for IdleSleepInhibitor {
    fn drop(&mut self) {
        self.native.cleanup_on_drop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct RecordingNative {
        acquire_calls: Arc<AtomicUsize>,
        acquire_error: Option<InhibitorError>,
        release_calls: Arc<AtomicUsize>,
        release_error: Option<InhibitorError>,
        state: InhibitorState,
    }

    impl Default for RecordingNative {
        fn default() -> Self {
            Self {
                acquire_calls: Arc::new(AtomicUsize::new(0)),
                acquire_error: None,
                release_calls: Arc::new(AtomicUsize::new(0)),
                release_error: None,
                state: InhibitorState::Inactive,
            }
        }
    }

    impl NativeInhibitor for RecordingNative {
        fn state(&self) -> InhibitorState {
            self.state
        }

        fn acquire(&mut self) -> Result<(), InhibitorError> {
            self.acquire_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.acquire_error.take() {
                return Err(error);
            }
            self.state = InhibitorState::Active;
            Ok(())
        }

        fn release(&mut self) -> Result<(), InhibitorError> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.state = InhibitorState::Inactive;
            Ok(())
        }

        fn cleanup_on_drop(&mut self) {}
    }

    #[test]
    fn a_new_inhibitor_is_inactive() {
        let inhibitor = IdleSleepInhibitor::with_native(RecordingNative::default());

        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
    }

    #[test]
    fn releasing_an_inactive_inhibitor_is_a_no_op() {
        let release_calls = Arc::new(AtomicUsize::new(0));
        let mut inhibitor = IdleSleepInhibitor::with_native(RecordingNative {
            acquire_calls: Arc::new(AtomicUsize::new(0)),
            acquire_error: None,
            release_calls: release_calls.clone(),
            release_error: None,
            state: InhibitorState::Inactive,
        });

        inhibitor.release().expect("release should succeed");

        assert_eq!(release_calls.load(Ordering::SeqCst), 0);
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
    }

    #[test]
    fn acquiring_twice_holds_one_native_inhibitor() {
        let acquire_calls = Arc::new(AtomicUsize::new(0));
        let mut inhibitor = IdleSleepInhibitor::with_native(RecordingNative {
            acquire_calls: acquire_calls.clone(),
            acquire_error: None,
            release_calls: Arc::new(AtomicUsize::new(0)),
            release_error: None,
            state: InhibitorState::Inactive,
        });

        inhibitor.acquire().expect("first acquire should succeed");
        inhibitor
            .acquire()
            .expect("duplicate acquire should succeed");

        assert_eq!(acquire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(inhibitor.state(), InhibitorState::Active);
    }

    #[test]
    fn a_released_inhibitor_can_be_acquired_again() {
        let acquire_calls = Arc::new(AtomicUsize::new(0));
        let release_calls = Arc::new(AtomicUsize::new(0));
        let mut inhibitor = IdleSleepInhibitor::with_native(RecordingNative {
            acquire_calls: acquire_calls.clone(),
            acquire_error: None,
            release_calls: release_calls.clone(),
            release_error: None,
            state: InhibitorState::Inactive,
        });

        inhibitor.acquire().expect("first acquire should succeed");
        inhibitor.release().expect("release should succeed");
        inhibitor.acquire().expect("second acquire should succeed");

        assert_eq!(acquire_calls.load(Ordering::SeqCst), 2);
        assert_eq!(release_calls.load(Ordering::SeqCst), 1);
        assert_eq!(inhibitor.state(), InhibitorState::Active);
    }

    #[test]
    fn failed_acquisition_is_observable_and_leaves_protection_inactive() {
        let native_failure = NativeFailure::new(
            NativeOperation::Acquire,
            NativeErrorCode::MacOs(-536_870_212),
        );
        let mut inhibitor = IdleSleepInhibitor::with_native(RecordingNative {
            acquire_calls: Arc::new(AtomicUsize::new(0)),
            acquire_error: Some(InhibitorError::new(native_failure)),
            release_calls: Arc::new(AtomicUsize::new(0)),
            release_error: None,
            state: InhibitorState::Inactive,
        });

        let error = inhibitor
            .acquire()
            .expect_err("native acquisition should fail");

        assert_eq!(error.primary(), native_failure);
        assert_eq!(error.cleanup(), None);
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
    }

    #[test]
    fn failed_release_remains_active_and_can_be_retried() {
        let release_calls = Arc::new(AtomicUsize::new(0));
        let native_failure = NativeFailure::new(
            NativeOperation::Release,
            NativeErrorCode::MacOs(-536_870_206),
        );
        let mut inhibitor = IdleSleepInhibitor::with_native(RecordingNative {
            acquire_calls: Arc::new(AtomicUsize::new(0)),
            acquire_error: None,
            release_calls: release_calls.clone(),
            release_error: Some(InhibitorError::new(native_failure)),
            state: InhibitorState::Inactive,
        });
        inhibitor.acquire().expect("acquire should succeed");

        let error = inhibitor.release().expect_err("first release should fail");
        assert_eq!(error.primary(), native_failure);
        assert_eq!(inhibitor.state(), InhibitorState::Active);

        inhibitor.release().expect("release retry should succeed");
        assert_eq!(release_calls.load(Ordering::SeqCst), 2);
        assert_eq!(inhibitor.state(), InhibitorState::Inactive);
    }

    #[test]
    fn inhibitor_is_sendable_for_lifecycle_ownership() {
        fn assert_send<T: Send>() {}

        assert_send::<IdleSleepInhibitor>();
    }
}
