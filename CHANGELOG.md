# Changelog

## Unreleased

### Added

- `deliver init` scaffolds the `docker-compose` and `macos-app` deployers. Both
  shapes were detected before but emitted no deployer, so a Compose-only repo
  dead-ended at "No known deploy strategy detected". The Compose `backup:` block
  is read out of the Compose file (its Postgres service, declared user and
  database, named volumes) and the macOS stub from the Xcode project or fastlane
  lane, with appcast URLs filled from `--host`. What `init` cannot work out is
  printed as a note rather than guessed at.
- `deliver status` and `deliver history` read back what is deployed: the release
  the live symlink points at, when it landed and from which commit, the retained
  releases, and the deploys recorded in `.deliver/history.tsv`. Read-only, one
  connection per target, `--json` for scripts. A target that cannot be read
  exits `1` rather than reporting an empty deployment.
- `deliver deploy` diffs the nginx vhost and Docker Compose file it is about to
  install against the ones running on the target, and asks before applying a
  change. The read is read-only and never fails a release; secrets are elided
  from both sides.

### Changed

- Secret values resolved from the provider chain are now redacted from every
  console path — `plan`, `plan --json`, step labels, rollback messages and
  error chains — as `[redacted:NAME]`.

## 0.1.0 — 2026-08-07

First public beta of the standalone `deliver` CLI.

### Added

- Config discovery and project detection through `deliver init`.
- Read-only release plans and JSON plan output.
- Local, secret, file, tool, and SSH preflight checks.
- Hugo, files, Docker Compose, nginx, macOS app, and custom command deployers.
- Ordered multi-service releases with per-service selection.
- Live checks and automatic rollback for reversible release steps.
- Clean-tree, branch, upstream, version, tag, and post-release notification
  controls.
- Environment, dotenv, SOPS, macOS Keychain, and 1Password secret sources.
- Self-hosted CLI releases through `.deliver.yml`, crates.io, and local macOS
  and Linux release builds uploaded to GitHub after tagging.
- Release-specific previews through `deliver plan --version`.
- Retriable `versioning.after_tag` commands for work that needs the final tag.
