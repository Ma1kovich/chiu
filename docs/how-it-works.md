# How Chiù works

Chiù has one job: request that the operating system postpone **idle system
sleep** while there is a reason to stay awake. It is a tray/menu-bar utility and
has no normal application window.

## What protection means

When protection is active, Chiù asks the operating system to prevent idle
system sleep. It allows display sleep. An explicit sleep command,
closing a laptop lid, and other platform power policies keep their normal
meaning.

The tray distinguishes wanting protection from actually holding it. If Chiù
has an active reason but cannot acquire the native sleep request, it reports
`Protection failed`; it does not claim to be keeping the computer awake.

## Keep awake during downloads

`Keep awake during downloads` turns the automatic feature on or off. When it is
on, Chiù observes aggregate incoming network activity and waits for sustained
activity to qualify. Qualified activity creates an automatic wake reason. A
quiet grace period preserves that reason briefly before it is removed.

The automatic reason is eligible on external power. On battery, another
limited power source, or an unknown power source, it is eligible only when
`Keep awake for downloads on battery` is enabled. Changing either setting or
the observed power source reconciles an already-qualified detector immediately;
the activity does not have to qualify again.

See [Automatic download detection](automatic-detection.md) for the detector
states, defaults, tuning, and trade-offs.

## Keep awake manually

`Keep awake manually` offers these starting durations:

- 15 minutes
- 30 minutes
- 1 hour
- 2 hours
- 4 hours
- 8 hours
- Until disabled

While a finite session is running, it can be stopped, converted to `Until
disabled`, or extended by 15 minutes, 30 minutes, 1 hour, 2 hours, or 4 hours.
Adding time extends the current deadline rather than starting a new duration
from the moment of the command.

Manual protection is independent of the automatic feature and its battery
permission. It is process-scoped: quitting or restarting Chiù ends the manual
session, including `Until disabled`.

## Overlapping reasons

Automatic and manual protection may be active at the same time. They are
reasons for one shared native sleep request, not competing requests. Ending one
reason leaves protection active while the other remains. The native request is
released only after the last reason ends.

## Settings and launch at login

Chiù persists its automatic-protection, battery, launch-at-login,
download-tuning, and automatic-update preferences. Missing or malformed fields
recover independently where possible; the tray reports when safe defaults or a
backup were needed. A settings document from an unsupported future version is
preserved and changes are blocked rather than overwriting it.

The launch-at-login setting is desired state. Chiù asks the operating system to
match that preference at startup and after a change, then observes whether
registration is enabled, disabled, or unknown. A registration failure is shown
as unavailable or failed without disabling manual protection.

## Optional update checking

When update discovery is configured, update checking is independent of
keep-awake behavior. The preference defaults to enabled, but an automatic check
is limited to at most once per 24 hours. A manual check remains available even
when automatic checking is disabled.

Finding an update never authorizes installation. Every download and install
attempt requires an explicit confirmation in the current Chiù session. An
offline or failed updater does not disable core protection.

When update discovery is unavailable, its controls are disabled without
affecting keep-awake behavior. See [Releasing](releasing.md) for update
distribution policy.

## Startup and shutdown

Required capabilities must start before Chiù reports readiness. Optional
capabilities such as update checking, launch at login, diagnostics, and local
logging may be unavailable without preventing core keep-awake behavior.

During shutdown, Chiù stops accepting work, clears automatic intent, ends the
manual session, and releases acquired native protection. Cleanup continues even
if one optional step fails.
