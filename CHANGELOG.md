# Changelog

## Unreleased

### Added

- `deliver logs [--service S] [--follow] [--tail N]` tails what the deploy is
  running, closing the operational loop next to `status`/`history`: deploy →
  see what is live → watch it run, without an ssh session. A `docker-compose`
  service is tailed with the deploy's own compose invocation — the same `-f`
  files and `-p` project it brought the containers up with, declared by the
  deployer on the step that records the deploy, so the tail cannot drift from
  what was started. Every other service says where to look with a `logs:`
  block (`unit:` for a systemd unit, `files:` for log files, `command:` for
  anything else), because a `files` or `hugo` release is served by a web server
  Delivery Boy never configured and a guessed path would be wrong on half of
  the hosts this tool targets; a service with neither is reported and exits
  `2`. A `logs:` block wins over the deployer's own answer. Following tails one
  service at a time rather than interleaving two streams. Read-only, and the
  command is printed before it runs.

## 0.2.0 — 2026-09-16

### Added

- `deliver init` detects a built front-end and scaffolds the `files` deployer
  with its `build:` step. `files` has supported `build`/`build_dir`/`env`
  feeding the atomic release path all along, but nothing detected a JavaScript
  project, so a whole common app class dead-ended at "No known deploy strategy
  detected". Vite, Next.js, Astro, SvelteKit, Nuxt, Angular, Create React App,
  Vue CLI and Parcel are recognised at the repo root or in `apps/web`, `web`,
  `frontend`, `client` or `ui`. The install command follows whichever lockfile
  is present, the output directory is the framework's default unless the repo
  has been built and shows a real one, and the scaffold says plainly that
  build-time variables are baked into the bundle and need an `env:` block.
- `deliver rollback --to <deploy-id>` restores any release still retained on the
  target, not just the one step back `.deliver-previous` records — so a release
  that was itself bad can be skipped past. The ids are the ones `deliver history`
  prints. Every selected service is resolved against the target before anything
  is swapped: an id that is not retained is refused with the list of ids that
  are, an id already live is left alone, and if any service cannot be satisfied
  nothing is changed anywhere. A targeted swap rewrites `.deliver-previous`, so a
  plain `deliver rollback` afterwards steps back to where it came from.
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
