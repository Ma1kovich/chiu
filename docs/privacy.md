# Privacy

Chiù keeps its data use small, local, and inspectable. It has no telemetry,
analytics identifier, crash-reporting service, or automatic diagnostic upload.

These boundaries apply to Chiù itself. The operating system and external
services have their own privacy behavior.

## 1. Automatic download detection

Chiù accesses aggregate received-byte counters supplied by the operating
system, together with timing and source-continuity information. It combines
operational interfaces into one receive rate and keeps the recent detector
state in memory.

It does **not** collect or inspect:

- packet contents or packet captures;
- URLs, hostnames, or DNS history;
- IP or MAC addresses;
- filenames or download names;
- application, process, or window activity; or
- which remote service caused the traffic.

Detection does not transmit network activity. Normal logs record availability
and state transitions rather than every sample or receive rate.

## 2. Settings and local application state

Chiù stores a versioned settings document locally. It contains product
preferences such as automatic protection, battery permission, launch at login,
update checking, detector tuning, and the cadence timestamp for automatic
update checks.

Recoverable prior state may be kept locally so malformed or partially invalid
settings do not make core operation unusable. Unsupported future-schema files
are preserved rather than normalized or overwritten. Settings are not
automatically uploaded.

Manual keep-awake state is process-scoped and is not persisted across restart.

## 3. Local logs

When logging is available, Chiù writes informational events and state
transitions to its local log directory. Events cover application lifecycle,
settings health, manual commands, detector transitions, automatic eligibility,
power state, wake-reason changes, native protection results, network
availability, launch-at-login results, updater stages, diagnostics actions, and
presentation failures.

Logs are bounded to one current file plus four archives. Each file is limited
to about 1 MiB, for an approximate maximum retained payload envelope of 5 MiB.
`Open logs` opens this local directory; it does not upload the files.

Logs exclude the packet, address, filename, application, device-name, username,
and arbitrary-path data listed above. They also omit per-network-sample traces.

## 4. Copied diagnostics

`Copy diagnostics` is an explicit user action. Chiù creates a fresh plain-text
snapshot in memory, caps it at 64 KiB, and copies it to the system clipboard.
It does not save a diagnostic bundle or upload the snapshot.

The snapshot can include:

- Chiù version and safe operating-system version/architecture context;
- application and optional-capability health;
- settings values and recovery status;
- manual, detector, automatic-policy, power, and native-protection state;
- aggregate receive counters and recent deltas;
- privacy-preserving opaque interface identifiers rather than display names;
- launch-at-login and updater status; and
- logging health and retention limits.

It excludes packet contents, URLs, hostnames, DNS history, IP/MAC addresses,
filenames, application/process activity, device hostname, username, and
arbitrary filesystem paths. Clipboard contents remain under the control of the
user and operating system, so review the text before sharing it.

## 5. Update network activity

Automatic download detection itself does not contact a Chiù server. A
configured updater can contact its release endpoint to check for metadata and,
after explicit confirmation, download an update. Automatic checks default to
enabled but are limited to at most one request per 24 hours; manual checks are
independent of that preference.

Update checking is not telemetry. Chiù does not attach diagnostics, activity
history, received-byte counters, filenames, URLs visited, or an analytics
identifier to the request. Every update download and installation attempt
requires explicit confirmation in the current session.

Updater failure is isolated from manual and automatic keep-awake behavior.

## Summary

| Data flow | Stored locally | Transmitted by Chiù |
| --- | --- | --- |
| Aggregate network detection | Recent state in memory; tuning in settings | No |
| Settings and recoverable state | Yes | No |
| Bounded logs | Yes, up to five files of about 1 MiB each | No |
| `Copy diagnostics` | In memory, then system clipboard | No automatic transmission |
| Configured update checks/downloads | Cadence and status locally | Requests only to the configured update endpoint |

For help collecting or sharing evidence safely, see
[Troubleshooting](troubleshooting.md).
