# Releasing

GitHub Releases is Chiù's download and release-notes channel.

## Versioning and release notes

Chiù uses [Semantic Versioning](https://semver.org/). Stable tags use
`vMAJOR.MINOR.PATCH`; SemVer prerelease identifiers are allowed, for example
`v0.1.0-alpha.1`.

Release notes belong in GitHub Releases. The project intentionally does not
maintain a handwritten `CHANGELOG.md`.

To make a release, run the **Release** workflow from the GitHub Actions page and
enter the new version without a `v` prefix. The workflow updates
`src-tauri/Cargo.toml` and `src-tauri/Cargo.lock`, commits the version to the
default branch, creates the matching `v` tag, builds both desktop packages, and
creates a draft GitHub release with generated notes and SHA-256 checksums.
Review the draft before publishing it. The Windows installer is unsigned. The
universal macOS disk image contains an ad-hoc signed application: it seals the
code, but does not identify a trusted publisher and is neither Developer ID
signed nor notarized. Do not publish the draft until its disk image has been
downloaded normally, mounted, installed by dragging `Chiù.app` to the
`Applications` shortcut, and first-launched on a supported Mac using the
Gatekeeper approval path below.

A release provides:

- a Windows 11 x64 installer; and
- a universal macOS disk image containing an application with Intel and Apple
  Silicon slices.

When updater support is configured, the draft also contains the signed Windows
installer, the signed universal macOS updater archive, and their detached
signatures. These are not an authorization to update until the versioned
release has passed real-machine validation and has been published.

## macOS v0.1 policy

The v0.1 macOS artifact uses ad-hoc signing rather than Apple Developer ID
signing and is not notarized.

After downloading Chiù from GitHub Releases, mount the disk image, drag
`Chiù.app` to its `Applications` shortcut, then eject the disk image. macOS may
require one approval through **System Settings → Privacy & Security → Open
Anyway** when opening the installed application. Release documentation must use the normal
[Apple-supported flow](https://support.apple.com/en-gb/102445), never a command
that disables Gatekeeper or strips quarantine metadata.

## Updater releases

Chiù's updater keeps discovery separate from installation consent. Finding an
update never authorizes installation; each download and installation attempt
requires explicit confirmation in the active session.

An updater-enabled release requires signed updater artifacts, published
metadata, protected signing keys, and an exercised N-to-N+1 update on both
supported platforms.

After publishing a validated versioned release, run the **Promote updater
metadata** workflow with that version. It verifies that the public Windows and
macOS updater payloads and signatures are available, then updates the
prerelease-only metadata endpoint at
`releases/download/updater/latest.json`. The metadata references immutable
versioned assets, and maps the one universal macOS archive to both Intel and
Apple Silicon updater targets. The metadata asset can briefly be unavailable
while GitHub replaces it; update checks must treat that as a retryable failure,
never as a successful update.

Do not promote metadata before the release is public. The updater release is
only a pointer for installed applications; it is not a product release and
must remain a GitHub prerelease.

## Release validation

Before publication, validate artifact structure and architecture, installation,
first launch, tray/menu rendering, single-instance behavior, sleep prevention,
update behavior where configured, and clean process exit on supported systems.
See [Testing](testing.md) for the evidence boundary.
