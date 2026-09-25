# Changelog

## Unreleased

### Added

- A JSON Schema for `.deliver.yml`, so an editor catches a mistyped key while
  the file is being written instead of `deliver validate` catching it
  afterwards. `deliver schema` prints it; the docs site publishes it at
  `https://deliveryboy.app/schema/v1.json`; and `deliver init` starts the file
  it writes with the `# yaml-language-server: $schema=…` line that VS Code,
  Zed, Helix and Neovim's YAML servers read. The schema is exactly as strict as
  the loader on the file's own structure (targets, `ssh:`, services,
  `versioning:`, notifications, `logs:`), requires what each deployer's plan
  requires (`src` for `files`, `steps` for `commands`, `appcast.url` for
  `macos-app`), and describes the rest of each deployer's `config:` without
  refusing keys it does not list. Tests hold the schema to the loader in both
  directions: every config in the test suite, the README and the docs that
  loads must satisfy it, and a field added to the config types without the
  schema learning it fails.

- `deliver preflight` (and the preflight every `deploy` runs) checks the tools
  the target itself has to run, not only the local ones. A fresh or
  post-upgrade host used to pass preflight, build, ship, and then die at
  `docker compose up` or `nginx -t` with artifacts left behind. Each deployer
  now declares the remote binaries its steps call — `docker` and `docker
  compose` (probed as `docker compose version`, since the plugin can be
  missing where `docker` is not), `nginx` and `systemctl`, `tar`, `curl` for an
  on-target health check — and preflight asks for all of them in one ssh call
  per host, with the sbin directories on `PATH` so a deploy user finds what
  `sudo` would. Declared rather than parsed out of the shell: the certbot step
  installs certbot when it is absent, so reading its text would report a
  missing tool on exactly the host it is written to fix. Commands the operator
  wrote themselves are not inspected.

- `deliver deploy` shows the commit range it is about to ship. After preflight
  it reads the sha each target recorded for its live release — the same
  `.deliver/history.tsv` record `deliver status` reads — and lists
  `git log <live>..HEAD`, so the confirmation for a tag found on `HEAD` reads
  `Deploy release v1.2.3 (abc1234), shipping 7 commit(s) since live v1.2.2
  (def4567)?` instead of asking blind. A re-deploy of the live commit, a deploy
  that moves the target backwards and one that drops live commits from another
  branch are each named as such. The confirmation moved from before compiling
  to after preflight so it can carry the range; compile and preflight change
  nothing, so declining still leaves the target untouched. A dry run reads only
  `method: local` targets, matching preflight skipping remote reachability, and
  an unreadable target or a live commit the clone lacks prints a line rather
  than failing the deploy.

- `deliver deploy` and `deliver rollback` hold an advisory lock on each target
  for the length of the release. The model is an atomic symlink swap into
  `releases/<stamp>` on a single shared host, and nothing serialized two runs
  against it: a CI release and a hand-run deploy, two operators, or a `rollback`
  fired mid-swap interleave their swaps, restarts and health checks, and the box
  can end up running one release's artifact behind another's config with
  `verify:` passing against whichever won. A second run now refuses with the
  holder, services, release and how long ago it started. The lock is a `mkdir`
  beside the target directory — never underneath it, since for a service that
  serves the release root `dir` is the symlink the deploy is about to swap — and
  it is released in `Drop`, so the abort paths and the rollback unwind give it
  back without each caller remembering to. It is taken after the last prompt and
  immediately before the first mutating step, so it is never held while a human
  reads a diff. `--dry-run` and `deliver verify` change nothing and take no
  lock; `--force` takes a lock another run still holds; a lock older than
  `lock.stale_after` on the target (default one hour, `0` to disable) is treated
  as abandoned and taken over, because a Ctrl-C kills `deliver` outright. A lock
  that cannot be taken at all — no route to the host, an uncreatable directory —
  prints a line and the deploy continues: that is not evidence of a concurrent
  release, and failing closed on it would turn an unrelated permission problem
  into a failed deploy.

- `deliver init` scaffolds a `verify:` block for every deployer it writes, and
  `deliver plan` names the services that have none. A failed check fails the
  deploy and triggers the rollback `exec.rs` unwinds — the CLI's single best
  safety property — but it was opt-in YAML scaffolded for exactly two
  deployers, so a `docker-compose`, `files` or `macos-app` service written by
  `init` shipped blind and could never roll itself back. Every default is now
  derived from something `init` was actually told: the site root on `--host`
  for `hugo` and `files`, the appcast URL it just wrote for `macos-app`, and
  for `docker-compose` a container-up probe built from the same `-f` files and
  `-p` project the deployer will bring the project up with. What it still
  cannot know is a note under the finding rather than a guess inside the check.

- `deliver fleet preflight|deploy|status` runs one command across every repo in
  `deliver.fleet.yml`, which is a list of repo paths relative to itself. The
  founding case is ~8–10 apps on one host, and every command until now operated
  on one repo, so "deploy everything" or "what is live across the fleet?" meant
  N invocations. Each repo is *entered* and the existing per-repo command runs
  there — discovering that repo's own `.deliver.yml`, resolving that repo's
  release, and running relative commands against that repo's files — because a
  local `command:` step inherits the process's working directory, so anything
  less would silently run one repo's build in another repo's directory. The
  fleet file is found by walking up from the current directory (past a `.git`,
  unlike config discovery, since it lives above the repos it lists);
  `--repo NAME` narrows by directory name or the path as written, and a
  selector that matches nothing stops the run rather than quietly running a
  smaller fleet. `fleet deploy` stops at the first failure and reports the rest
  as `not attempted` (`--keep-going` runs them anyway); the reads never stop
  early. Every run ends with a per-repo summary and exits with the worst code
  any repo returned. A loop, not a scheduler: no state, no daemon, no
  cross-repo dependency graph.

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

### Fixed

- A `files` service whose `src:` is its `build:` output failed its first deploy
  from a clean checkout — the shape `deliver init` scaffolds for every
  front-end. Packaging was decided by testing `src` on disk when the plan was
  compiled, before the build that creates it had run, so the plan was a plain
  `scp` of a path that did not exist instead of the package/stage/activate
  release; and preflight reported the missing output as a missing input file.
  With `build:` configured, `src` is now treated as the directory the build
  will produce (unless it names an archive), and preflight no longer demands it
  up front.

- The `docker-compose` deployer always built an image, so a pull-only Compose
  project could not be deployed. The `docker build` step was pushed
  unconditionally while `image.tag` and `image.context` defaulted to
  `{app}:latest` and `.`, so a project whose services only pull published
  images — Postgres plus Redis plus a prebuilt app image, an ordinary shape on
  a shared box — got a `docker build --platform linux/amd64 -t <app>:latest .`
  it never asked for: it failed when there was no Dockerfile, and shipped a
  meaningless image when one happened to be lying around. The build, save, ship
  and load steps are now skipped for a project with nothing to build, leaving
  the file ship, backup, `up -d`, health and record steps — the whole deploy
  for a project like that. The evidence is deliberately conservative, and a
  build is skipped only when every Compose file parsed and none of them wanted
  one: an `image:`/`images:` block, a Compose service with a `build:`, or a
  Compose service naming the tag this deploy produces all mean build, and so
  does a file that cannot be read. `build: true` / `build: false` in the
  service config overrides all of it. `deliver init` now leaves the `image:`
  block out when it can see nothing to build, instead of scaffolding one and
  warning about it.

- `deliver preflight` stopped at plan compile, so its one-pass report was cut
  short. The command's whole promise is that it "reports every problem it can
  find in one pass", and `preflight::run` is built that way — it collects
  problems rather than returning at the first — but the plan was compiled
  *before* it with the error allowed to escape to the top level. A config with
  one unresolvable secret therefore exited 2 with no tools line, no input-file
  line and no ssh line, so an operator who was also missing `hugo` or could not
  reach the box found that out one run at a time. A service that will not
  compile is now a finding like any other: the services that do compile still
  contribute their tool checks, the failing service's host is still probed
  (derived from the config, since the plan is what broke), and everything is
  reported together. `plan`, `deploy` and `rollback` are unchanged — one broken
  service is still a hard error there, with the same message.

- The one `verify:` check `deliver init` did scaffold hardcoded
  `url: https://EXAMPLE/`, so an operator who did not edit it got a check that
  fails on a *good* release — worse than no check at all, because it teaches
  that a red verify means nothing. It is now the host `init` was given.

- `nginx-vhost` provisioned certificates from the **unrendered** conf. Cert
  needs were derived by reading the conf off disk, while the `render:`
  substitution happened separately further down, so a vhost whose `server_name`
  came from a placeholder had its certbot steps built from the placeholder
  text — `certbot certonly … -d __SITE_DOMAIN__`, plus a temporary ACME vhost
  with the same name. Let's Encrypt refuses that name, and failed
  authorizations are rate-limited, so a repeated deploy could lock the account
  out of issuing the real certificate. Cert needs are now derived from the
  rendered text the install step will actually put on the target; a conf with
  no `render:` block still ships byte-for-byte and is still read from disk.

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
