# Architecture

This overview describes stable ownership boundaries for contributors. Specific
filenames and internal types are not part of the product contract.

The durable product language lives in [CONTEXT.md](../CONTEXT.md). Changes that
affect correctness or public trust should also be evaluated against
[REVIEW.md](../REVIEW.md).

## Runtime ownership

One application lifecycle owns startup and shutdown. It restores required
state before long-running capabilities start, distinguishes required from
optional startup failures, publishes readiness, and releases acquired
resources in reverse dependency order. Cleanup continues after an individual
failure so one optional capability cannot strand another resource.

The lifecycle is the ownership point for process state. Tray callbacks and
background workers do not independently decide whether the application is
ready or shutting down.

## Tray presentation

The native tray is a projection of authoritative product state and a source of
user commands. It displays application, manual-session, detector, automatic
policy, settings, power, launch-at-login, updater, diagnostics, and native
protection status.

Product rules do not live in menu callbacks. A tray command crosses a stable
interface into the owning module, then the next projection reflects the result.
This keeps failure states truthful and lets deterministic tests exercise the
same interface used by the native menu.

## Protection reasons and wake coordination

Manual sessions and automatic download policy produce independent wake
reasons. They never acquire platform sleep assertions directly. One wake
coordinator owns the set of active reasons and the single native inhibitor:

- the first reason acquires native protection;
- additional reasons share it;
- removing one reason preserves protection while another remains;
- removing the last reason releases protection; and
- failed acquisition remains observable rather than being treated as active.

New wake-producing features add a reason through the coordinator instead of
introducing a competing native assertion.

## Automatic download path

The automatic path separates four responsibilities:

1. Platform network adapters normalize aggregate receive counters and source
   continuity.
2. The detector converts samples and elapsed time into Waiting, Active, and
   Grace behavior.
3. Automatic policy combines qualified intent with persisted enablement,
   battery permission, and observed power.
4. The wake coordinator reconciles the resulting automatic reason with any
   manual reason and native protection state.

Power and settings changes can reconcile qualified intent without resetting
the detector. By contrast, lost network continuity clears qualification,
grace, and automatic intent before fresh samples can qualify.

## Durable settings

Settings are versioned, recoverable state. The settings owner loads fields,
records recovery health, persists acknowledged changes, keeps recoverable prior
state, and protects documents written by unsupported future schema versions.

Features consume settings through their owning interfaces. They do not parse
or write the settings file independently. Launch-at-login also distinguishes
persisted desired state from observed operating-system registration.

## Platform and application edges

Tauri, native menus, timers, filesystem access, network counters, power-source
observers, sleep inhibitors, clipboard access, directory opening, autostart,
and update integrations stay at the application/platform edge. Product rules
behind those seams remain deterministic and testable without a live Tauri
runtime or native side effect.

Windows and macOS adapters may differ internally while presenting equivalent
product facts. Linux-specific behavior is not part of the v0.1 interface.

## Optional capabilities

Update checking, launch at login, diagnostics, and logging are optional. Their
failures remain visible in the tray or diagnostics, but cannot block manual or
automatic keep-awake when required power functionality is healthy.

The updater has its own worker and shutdown completion requirements, but it
does not own the application lifecycle. Diagnostics take a bounded snapshot
through observable interfaces; they do not reach into private implementation
state or automatically transmit the result.

## Invariants to preserve

- Tray presentation never owns competing product state.
- One wake reason ending cannot cancel another.
- Requested protection is not reported as acquired protection.
- Source-continuity loss cannot leave stale automatic intent.
- Persisted preferences and observed native state remain distinct where the
  operating system can reject or obscure a change.
- Optional failures do not block core protection.
- Shutdown is ordered, idempotent, and does not skip later cleanup after an
  earlier failure.
- Platform APIs remain behind explicit seams at the application edge.
