# Contributing to Chiù

Keep each contribution focused on one behavior or repository improvement, and
describe only validation that was actually performed.

## Before opening a pull request

- Discuss or link a relevant issue for non-trivial work.
- Target `main` from a focused branch.
- Follow the durable product language in [CONTEXT.md](CONTEXT.md) and the review
  considerations in [REVIEW.md](REVIEW.md).
- Keep Tauri and native operating-system integration at the application edge;
  product rules should remain deterministically testable.
- Do not include secrets, personal or machine-specific data, private prompts,
  generated junk, or unsanitized diagnostics.

The repository has no default pull-request template. Describe the result,
linked issue, evidence, platform coverage, and any remaining limitation.

## Titles and merge convention

Use a Conventional Commit-style pull-request title. Common prefixes include
`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `ci:`, and `chore:`. An optional
scope is welcome, for example `chore(deps): update Rust tooling`.

Chiù uses squash merge as a workflow convention. Write the pull-request title
so it can describe the resulting logical commit. Renovate pull requests follow
the same semantic title convention.

## Validation

Run the canonical Rust checks and follow the evidence guidance in
[Testing](docs/testing.md). Pull requests are expected to have green CI.

For platform-specific changes, record validation for every relevant supported
host. A compile check or inspection command is not a substitute for real native
behavior, and evidence from one operating system does not prove the other.

Contributor-facing ownership and seams are summarized in
[Architecture](docs/architecture.md). Release-sensitive changes should also
follow [Releasing](docs/releasing.md).

## Security and sensitive material

Suspected vulnerabilities and sensitive exploit details do not belong in an
ordinary issue or pull request. Do not include secrets, personal information,
or sensitive diagnostics in repository-visible discussion. See
[SECURITY.md](SECURITY.md) for reporting instructions.

## License and release notes

Contributions are made under the repository's [MIT license](LICENSE). Chiù does
not require a Contributor License Agreement or Developer Certificate of Origin.

Release notes are maintained through GitHub Releases. The project intentionally
does not maintain a handwritten `CHANGELOG.md`.
