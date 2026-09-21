# Testing and validation

Chiù combines deterministic product rules with native Windows and macOS
integration. Both kinds of evidence matter, but they prove different things.

## Canonical local checks

Run the same normal checks represented by pull-request CI:

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --all-targets --locked
cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked
```

[AGENTS.md](../AGENTS.md) defines the canonical checks. For a dependency or
policy change, also run the audit and policy commands documented there.

## Deterministic behavior

Tests exercise product behavior through stable observable seams rather than
private implementation details. The deterministic suite covers:

- application startup, degraded readiness, ordered shutdown, and cleanup
  failures;
- manual session deadlines, extensions, expiry, and commands;
- detector qualification, grace, counter resets, and continuity loss;
- automatic power eligibility and reconciliation;
- overlapping wake reasons and truthful native acquisition failure;
- settings persistence, partial recovery, backup recovery, and future-schema
  protection;
- tray projection and command vocabulary;
- launch-at-login desired/observed state;
- updater cadence, consent, failure isolation, and shutdown completion; and
- bounded local logs and copied diagnostics.

These tests prove rules and state transitions. They do not prove that a native
operating-system request is honored on a particular machine.

## Native inspection aids

On Windows, inspect outstanding power requests with:

```powershell
powercfg /requests
```

While Chiù protection is active, the output should identify Chiù under a
`SYSTEM` request and should not show a Chiù `DISPLAY` request. The request
should disappear after protection ends or the process exits.

On macOS, inspect assertions with:

```bash
pmset -g assertions
```

While protection is active, the output should show a Chiù
`PreventUserIdleSystemSleep` assertion and no Chiù display-sleep assertion. It
should disappear after protection ends or the process exits.

These commands inspect native state; they do not replace a real sleep test.
Platform acceptance must also exercise actual idle-sleep behavior, release,
process exit, and relevant power-source transitions on supported hardware.

## CI evidence

Pull-request CI runs formatting, linting, tests, and a macOS compile check on a
macOS runner. A Windows job runs tests and a compile check on a hosted Windows
runner. A green compile job proves that the supported target builds in that
environment; it does not prove tray appearance, sleep behavior, autostart,
installer behavior, or clean native shutdown on a user's machine.

Platform-specific changes should record which Windows and macOS scenarios were
exercised, on what kind of host, and any limitation that remained. Evidence
from one supported operating system does not establish behavior on the other.

## Packaging validation

The [release workflow](../.github/workflows/release.yml) is started manually
with a new version number. It commits and tags that version, then builds:

- an unsigned Windows 11 x64 NSIS installer; and
- an ad-hoc signed universal macOS disk image.

Before upload, the workflow verifies the disk image's integrity, mounts it,
checks that it contains `Chiù.app` and an `Applications` shortcut, verifies the
complete application signature, and confirms that its executable includes both
Apple Silicon and Intel slices. It then creates a draft GitHub release,
generates release notes, and adds SHA-256 checksums. Review and complete
platform validation before publishing the draft.

Successful packaging does not prove drag installation, Gatekeeper approval,
first launch, tray/menu rendering, single-instance behavior, actual sleep
prevention, updater integration, or clean process exit. Real-machine macOS
validation must download the disk image normally, mount it, drag `Chiù.app` to
the `Applications` shortcut, and exercise the normal Gatekeeper approval path
when macOS blocks its first launch. Those behaviors require real-machine
validation.

## Updater validation

The promotion workflow verifies published release checksums and updater
signatures, rejects version rollback, deploys `latest.json` to GitHub Pages,
and verifies the served file. This proves publication, not installed update
behavior.

Validate updater changes with a published N-to-N+1 update on both supported
platforms. Exercise explicit consent, interruption and retry, invalid-signature
rejection, restart, settings preservation, and isolation from active
keep-awake reasons. Repeat the test for the alpha-to-stable transition.

## Recording validation

A pull request should state:

- the exact deterministic commands run;
- the supported platforms affected;
- native checks and real-machine scenarios actually performed;
- behavior that remains unverified; and
- whether packaging, signing, updater, permissions, or release-sensitive
  configuration changed.

Do not convert an unperformed native scenario into a documentation claim.
