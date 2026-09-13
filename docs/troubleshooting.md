# Troubleshooting

Start with the tray. Its primary status says whether Chiù is ready, keeping the
computer awake, unable to detect network activity, or unable to acquire native
protection. An optional detail or warning explains the most relevant reason.

## Chiù does not open on first launch on macOS

Chiù's macOS package uses ad-hoc signing and is not notarized, so it may require
one explicit Gatekeeper approval:

1. Confirm that the package came from the project's GitHub Releases page.
2. Try to open Chiù once and close the warning.
3. Open **System Settings → Privacy & Security**.
4. Choose **Open Anyway** for Chiù, then confirm **Open**.

Follow [Apple's current instructions](https://support.apple.com/en-gb/102445).
Do not disable Gatekeeper or remove quarantine attributes with a command.

## Windows sleeps while automatic protection is allowed on battery

On a Modern Standby system, Windows may stop honoring an ordinary application
power request on battery after the configured sleep timeout plus at most five
minutes. The same request can block the pre-sleep phase indefinitely on AC
power. This is an operating-system limitation; the battery setting cannot
override it.

See [Microsoft's Modern Standby guidance](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/prepare-software-for-modern-standby).

## Keep awake during downloads does not start

Check these conditions in order:

1. `Keep awake during downloads` is checked.
2. On battery, limited power, or an unknown power source, `Keep awake for
   downloads on battery` is checked.
3. The `Download detection` submenu shows a receive rate and does not report an
   unavailable network source.
4. Activity remains above the selected threshold for the five-second
   qualification period.

The thresholds are 256 KiB/s, 1 MiB/s, and 5 MiB/s. The default is 1 MiB/s.
Network activity below the selected threshold correctly remains in Waiting.

After a source interruption, counter reset, or interface change, Chiù clears
stale detector history and requires fresh activity to qualify. If the tray
continues to say `Can’t detect network activity`, use `Copy diagnostics` and
`Open logs` before filing a bug.

## The tray says `Protection failed`

Chiù has a valid automatic or manual reason but the operating system sleep
request was not acquired. This differs from `Keeping awake`.

Manual and automatic reasons share the same native request, so changing
detector settings is not a fix for a native acquisition failure. Capture
diagnostics and logs, note the operating system version and whether the machine
was on AC or battery, and file a bug. Do not assume the computer is protected
while this status is visible.

## Detection feels too sensitive or not sensitive enough

Open `Download detection` and adjust one setting at a time:

- Raise the receive-rate threshold to reduce activation from lighter
  background traffic.
- Lower it when legitimate transfers are too slow to qualify.
- Shorten grace when protection remains active too long after traffic stops.
- Lengthen grace when expected transfers have brief quiet intervals.

The detector sees aggregate network traffic, not semantic downloads. See
[Automatic download detection](automatic-detection.md) before tuning around a
specific workload.

## Launch at login is unavailable or failed

The saved preference and the operating system's observed registration are
separate. Chiù tries to reconcile them at startup and after you change the
setting. If registration is unavailable or the operation fails, the tray shows
that result rather than treating it as successful.

Core manual and automatic protection remain usable. Start Chiù normally,
collect diagnostics, and report the registration result if the failure
persists.

## Update checking is unavailable or failed

Unavailable controls mean update discovery is not configured or could not be
initialized. An offline or failed check affects only the updater; it does not
mean keep-awake protection has failed.

When update discovery is configured, automatic checking can be disabled while
retaining manual checks. Finding an update never installs it automatically:
every download and install attempt requires confirmation in the current
session.

## Settings were recovered or cannot be saved

Chiù recovers malformed settings field by field where possible and may use a
last-known-good backup or safe defaults. The tray warns when recovery occurred.
Review the visible settings before continuing.

If settings cannot be saved, the tray reports the failure and leaves the live
policy unchanged. A settings file from an unsupported future version is
preserved and changes remain unavailable rather than overwriting it.

## Collecting useful evidence

Under `Settings`:

- `Copy diagnostics` creates a fresh, bounded text snapshot in memory and
  copies it to the clipboard. Review it before sharing.
- `Open logs` opens Chiù's local log directory when logging is available.

Neither action uploads anything. Logs and diagnostics exclude
packet contents, hostnames, URLs, addresses, filenames, usernames, and process
activity. See [Privacy](privacy.md) for the complete boundary.

Use an ordinary GitHub issue for reproducible bugs after removing any unrelated
personal information. Do not post suspected vulnerabilities, exploit details,
secrets, or sensitive diagnostics in a public issue; follow the
[security policy](../SECURITY.md).
