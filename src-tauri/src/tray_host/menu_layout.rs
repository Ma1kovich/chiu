use crate::tray::{TrayCommand, TrayView};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TopLevelItem {
    Status {
        id: &'static str,
        label: String,
    },
    Separator,
    CheckedCommand {
        command: TrayCommand,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Submenu {
        id: &'static str,
        label: &'static str,
        enabled: bool,
    },
    Command {
        command: TrayCommand,
        label: String,
        enabled: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SettingsItem {
    Separator,
    CheckedCommand {
        command: TrayCommand,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Command {
        command: TrayCommand,
        label: String,
        enabled: bool,
    },
    Status {
        id: &'static str,
        label: String,
    },
    About {
        label: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MenuToken {
    Item(String),
    Separator,
    About,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MenuLayoutShape {
    pub(super) detail: bool,
    pub(super) warning: bool,
    pub(super) updater_result: bool,
    pub(super) updater_install: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MenuLayout {
    top_level: Vec<TopLevelItem>,
    settings: Vec<SettingsItem>,
}

impl MenuLayout {
    pub(super) fn from(view: &TrayView) -> Self {
        let mut top_level = vec![TopLevelItem::Status {
            id: "status.primary",
            label: view.primary.clone(),
        }];
        if let Some(detail) = &view.detail {
            top_level.push(TopLevelItem::Status {
                id: "status.detail",
                label: detail.clone(),
            });
        }
        if let Some(warning) = &view.warning {
            top_level.push(TopLevelItem::Status {
                id: "status.warning",
                label: warning.clone(),
            });
        }
        top_level.extend([
            TopLevelItem::Separator,
            TopLevelItem::CheckedCommand {
                command: TrayCommand::ToggleAutomaticProtection,
                label: "Keep awake during downloads".to_owned(),
                enabled: view.commands_enabled,
                checked: view.automatic_checked,
            },
            TopLevelItem::CheckedCommand {
                command: TrayCommand::ToggleProtectOnBattery,
                label: "Keep awake for downloads on battery".to_owned(),
                enabled: view.commands_enabled,
                checked: view.protect_on_battery_checked,
            },
            TopLevelItem::Submenu {
                id: "detection",
                label: "Download detection",
                enabled: true,
            },
            TopLevelItem::Submenu {
                id: "manual",
                label: "Keep awake manually",
                enabled: view.commands_enabled,
            },
            TopLevelItem::Separator,
            TopLevelItem::Submenu {
                id: "settings",
                label: "Settings",
                enabled: true,
            },
            TopLevelItem::Command {
                command: TrayCommand::Quit,
                label: "Quit Chiù".to_owned(),
                enabled: true,
            },
        ]);
        let mut settings = vec![
            SettingsItem::CheckedCommand {
                command: TrayCommand::ToggleLaunchAtLogin,
                label: view.launch_at_login.label.clone(),
                enabled: view.launch_at_login.enabled,
                checked: view.launch_at_login.checked,
            },
            SettingsItem::Separator,
            SettingsItem::CheckedCommand {
                command: TrayCommand::ToggleAutomaticUpdates,
                label: view.updater.automatic_label.clone(),
                enabled: view.updater.automatic_enabled,
                checked: view.updater.automatic_checked,
            },
            SettingsItem::Command {
                command: TrayCommand::CheckUpdates,
                label: view.updater.check_label.clone(),
                enabled: view.updater.check_enabled,
            },
        ];
        if let Some(result) = &view.updater.result {
            settings.push(SettingsItem::Status {
                id: "updater.result",
                label: result.clone(),
            });
        }
        if let Some((label, enabled)) = &view.updater.install {
            settings.push(SettingsItem::Command {
                command: TrayCommand::InstallUpdate,
                label: label.clone(),
                enabled: *enabled,
            });
        }
        settings.extend([
            SettingsItem::Separator,
            SettingsItem::Command {
                command: TrayCommand::CopyDiagnostics,
                label: view.diagnostics.copy_label.clone(),
                enabled: view.diagnostics.copy_enabled,
            },
            SettingsItem::Command {
                command: TrayCommand::OpenLogs,
                label: view.diagnostics.open_logs_label.clone(),
                enabled: view.diagnostics.open_logs_enabled,
            },
            SettingsItem::Separator,
            SettingsItem::About {
                label: "About Chiù".to_owned(),
            },
        ]);
        Self {
            top_level,
            settings,
        }
    }

    pub(super) fn top_level(&self) -> &[TopLevelItem] {
        &self.top_level
    }

    pub(super) fn settings(&self) -> &[SettingsItem] {
        &self.settings
    }

    pub(super) fn shape(&self) -> MenuLayoutShape {
        MenuLayoutShape {
            detail: self.top_level.iter().any(|entry| {
                matches!(
                    entry,
                    TopLevelItem::Status {
                        id: "status.detail",
                        ..
                    }
                )
            }),
            warning: self.top_level.iter().any(|entry| {
                matches!(
                    entry,
                    TopLevelItem::Status {
                        id: "status.warning",
                        ..
                    }
                )
            }),
            updater_result: self.settings.iter().any(|entry| {
                matches!(
                    entry,
                    SettingsItem::Status {
                        id: "updater.result",
                        ..
                    }
                )
            }),
            updater_install: self.settings.iter().any(|entry| {
                matches!(
                    entry,
                    SettingsItem::Command {
                        command: TrayCommand::InstallUpdate,
                        ..
                    }
                )
            }),
        }
    }

    pub(super) fn top_level_tokens(&self) -> Vec<MenuToken> {
        self.top_level
            .iter()
            .map(|entry| match entry {
                TopLevelItem::Status { id, .. } | TopLevelItem::Submenu { id, .. } => {
                    MenuToken::Item((*id).to_owned())
                }
                TopLevelItem::Separator => MenuToken::Separator,
                TopLevelItem::CheckedCommand { command, .. }
                | TopLevelItem::Command { command, .. } => MenuToken::Item(command.id().to_owned()),
            })
            .collect()
    }

    pub(super) fn settings_tokens(&self) -> Vec<MenuToken> {
        self.settings
            .iter()
            .map(|entry| match entry {
                SettingsItem::Separator => MenuToken::Separator,
                SettingsItem::CheckedCommand { command, .. }
                | SettingsItem::Command { command, .. } => MenuToken::Item(command.id().to_owned()),
                SettingsItem::Status { id, .. } => MenuToken::Item((*id).to_owned()),
                SettingsItem::About { .. } => MenuToken::About,
            })
            .collect()
    }
}
