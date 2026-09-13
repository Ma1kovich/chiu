use crate::{
    local_log::{EventOutcome, LocalLog, SafeEvent},
    settings::{PersistenceCondition, SettingsService, SettingsUpdate, SettingsUpdateError},
};
use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

pub(crate) trait LaunchAtLoginAdapter: Send + Sync {
    fn observe(&self) -> Result<bool, String>;
    fn set_enabled(&self, enabled: bool) -> Result<(), String>;
}

struct TauriAutostartAdapter {
    app: AppHandle,
}

impl LaunchAtLoginAdapter for TauriAutostartAdapter {
    fn observe(&self) -> Result<bool, String> {
        self.app
            .autolaunch()
            .is_enabled()
            .map_err(|error| error.to_string())
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let manager = self.app.autolaunch();
        if enabled {
            manager.enable()
        } else {
            manager.disable()
        }
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Registration {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchAtLoginFailureStage {
    Initialize,
    Enable,
    Disable,
    Verify,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchAtLoginFailure {
    stage: LaunchAtLoginFailureStage,
    message: String,
}

impl LaunchAtLoginFailure {
    fn new(stage: LaunchAtLoginFailureStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }

    pub(crate) fn stage(&self) -> LaunchAtLoginFailureStage {
        self.stage
    }

    pub(crate) fn initialize(message: impl Into<String>) -> Self {
        Self::new(LaunchAtLoginFailureStage::Initialize, message)
    }
}

impl std::fmt::Display for LaunchAtLoginFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "launch-at-login {:?} failed: {}",
            self.stage, self.message
        )
    }
}

impl std::error::Error for LaunchAtLoginFailure {}

#[derive(Debug)]
pub(crate) enum LaunchAtLoginCommandError {
    Unavailable,
    Settings(SettingsUpdateError),
    Native(LaunchAtLoginFailure),
}

impl std::fmt::Display for LaunchAtLoginCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("launch at login is unavailable"),
            Self::Settings(error) => error.fmt(formatter),
            Self::Native(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LaunchAtLoginCommandError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchAtLoginSnapshot {
    desired: bool,
    observed: Registration,
    available: bool,
    latest_failure: Option<LaunchAtLoginFailure>,
}

impl LaunchAtLoginSnapshot {
    pub(crate) fn desired(&self) -> bool {
        self.desired
    }

    pub(crate) fn observed(&self) -> Registration {
        self.observed
    }

    pub(crate) fn available(&self) -> bool {
        self.available
    }

    pub(crate) fn latest_failure(&self) -> Option<&LaunchAtLoginFailure> {
        self.latest_failure.as_ref()
    }
}

#[derive(Clone)]
enum LaunchAtLoginBackend {
    Available(Arc<dyn LaunchAtLoginAdapter>),
    Unavailable(LaunchAtLoginFailure),
}

struct LaunchAtLoginState {
    observed: Registration,
    latest_failure: Option<LaunchAtLoginFailure>,
}

#[derive(Clone)]
pub(crate) struct LaunchAtLoginService {
    settings: SettingsService,
    backend: LaunchAtLoginBackend,
    state: Arc<Mutex<LaunchAtLoginState>>,
    operation: Arc<Mutex<()>>,
    log: LocalLog,
}

impl LaunchAtLoginService {
    pub(crate) fn register(
        settings: SettingsService,
        app: &AppHandle,
        log: LocalLog,
    ) -> Result<Self, LaunchAtLoginFailure> {
        app.plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .map_err(|error| LaunchAtLoginFailure::initialize(error.to_string()))?;
        Ok(Self::available(
            settings,
            TauriAutostartAdapter { app: app.clone() },
            log,
        ))
    }

    pub(crate) fn available(
        settings: SettingsService,
        adapter: impl LaunchAtLoginAdapter + 'static,
        log: LocalLog,
    ) -> Self {
        Self {
            settings,
            backend: LaunchAtLoginBackend::Available(Arc::new(adapter)),
            state: Arc::new(Mutex::new(LaunchAtLoginState {
                observed: Registration::Unknown,
                latest_failure: None,
            })),
            operation: Arc::new(Mutex::new(())),
            log,
        }
    }

    pub(crate) fn unavailable(
        settings: SettingsService,
        failure: LaunchAtLoginFailure,
        log: LocalLog,
    ) -> Self {
        Self {
            settings,
            backend: LaunchAtLoginBackend::Unavailable(failure.clone()),
            state: Arc::new(Mutex::new(LaunchAtLoginState {
                observed: Registration::Unknown,
                latest_failure: Some(failure),
            })),
            operation: Arc::new(Mutex::new(())),
            log,
        }
    }

    pub(crate) fn snapshot(&self) -> LaunchAtLoginSnapshot {
        let state = self
            .state
            .lock()
            .expect("launch-at-login state lock poisoned");
        LaunchAtLoginSnapshot {
            desired: self.settings.snapshot().launch_at_login(),
            observed: state.observed,
            available: matches!(self.backend, LaunchAtLoginBackend::Available(_)),
            latest_failure: state.latest_failure.clone(),
        }
    }

    pub(crate) fn reconcile(&self) -> Result<LaunchAtLoginSnapshot, LaunchAtLoginFailure> {
        let _operation = self
            .operation
            .lock()
            .expect("launch-at-login operation lock poisoned");
        let desired = self.settings.snapshot().launch_at_login();
        if let LaunchAtLoginBackend::Unavailable(failure) = &self.backend {
            return Err(failure.clone());
        }
        if self.settings.health().persistence() != PersistenceCondition::Writable {
            return Err(self.record_failure(
                Registration::Unknown,
                LaunchAtLoginFailureStage::Verify,
                "saved launch-at-login preference is unavailable",
            ));
        }
        self.reconcile_locked(desired)
    }

    pub(crate) fn toggle(&self) -> Result<LaunchAtLoginSnapshot, LaunchAtLoginCommandError> {
        let _operation = self
            .operation
            .lock()
            .expect("launch-at-login operation lock poisoned");
        if matches!(self.backend, LaunchAtLoginBackend::Unavailable(_)) {
            return Err(LaunchAtLoginCommandError::Unavailable);
        }
        let target = !self.settings.snapshot().launch_at_login();
        self.settings
            .update(SettingsUpdate::LaunchAtLogin(target))
            .map_err(LaunchAtLoginCommandError::Settings)?;
        self.apply_and_verify(target)
            .map_err(LaunchAtLoginCommandError::Native)
    }

    fn reconcile_locked(
        &self,
        desired: bool,
    ) -> Result<LaunchAtLoginSnapshot, LaunchAtLoginFailure> {
        let adapter = self.adapter();

        if let Ok(observed) = adapter.observe()
            && observed == desired
        {
            return Ok(self.record_success(observed));
        }

        self.apply_and_verify(desired)
    }

    fn apply_and_verify(
        &self,
        desired: bool,
    ) -> Result<LaunchAtLoginSnapshot, LaunchAtLoginFailure> {
        let adapter = self.adapter();

        let apply_failure = adapter.set_enabled(desired).err().map(|message| {
            LaunchAtLoginFailure::new(
                if desired {
                    LaunchAtLoginFailureStage::Enable
                } else {
                    LaunchAtLoginFailureStage::Disable
                },
                message,
            )
        });
        let verification = adapter.observe();
        let observed = verification
            .as_ref()
            .map(|enabled| registration(*enabled))
            .unwrap_or(Registration::Unknown);
        if let Some(failure) = apply_failure {
            return Err(self.record_failure(observed, failure.stage, failure.message));
        }
        let observed = verification.map_err(|message| {
            self.record_failure(
                Registration::Unknown,
                LaunchAtLoginFailureStage::Verify,
                message,
            )
        })?;
        if observed != desired {
            return Err(self.record_failure(
                registration(observed),
                LaunchAtLoginFailureStage::Verify,
                "observed registration did not match the saved preference",
            ));
        }
        Ok(self.record_success(observed))
    }

    fn record_success(&self, observed: bool) -> LaunchAtLoginSnapshot {
        let mut state = self
            .state
            .lock()
            .expect("launch-at-login state lock poisoned");
        state.observed = registration(observed);
        state.latest_failure = None;
        drop(state);
        self.record(SafeEvent::LaunchAtLogin {
            action: "reconcile",
            outcome: EventOutcome::Succeeded,
            stage: "verified",
        });
        self.snapshot()
    }

    fn record_failure(
        &self,
        observed: Registration,
        stage: LaunchAtLoginFailureStage,
        message: impl Into<String>,
    ) -> LaunchAtLoginFailure {
        let failure = LaunchAtLoginFailure::new(stage, message);
        let mut state = self
            .state
            .lock()
            .expect("launch-at-login state lock poisoned");
        state.observed = observed;
        state.latest_failure = Some(failure.clone());
        drop(state);
        self.record(SafeEvent::LaunchAtLogin {
            action: "reconcile",
            outcome: EventOutcome::Failed,
            stage: launch_failure_stage(stage),
        });
        failure
    }

    fn record(&self, event: SafeEvent) {
        self.log.record(event);
    }

    fn adapter(&self) -> &Arc<dyn LaunchAtLoginAdapter> {
        match &self.backend {
            LaunchAtLoginBackend::Available(adapter) => adapter,
            LaunchAtLoginBackend::Unavailable(_) => {
                unreachable!("unavailable launch-at-login service cannot perform native work")
            }
        }
    }
}

fn launch_failure_stage(stage: LaunchAtLoginFailureStage) -> &'static str {
    match stage {
        LaunchAtLoginFailureStage::Initialize => "initialize",
        LaunchAtLoginFailureStage::Enable => "enable",
        LaunchAtLoginFailureStage::Disable => "disable",
        LaunchAtLoginFailureStage::Verify => "verify",
    }
}

fn registration(enabled: bool) -> Registration {
    if enabled {
        Registration::Enabled
    } else {
        Registration::Disabled
    }
}

#[cfg(test)]
mod tests;
