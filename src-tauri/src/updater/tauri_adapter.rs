use super::*;
use ::tauri::AppHandle;
use serde_json::Value;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Error as TauriUpdaterError, UpdaterExt};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) enum ConfiguredUpdater {
    Unconfigured,
    Configured,
}

pub(super) fn configured_updater(
    value: Option<&Value>,
) -> Result<ConfiguredUpdater, UpdaterInitializationFailure> {
    let Some(value) = value else {
        return Ok(ConfiguredUpdater::Unconfigured);
    };
    let config: tauri_plugin_updater::Config =
        serde_json::from_value(value.clone()).map_err(|_| UpdaterInitializationFailure)?;
    if config.endpoints.is_empty()
        || config.pubkey.trim().is_empty()
        || config.dangerous_insecure_transport_protocol
        || has_dangerous_tls_override(value)
        || config
            .endpoints
            .iter()
            .any(|endpoint| endpoint.scheme() != "https")
    {
        return Err(UpdaterInitializationFailure);
    }
    Ok(ConfiguredUpdater::Configured)
}

fn has_dangerous_tls_override(value: &Value) -> bool {
    [
        "dangerousAcceptInvalidCerts",
        "dangerous-accept-invalid-certs",
        "dangerousAcceptInvalidHostnames",
        "dangerous-accept-invalid-hostnames",
    ]
    .iter()
    .any(|key| value.get(key).and_then(Value::as_bool) == Some(true))
}

pub(crate) struct UpdaterRegistration {
    pub(crate) service: UpdaterService,
    pub(crate) runtime: UpdaterRuntime,
    pub(crate) failure: Option<UpdaterInitializationFailure>,
}

pub(crate) fn register(
    app: &AppHandle,
    settings: SettingsService,
    cleanup_before_exit: Arc<dyn Fn() + Send + Sync>,
    native_dialogs_available: bool,
    log: LocalLog,
) -> UpdaterRegistration {
    let config = app.config().plugins.0.get("updater");
    match configured_updater(config) {
        Ok(ConfiguredUpdater::Unconfigured) => UpdaterRegistration {
            service: UpdaterService::unconfigured(&settings),
            runtime: UpdaterRuntime::empty(),
            failure: None,
        },
        Err(failure) => UpdaterRegistration {
            service: UpdaterService::initialization_failed(&settings),
            runtime: UpdaterRuntime::empty(),
            failure: Some(failure),
        },
        Ok(ConfiguredUpdater::Configured) if !native_dialogs_available => UpdaterRegistration {
            service: UpdaterService::initialization_failed(&settings),
            runtime: UpdaterRuntime::empty(),
            failure: Some(UpdaterInitializationFailure),
        },
        Ok(ConfiguredUpdater::Configured) => {
            match register_configured(app, settings.clone(), cleanup_before_exit, log.clone()) {
                Ok((service, runtime)) => UpdaterRegistration {
                    service,
                    runtime,
                    failure: None,
                },
                Err(failure) => UpdaterRegistration {
                    service: UpdaterService::initialization_failed(&settings),
                    runtime: UpdaterRuntime::empty(),
                    failure: Some(failure),
                },
            }
        }
    }
}

fn register_configured(
    app: &AppHandle,
    settings: SettingsService,
    cleanup_before_exit: Arc<dyn Fn() + Send + Sync>,
    log: LocalLog,
) -> Result<(UpdaterService, UpdaterRuntime), UpdaterInitializationFailure> {
    if app
        .plugin(tauri_plugin_updater::Builder::new().build())
        .is_err()
    {
        return Err(UpdaterInitializationFailure);
    }
    let worker_exit = Arc::new(WorkerExit::running());
    #[cfg(target_os = "windows")]
    let native_exit = worker_exit.clone();
    let cleanup = cleanup_before_exit.clone();
    let updater = match app
        .updater_builder()
        .timeout(DISCOVERY_TIMEOUT)
        .on_before_exit(move || {
            #[cfg(target_os = "windows")]
            native_exit.mark_native_exit_pending();
            cleanup();
        })
        .build()
    {
        Ok(updater) => updater,
        Err(_) => {
            app.remove_plugin("updater");
            return Err(UpdaterInitializationFailure);
        }
    };
    UpdaterService::available_with_exit(
        settings,
        Arc::new(SystemClock),
        Box::new(TauriUpdateBackend { updater }),
        Arc::new(TauriUpdatePresentation { app: app.clone() }),
        Arc::new(TauriInstalledUpdate { app: app.clone() }),
        log,
        worker_exit,
    )
    .map_err(|_| {
        app.remove_plugin("updater");
        UpdaterInitializationFailure
    })
}

struct TauriUpdateBackend {
    updater: tauri_plugin_updater::Updater,
}

impl UpdateBackend for TauriUpdateBackend {
    fn check(&mut self) -> Result<Option<Box<dyn PendingUpdate>>, ()> {
        ::tauri::async_runtime::block_on(self.updater.check())
            .map(|update| {
                update.map(|mut update| {
                    update.timeout = Some(DOWNLOAD_TIMEOUT);
                    Box::new(TauriPendingUpdate { update }) as Box<dyn PendingUpdate>
                })
            })
            .map_err(|_| ())
    }
}

struct TauriPendingUpdate {
    update: tauri_plugin_updater::Update,
}

impl PendingUpdate for TauriPendingUpdate {
    fn announced_version(&self) -> &str {
        &self.update.version
    }

    fn download(
        &mut self,
        progress: &mut dyn FnMut(usize, Option<u64>),
        download_finished: &mut dyn FnMut(),
    ) -> Result<Vec<u8>, UpdateFailureStage> {
        ::tauri::async_runtime::block_on(self.update.download(progress, download_finished))
            .map_err(classify_download_error)
    }

    fn install(&mut self, bytes: Vec<u8>) -> Result<(), UpdateFailureStage> {
        self.update
            .install(bytes)
            .map_err(|_| UpdateFailureStage::Install)
    }
}

fn classify_download_error(error: TauriUpdaterError) -> UpdateFailureStage {
    match error {
        TauriUpdaterError::Minisign(_)
        | TauriUpdaterError::Base64(_)
        | TauriUpdaterError::SignatureUtf8(_) => UpdateFailureStage::Verification,
        _ => UpdateFailureStage::Download,
    }
}

struct TauriUpdatePresentation {
    app: AppHandle,
}

impl UpdatePresentation for TauriUpdatePresentation {
    fn information(&self, message: &'static str) {
        self.app
            .dialog()
            .message(message)
            .title("Chiù Updates")
            .kind(MessageDialogKind::Info)
            .show(|_| {});
    }

    fn error(&self, message: &'static str) {
        self.app
            .dialog()
            .message(message)
            .title("Chiù Updates")
            .kind(MessageDialogKind::Error)
            .show(|_| {});
    }

    fn confirm_install(&self, version: Option<String>, callback: Box<dyn FnOnce(bool) + Send>) {
        let message = version.map_or_else(
            || "Install the available Chiù update now?".to_owned(),
            |version| format!("Install Chiù {version} now?"),
        );
        self.app
            .dialog()
            .message(message)
            .title("Chiù Update Available")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Install".to_owned(),
                "Later".to_owned(),
            ))
            .show(callback);
    }
}

struct TauriInstalledUpdate {
    app: AppHandle,
}

impl InstalledUpdate for TauriInstalledUpdate {
    fn finish(&self) -> Result<(), ()> {
        #[cfg(target_os = "macos")]
        self.app.request_restart();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_signature_failures_are_classified_before_installation() {
        assert_eq!(
            classify_download_error(TauriUpdaterError::SignatureUtf8("invalid".to_owned())),
            UpdateFailureStage::Verification
        );
    }
}
