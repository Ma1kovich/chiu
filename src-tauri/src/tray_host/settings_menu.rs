use super::menu_layout::{MenuLayout, MenuToken, SettingsItem, TopLevelItem};
use super::{CheckMenuItem, MenuItem, Submenu, command_item, disabled_item, ensure_native_access};
use tauri::{
    AppHandle,
    menu::{AboutMetadata, PredefinedMenuItem},
};

pub(super) struct SettingsMenu {
    submenu: Submenu,
    items: Vec<NativeSettingsItem>,
}

enum NativeSettingsItem {
    Separator(PredefinedMenuItem<tauri::Wry>),
    CheckedCommand(CheckMenuItem),
    Command(MenuItem),
    Status(MenuItem),
    About(PredefinedMenuItem<tauri::Wry>),
}

impl SettingsMenu {
    pub(super) fn build(app: &AppHandle, layout: &MenuLayout) -> tauri::Result<Self> {
        let (label, enabled) = container(layout)?;
        let submenu = Submenu::with_id(app, "settings", label, enabled)?;
        let mut items = Vec::with_capacity(layout.settings().len());
        for entry in layout.settings() {
            let item = NativeSettingsItem::build(app, entry)?;
            item.append_to(&submenu)?;
            items.push(item);
        }
        let settings = Self { submenu, items };
        settings.verify(layout)?;
        Ok(settings)
    }

    pub(super) fn submenu(&self) -> &Submenu {
        &self.submenu
    }

    pub(super) fn update(&self, layout: &MenuLayout) -> tauri::Result<()> {
        let (label, enabled) = container(layout)?;
        self.submenu.set_text(label)?;
        self.submenu.set_enabled(enabled)?;
        ensure_native_access(
            "Settings menu shape",
            self.items.len() == layout.settings().len(),
            true,
        )?;
        for (native, expected) in self.items.iter().zip(layout.settings()) {
            native.update(expected)?;
        }
        self.verify(layout)
    }

    fn verify(&self, layout: &MenuLayout) -> tauri::Result<()> {
        let (_, enabled) = container(layout)?;
        ensure_native_access("Settings", self.submenu.is_enabled()?, enabled)?;
        ensure_native_access(
            "Settings menu shape",
            self.items.len() == layout.settings().len(),
            true,
        )?;
        for (native, expected) in self.items.iter().zip(layout.settings()) {
            native.verify(expected)?;
        }

        let about_id = self.items.iter().find_map(|item| match item {
            NativeSettingsItem::About(item) => Some(item.id()),
            _ => None,
        });
        let actual = self
            .submenu
            .items()?
            .iter()
            .map(|item| {
                if Some(item.id()) == about_id {
                    MenuToken::About
                } else if item.as_predefined_menuitem().is_some() {
                    MenuToken::Separator
                } else {
                    MenuToken::Item(item.id().as_ref().to_owned())
                }
            })
            .collect::<Vec<_>>();
        ensure_native_access(
            "Settings child ordering and grouping",
            actual == layout.settings_tokens(),
            true,
        )
    }
}

impl NativeSettingsItem {
    fn build(app: &AppHandle, entry: &SettingsItem) -> tauri::Result<Self> {
        Ok(match entry {
            SettingsItem::Separator => Self::Separator(PredefinedMenuItem::separator(app)?),
            SettingsItem::CheckedCommand {
                command,
                label,
                enabled,
                checked,
            } => Self::CheckedCommand(CheckMenuItem::with_id(
                app,
                command.id(),
                label,
                *enabled,
                *checked,
                None::<&str>,
            )?),
            SettingsItem::Command {
                command,
                label,
                enabled,
            } => Self::Command(command_item(app, *command, label, *enabled)?),
            SettingsItem::Status { id, label } => Self::Status(disabled_item(app, id, label)?),
            SettingsItem::About { label } => Self::About(PredefinedMenuItem::about(
                app,
                Some(label),
                Some(AboutMetadata {
                    name: Some("Chiù".to_owned()),
                    version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    ..Default::default()
                }),
            )?),
        })
    }

    fn append_to(&self, submenu: &Submenu) -> tauri::Result<()> {
        match self {
            Self::Separator(item) | Self::About(item) => submenu.append(item),
            Self::CheckedCommand(item) => submenu.append(item),
            Self::Command(item) | Self::Status(item) => submenu.append(item),
        }
    }

    fn update(&self, expected: &SettingsItem) -> tauri::Result<()> {
        match (self, expected) {
            (Self::Separator(_), SettingsItem::Separator)
            | (Self::About(_), SettingsItem::About { .. }) => Ok(()),
            (
                Self::CheckedCommand(item),
                SettingsItem::CheckedCommand {
                    label,
                    enabled,
                    checked,
                    ..
                },
            ) => {
                item.set_text(label)?;
                item.set_enabled(*enabled)?;
                item.set_checked(*checked)
            }
            (Self::Command(item), SettingsItem::Command { label, enabled, .. }) => {
                item.set_text(label)?;
                item.set_enabled(*enabled)
            }
            (Self::Status(item), SettingsItem::Status { label, .. }) => item.set_text(label),
            _ => Err(shape_mismatch()),
        }
    }

    fn verify(&self, expected: &SettingsItem) -> tauri::Result<()> {
        match (self, expected) {
            (Self::Separator(_), SettingsItem::Separator) => Ok(()),
            (
                Self::CheckedCommand(item),
                SettingsItem::CheckedCommand {
                    command,
                    label,
                    enabled,
                    checked,
                },
            ) => {
                ensure_text(command.id(), item.id().as_ref(), command.id())?;
                ensure_text(command.id(), &item.text()?, label)?;
                ensure_native_access(command.id(), item.is_enabled()?, *enabled)?;
                ensure_native_access(
                    &format!("{} checked state", command.id()),
                    item.is_checked()?,
                    *checked,
                )
            }
            (
                Self::Command(item),
                SettingsItem::Command {
                    command,
                    label,
                    enabled,
                },
            ) => {
                ensure_text(command.id(), item.id().as_ref(), command.id())?;
                ensure_text(command.id(), &item.text()?, label)?;
                ensure_native_access(command.id(), item.is_enabled()?, *enabled)
            }
            (Self::Status(item), SettingsItem::Status { id, label }) => {
                ensure_text(id, item.id().as_ref(), id)?;
                ensure_text(id, &item.text()?, label)?;
                ensure_native_access(id, item.is_enabled()?, false)
            }
            (Self::About(item), SettingsItem::About { label }) => {
                ensure_text("About Chiù", &item.text()?, label)
            }
            _ => Err(shape_mismatch()),
        }
    }
}

fn container(layout: &MenuLayout) -> tauri::Result<(&str, bool)> {
    layout
        .top_level()
        .iter()
        .find_map(|entry| match entry {
            TopLevelItem::Submenu {
                id: "settings",
                label,
                enabled,
            } => Some((*label, *enabled)),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("missing Settings menu container").into())
}

fn ensure_text(name: &str, actual: &str, expected: &str) -> tauri::Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "native menu text for {name} was {actual:?}, expected {expected:?}"
        ))
        .into())
    }
}

fn shape_mismatch() -> tauri::Error {
    std::io::Error::other("native Settings menu shape does not match its layout").into()
}
