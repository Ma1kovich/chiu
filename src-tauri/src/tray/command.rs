use crate::manual_session::{
    ManualSessionAddPreset, ManualSessionCommand, ManualSessionStartPreset,
};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThresholdPreset {
    Kibibytes256,
    Mebibyte1,
    Mebibytes5,
}

impl ThresholdPreset {
    pub(crate) const fn bytes_per_second(self) -> u64 {
        match self {
            Self::Kibibytes256 => 262_144,
            Self::Mebibyte1 => 1_048_576,
            Self::Mebibytes5 => 5_242_880,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GracePreset {
    Seconds30,
    Minutes2,
    Minutes5,
    Minutes15,
    Minutes30,
}

impl GracePreset {
    pub(crate) const ALL: [Self; 5] = [
        Self::Seconds30,
        Self::Minutes2,
        Self::Minutes5,
        Self::Minutes15,
        Self::Minutes30,
    ];

    pub(crate) const fn duration(self) -> Duration {
        match self {
            Self::Seconds30 => Duration::from_secs(30),
            Self::Minutes2 => Duration::from_secs(120),
            Self::Minutes5 => Duration::from_secs(300),
            Self::Minutes15 => Duration::from_secs(900),
            Self::Minutes30 => Duration::from_secs(1_800),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrayCommand {
    ToggleAutomaticProtection,
    ToggleProtectOnBattery,
    ToggleLaunchAtLogin,
    ToggleAutomaticUpdates,
    CheckUpdates,
    InstallUpdate,
    CopyDiagnostics,
    OpenLogs,
    Manual(ManualSessionCommand),
    SetThreshold(ThresholdPreset),
    SetGrace(GracePreset),
    Quit,
}

macro_rules! command_vocabulary {
    (
        commands { $($command:ident => $command_id:literal),+ $(,)? }
        starts { $($start:ident => $start_id:literal),+ $(,)? }
        adds { $($add:ident => $add_id:literal),+ $(,)? }
        manual { $($manual:ident => $manual_id:literal),+ $(,)? }
        thresholds { $($threshold:ident => $threshold_id:literal),+ $(,)? }
        grace { $($grace:ident => $grace_id:literal),+ $(,)? }
        final_command { $final_command:ident => $final_id:literal }
    ) => {
        impl TrayCommand {
            pub(crate) const fn id(self) -> &'static str {
                match self {
                    $(Self::$command => $command_id,)+
                    $(Self::Manual(ManualSessionCommand::Start(
                        ManualSessionStartPreset::$start,
                    )) => $start_id,)+
                    $(Self::Manual(ManualSessionCommand::Add(
                        ManualSessionAddPreset::$add,
                    )) => $add_id,)+
                    $(Self::Manual(ManualSessionCommand::$manual) => $manual_id,)+
                    $(Self::SetThreshold(ThresholdPreset::$threshold) => $threshold_id,)+
                    $(Self::SetGrace(GracePreset::$grace) => $grace_id,)+
                    Self::$final_command => $final_id,
                }
            }

            pub(crate) fn from_id(id: &str) -> Option<Self> {
                ALL_COMMANDS
                    .iter()
                    .copied()
                    .find(|command| command.id() == id)
            }
        }

        const ALL_COMMANDS: [TrayCommand; 31] = [
            $(TrayCommand::$command,)+
            $(TrayCommand::Manual(ManualSessionCommand::Start(
                ManualSessionStartPreset::$start,
            )),)+
            $(TrayCommand::Manual(ManualSessionCommand::Add(
                ManualSessionAddPreset::$add,
            )),)+
            $(TrayCommand::Manual(ManualSessionCommand::$manual),)+
            $(TrayCommand::SetThreshold(ThresholdPreset::$threshold),)+
            $(TrayCommand::SetGrace(GracePreset::$grace),)+
            TrayCommand::$final_command,
        ];
    };
}

command_vocabulary! {
    commands {
        ToggleAutomaticProtection => "automatic.toggle",
        ToggleProtectOnBattery => "battery.toggle",
        ToggleLaunchAtLogin => "future.launch-at-login",
        ToggleAutomaticUpdates => "future.automatic-updates",
        CheckUpdates => "future.check-updates",
        InstallUpdate => "updater.install",
        CopyDiagnostics => "future.copy-diagnostics",
        OpenLogs => "future.open-logs",
    }
    starts {
        FifteenMinutes => "manual.start.15m",
        ThirtyMinutes => "manual.start.30m",
        OneHour => "manual.start.1h",
        TwoHours => "manual.start.2h",
        FourHours => "manual.start.4h",
        EightHours => "manual.start.8h",
        UntilDisabled => "manual.start.until-disabled",
    }
    adds {
        FifteenMinutes => "manual.add.15m",
        ThirtyMinutes => "manual.add.30m",
        OneHour => "manual.add.1h",
        TwoHours => "manual.add.2h",
        FourHours => "manual.add.4h",
    }
    manual {
        ConvertToUntilDisabled => "manual.convert.until-disabled",
        Stop => "manual.stop",
    }
    thresholds {
        Kibibytes256 => "detection.threshold.262144",
        Mebibyte1 => "detection.threshold.1048576",
        Mebibytes5 => "detection.threshold.5242880",
    }
    grace {
        Seconds30 => "detection.grace.30",
        Minutes2 => "detection.grace.120",
        Minutes5 => "detection.grace.300",
        Minutes15 => "detection.grace.900",
        Minutes30 => "detection.grace.1800",
    }
    final_command { Quit => "quit" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn command_vocabulary_preserves_unique_stable_ids_and_round_trips() {
        let expected = [
            "automatic.toggle",
            "battery.toggle",
            "future.launch-at-login",
            "future.automatic-updates",
            "future.check-updates",
            "updater.install",
            "future.copy-diagnostics",
            "future.open-logs",
            "manual.start.15m",
            "manual.start.30m",
            "manual.start.1h",
            "manual.start.2h",
            "manual.start.4h",
            "manual.start.8h",
            "manual.start.until-disabled",
            "manual.add.15m",
            "manual.add.30m",
            "manual.add.1h",
            "manual.add.2h",
            "manual.add.4h",
            "manual.convert.until-disabled",
            "manual.stop",
            "detection.threshold.262144",
            "detection.threshold.1048576",
            "detection.threshold.5242880",
            "detection.grace.30",
            "detection.grace.120",
            "detection.grace.300",
            "detection.grace.900",
            "detection.grace.1800",
            "quit",
        ];
        let actual = ALL_COMMANDS.map(TrayCommand::id);

        assert_eq!(actual, expected);
        assert_eq!(
            actual.into_iter().collect::<HashSet<_>>().len(),
            actual.len()
        );
        for command in ALL_COMMANDS {
            assert_eq!(TrayCommand::from_id(command.id()), Some(command));
        }
    }

    #[test]
    fn non_actionable_and_inexact_ids_do_not_decode() {
        for id in [
            "status.primary",
            "manual",
            "manual.remaining",
            "detection",
            "detection.threshold",
            "detection.threshold.custom",
            "detection.grace.custom",
            "updater.result",
            "settings",
            "automatic",
            "automatic.toggle.extra",
            "Automatic.toggle",
            " automatic.toggle",
            "automatic.toggle ",
            "",
        ] {
            assert_eq!(TrayCommand::from_id(id), None, "{id:?}");
        }
    }
}
