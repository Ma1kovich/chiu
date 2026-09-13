# Chiù product context

`CONTEXT.md` records durable product language. It does not choose modules,
traits, dependencies, thresholds, timing defaults, tray wording, or
implementation APIs.

## Product constraints and architecture

- Chiù is a tray/menu-bar utility. Normal operation must not require a product
  webview or window.
- v0.1 targets Windows 11 x64 and macOS 14+. Linux is not a v0.1 product
  target.
- While protection is active, Chiù prevents idle system sleep. Display sleep
  remains allowed, and explicit/manual sleep and lid semantics are respected as
  far as each platform permits.
- Native acceptance remains subject to operating-system power policy. On
  Windows Modern Standby systems using battery power, the operating system may
  stop honoring protection after the configured sleep timeout plus five
  minutes.
- Protection can have overlapping reasons. Releasing or expiring one reason
  must not release the platform inhibitor while another reason remains active.
- A manual keep-awake session is process-scoped and is either inactive, finite
  with one expiry deadline, or active until disabled. It is not restored after
  application restart.
- Adding time to a finite manual session extends its existing deadline rather
  than establishing a new deadline from the current time.
- The wake coordinator is the product-level ownership point for whether the
  platform sleep adapter is held or released. Features must not compete over
  native assertions independently.
- Tauri, tray, and native APIs are application-edge concerns. Detector, timer,
  policy, settings, and coordination rules should be deterministic and
  testable without embedding them in tray callbacks or OS API wrappers.
- One process-level lifecycle owns startup and shutdown of long-running
  application capabilities. Required prerequisites gate readiness, optional
  failures degrade only their capability, and shutdown releases acquired
  resources in reverse dependency order without skipping remaining cleanup.
- Durable settings are versioned and recover field by field where possible.
  Missing, malformed, or unavailable settings must leave core operation usable
  through safe defaults or a last-known-good copy, with recovery health kept
  observable for local diagnostics.
- A settings file from an unsupported future schema must not be normalized or
  overwritten. Settings changes remain blocked until a supported compatibility
  path exists.
- Network detection uses normalized aggregate receive activity, never packet
  inspection or application/process surveillance.
- Automatic download detection is Idle while activity is unqualified, Active
  after meaningful receive activity has qualified, and in Hold while a quiet
  grace period preserves already-qualified automatic protection.
- Automatic qualification and grace use the actual observed durations of
  normalized receive activity. Loss of source continuity clears prior
  qualification, grace, and automatic protection intent.
- Automatic protection is eligible only when detection is qualified, the
  feature is enabled, and either external power is present or protection on
  battery has been enabled. Unknown power is treated conservatively like a
  limited source unless battery protection is enabled.
- Power-source changes and relevant settings changes reconcile already-qualified
  automatic intent immediately; they do not restart network qualification.
- Persisted automatic-protection settings are authoritative. A failed settings
  write leaves live policy unchanged, while a later wake-coordination event can
  retry an otherwise persisted and desired policy transition.
- Suspend/resume, interface topology changes, counter resets, stale samples,
  and failed native acquisitions must not leave stale protection state or cause
  Chiù to claim protection it does not hold.
- Optional update checking, autostart, diagnostics, and logging must not be
  prerequisites for core keep-awake correctness.
- Update discovery is optional and may be intentionally unconfigured. Automatic
  discovery is bounded and never grants installation consent; every download and
  installation attempt requires an explicit current-session confirmation.
- The saved launch-at-login preference is durable desired state, while observed
  operating-system registration is separate native state that may be enabled,
  disabled, or unknown. Chiù reconciles the native state to the saved preference
  at startup and after an explicit command without making reconciliation failure
  fatal to core operation.
- v0.1 does not collect telemetry or automatically upload diagnostics. Its
  detector and diagnostics path must not collect packet contents, URLs,
  IP/MAC addresses, filenames, or process activity.

## Product capabilities

Chiù exposes manual keep-awake, power-aware automatic download protection,
detector threshold/grace controls, and launch-at-login integration through its
native tray. The tray shows application, policy, detector, settings, launch
registration, updater, and native protection state; it does not own competing
product state. Update discovery is available only when its endpoint and signing
metadata are configured. Chiù keeps bounded local logs and provides copyable
diagnostics when the required platform integrations are available.
