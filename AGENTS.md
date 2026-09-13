# Chiù repository guidance

Chiù is a Rust-only Tauri 2 tray/menu-bar application. The entry
point is `src-tauri/src/main.rs`; Tauri configuration is in
`src-tauri/tauri.conf.json`; and the application manifest and lockfile are in
`src-tauri/Cargo.toml` and `src-tauri/Cargo.lock`.

Normal operation has no application webview or window. v0.1 supports Windows
11 x64 and macOS 14+; Linux is not a v0.1 product target. See
[PROJECT.md](PROJECT.md) for the authoritative identity, platform, release,
license, and publication facts.

## Quality checks

Run the same normal checks represented by pull-request CI:

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --all-targets --locked
cargo check --manifest-path src-tauri/Cargo.toml --all-targets --locked
```

For dependency or policy changes, use `deny.toml` and
`.github/workflows/dependency-policy.yml` as the source of the applicable
checks:

```bash
cargo audit --file src-tauri/Cargo.lock
cargo deny --manifest-path src-tauri/Cargo.toml --config deny.toml --locked check bans licenses sources
```

## Working rules

- Tauri and native OS integration belong at the application/platform edge.
  Keep product rules testable without a live Tauri runtime or native OS side
  effects.
- Test new behavior through stable observable seams; do not expose internals
  solely for tests.
- The active work item or pull request defines change scope. This repository
  records durable constraints; change-local implementation and module choices
  remain local unless promoted to repository guidance.
- Update the responsible guidance file in the same change when it settles a
  durable repository fact, product constraint, or review risk. Do not record
  speculative or change-local details.
- [CONTEXT.md](CONTEXT.md) records product direction,
  [REVIEW.md](REVIEW.md) is the project-specific review lens, and CI workflows
  are the executable quality source of truth. Do not add nested `AGENTS.md`
  files unless a subtree gains independent rules.
- Keep personal, machine-specific, and private workflow instructions out of
  this repository.
