use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum WorkerExitState {
    Running,
    #[cfg(any(test, target_os = "windows"))]
    NativeExitPending,
    Finished,
}

pub(super) struct WorkerExit {
    state: Mutex<WorkerExitState>,
    changed: Condvar,
}

impl WorkerExit {
    pub(super) fn running() -> Self {
        Self {
            state: Mutex::new(WorkerExitState::Running),
            changed: Condvar::new(),
        }
    }

    pub(super) fn finished() -> Self {
        Self {
            state: Mutex::new(WorkerExitState::Finished),
            changed: Condvar::new(),
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    pub(super) fn mark_native_exit_pending(&self) {
        let mut state = self
            .state
            .lock()
            .expect("updater worker exit lock poisoned");
        if *state == WorkerExitState::Running {
            *state = WorkerExitState::NativeExitPending;
            self.changed.notify_all();
        }
    }

    pub(super) fn mark_finished(&self) {
        *self
            .state
            .lock()
            .expect("updater worker exit lock poisoned") = WorkerExitState::Finished;
        self.changed.notify_all();
    }

    pub(super) fn wait_until_shutdown_can_continue(&self) -> WorkerExitState {
        let mut state = self
            .state
            .lock()
            .expect("updater worker exit lock poisoned");
        while *state == WorkerExitState::Running {
            state = self
                .changed
                .wait(state)
                .expect("updater worker exit lock poisoned");
        }
        *state
    }

    #[cfg(test)]
    pub(super) fn is_finished(&self) -> bool {
        *self
            .state
            .lock()
            .expect("updater worker exit lock poisoned")
            == WorkerExitState::Finished
    }
}

pub(super) struct WorkerCompletion(pub(super) Arc<WorkerExit>);

impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        self.0.mark_finished();
    }
}
