# Chiù review lens

For changes affecting correctness or public trust, review the following when
relevant:

- **Power lifecycle:** Are acquisition and release symmetric? Does shutdown
  clean up? Are failed acquisitions truthful, and do suspend/resume transitions
  avoid stale native assertions?
- **Power-source callbacks:** Are callback inputs validated before dereference,
  panics contained at the FFI boundary, and callback contexts kept alive until
  native unregistration or run-loop source invalidation is complete? Can late
  Windows callbacks safely become no-ops, and does macOS teardown leave the
  application run loop running?
- **Automatic power policy:** Do detector intent, persisted enablement, battery
  permission, and the observed power source reconcile through one policy owner?
  Are unknown power and observer failures conservative, and do power/settings
  changes preserve detector qualification?
- **Overlapping wake reasons:** Can one reason stopping ever cancel another?
  Do duplicated or reordered events preserve correct state?
- **State ownership:** Does tray/UI code project state and send commands rather
  than own product rules? Do platform adapters avoid deciding product policy?
- **Settings durability:** Do partial or corrupt files recover without blocking
  core operation? Does every acknowledged write retain a recoverable prior
  state, and are unsupported future schemas protected from destructive writes?
- **Network sampling:** Can counter resets, topology changes, or resume create
  artificial activity spikes or stale activity?
- **Platform seams:** Do Windows and macOS differences remain behind explicit
  seams? Does the change avoid adding Linux to the v0.1 contract?
- **Privacy:** Does the change add telemetry, automatic uploads, packet or
  process surveillance, or unnecessarily identifying diagnostics?
- **Tests:** Are important behaviors verified through stable observable seams,
  including lifecycle and error transitions, rather than only happy paths or
  implementation details?
- **Release-sensitive changes:** Have application identity, bundle metadata,
  permissions/capabilities, signing, updater or release configuration, CI
  permissions, dependency-policy exceptions, and new native APIs received
  explicit scrutiny?
