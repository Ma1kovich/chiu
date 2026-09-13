use crate::{
    manual_session::{
        ManualSessionAddPreset as Add, ManualSessionCommand, ManualSessionStartPreset as Start,
    },
    tray::{KeepAwakeView, TrayCommand, TrayView},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ManualMenuEntry {
    Status {
        label: String,
        dynamic: bool,
    },
    Command {
        command: TrayCommand,
        label: &'static str,
        enabled: bool,
    },
    Separator,
}

pub(super) fn manual_menu_entries(view: &TrayView) -> Vec<ManualMenuEntry> {
    let command = |command, label| ManualMenuEntry::Command {
        command,
        label,
        enabled: view.commands_enabled,
    };
    match &view.keep_awake {
        KeepAwakeView::Inactive => vec![
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::FifteenMinutes)),
                "15 minutes",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::ThirtyMinutes)),
                "30 minutes",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::OneHour)),
                "1 hour",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::TwoHours)),
                "2 hours",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::FourHours)),
                "4 hours",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::EightHours)),
                "8 hours",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Start(Start::UntilDisabled)),
                "Until disabled",
            ),
        ],
        KeepAwakeView::Finite { remaining } => vec![
            ManualMenuEntry::Status {
                label: remaining.clone(),
                dynamic: true,
            },
            command(TrayCommand::Manual(ManualSessionCommand::Stop), "Stop"),
            ManualMenuEntry::Separator,
            command(
                TrayCommand::Manual(ManualSessionCommand::Add(Add::FifteenMinutes)),
                "Add 15 minutes",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Add(Add::ThirtyMinutes)),
                "Add 30 minutes",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Add(Add::OneHour)),
                "Add 1 hour",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Add(Add::TwoHours)),
                "Add 2 hours",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::Add(Add::FourHours)),
                "Add 4 hours",
            ),
            command(
                TrayCommand::Manual(ManualSessionCommand::ConvertToUntilDisabled),
                "Until disabled",
            ),
        ],
        KeepAwakeView::UntilDisabled => vec![
            ManualMenuEntry::Status {
                label: "Active until disabled".to_owned(),
                dynamic: false,
            },
            command(TrayCommand::Manual(ManualSessionCommand::Stop), "Stop"),
        ],
    }
}
