# Changelog

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
