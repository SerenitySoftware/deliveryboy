---
title: "Commands"
description: "Delivery Boy CLI command reference."
weight: 4
---

## Global options

Pass `--config PATH` before or after a command to use a specific config instead of searching from the current directory:

```bash
deliver --config ops/production.yml plan
```

`deliver --version` prints the CLI version. `deliver --help` and `deliver COMMAND --help` show the installed command surface.

## `deliver init`

Inspect a repository, report supported project shapes, and prepare `.deliver.yml`.

```bash
deliver init
deliver init --write --host example.com --dir /var/www/example
```

It scaffolds every shape it can map to a deployer: a Hugo site, a built front-end, an nginx vhost, a Docker Compose project, and a macOS app. What it writes comes from what the repo says — the Compose file's own Postgres service and named volumes become the `backup:` block, an Xcode project becomes the `xcodebuild:` block, a `fastlane/Fastfile` becomes a lane — and `--host` fills in the URLs.

A front-end is anything with a `package.json` that declares a `build` script — Vite, Next.js, Astro, SvelteKit, Nuxt, Angular, Create React App, Vue CLI or Parcel — at the repo root or in `apps/web`, `web`, `frontend`, `client` or `ui`. It becomes a `files` service with a `build:` step: the install command follows whichever lockfile is actually present (`pnpm-lock.yaml` → `pnpm install --frozen-lockfile`, no lockfile at all → `npm install`, since `npm ci` needs one), and `src` is the framework's output directory — or, if the project has been built once already, whatever directory is really on disk.

Every service it writes also gets a `verify:` block — the only thing that can roll a bad release back on its own — derived from what `init` was told rather than from a placeholder: the site root on `--host`, the appcast URL it just wrote, `nginx -t`, or a container-up probe using the same compose files and project the deploy will use.

Anything it cannot work out is printed as a `!` note under the finding rather than guessed at. In particular no `env_file:` block is scaffolded: that block makes Delivery Boy *render* the env file from literals and resolved secrets, so an empty one would ship an empty `.env` over a working one. Add it yourself with `from_secrets:` once the secret names are declared. A front-end always gets the note that matters most for it: the build runs locally and bakes its variables into the bundle, so every `VITE_*` (or `NEXT_PUBLIC_*`, `REACT_APP_*` …) the build reads has to be declared in an `env:` block — a missing one does not fail the build, it silently ships the development default.

## `deliver validate`

Check the config schema, service references, deployer names, and target names.

## `deliver plan`

Compile and print every step without running it. Add `--json` for machine-readable output, `--service NAME` to select services, or `--version VERSION` to preview the exact release placeholders that a later deploy will use.

```bash
deliver plan --version 1.2.3
```

## `deliver preflight`

Check local tools, input files, secrets, and SSH access. It reports every problem it can find in one pass.

## `deliver deploy`

Run preflight, build, stage, activate, and verify. Use `--dry-run` to walk the flow without changing anything.

```bash
deliver deploy
deliver deploy --service web
deliver deploy --version 1.2.3
```

`--service NAME` can be repeated. `--version` supplies the release version without a prompt. `--yes` accepts a tag already present on `HEAD`; it does not invent an untagged release.

Before the first step that changes anything, `deploy` takes an advisory lock on each target and refuses if another run holds it (exit code `2`, nothing built or shipped). `--force` takes the lock anyway. A dry run takes no lock. See [Safety](../safety/).

## `deliver status`

Read back what is live on the target right now: the release the live symlink points at, when it was deployed and from which commit, how many releases are retained, and how many deploys are on record. Nothing is modified — it is one read per target.

```bash
deliver status
deliver status --service web --json
```

Services deployed with `files` or `hugo` answer from the release symlink; `docker-compose` services have no symlink, so they answer from the newest recorded deploy and say so. A target that cannot be read is reported as such and exits `1`, so a failed read never looks like "nothing is deployed".

## `deliver history`

List the deploys recorded on the target, newest first, marking the one that is live.

```bash
deliver history
deliver history --limit 0
```

`--limit N` shows the newest `N` per service (default 10); `--limit 0` shows every recorded deploy.

## `deliver logs`

Tail what the deploy is running, without an ssh session.

```bash
deliver logs
deliver logs --service api --follow
deliver logs --tail 200
```

`--tail N` sets how many existing lines to show first (default 50) and
`--follow` keeps the stream open. Following tails one service at a time: with
several services in range it lists them and asks you to narrow the run with
`--service NAME`, rather than interleaving two streams into something neither
of them said.

Where the log is comes from one of two places. A `docker-compose` service is
tailed with the deploy's own `docker compose` invocation — the same `-f` files
and `-p` project it brought the containers up with. Every other service has to
say, because the deploy does not know: a `files` or `hugo` release is served by
a web server Delivery Boy never configured, so guessing a path would be wrong
on half of the hosts this tool targets. Give those services a
[`logs:` block](../configuration/#where-the-logs-are); a service with neither
is reported as such and exits `2`.

The command it runs is printed before it runs, and the read is read-only.

## `deliver verify`

Run only the checks from the selected services.

## `deliver rollback`

Restore the previous release for services that support rollback.

```bash
deliver rollback
deliver rollback --to 20260202-1000-bbb2222
```

Without `--to`, each service goes one step back, to the release that was live
before the last deploy. `--to <deploy-id>` restores any release still retained
on the target instead, so you can skip past a release that was itself bad.
`deliver history` lists the ids.

The target is read before anything is changed. A deploy id that is no longer
retained is refused with the list of ids that are, an id that is already live
is left alone, and a deployer that keeps no release directories (`docker-compose`
replaces containers in place) is told to use plain `deliver rollback`. If any
selected service cannot be satisfied, nothing is changed anywhere — so a
multi-service rollback never half-lands. Narrow the run with `--service NAME`
when only one app should move.

## `deliver fleet`

Run one command across every repo listed in `deliver.fleet.yml`, instead of
`cd`-ing through them one at a time.

```bash
deliver fleet status
deliver fleet preflight
deliver fleet deploy
deliver fleet --repo conduit deploy
```

The fleet file is a list of repo paths, relative to the file itself (`~` works):

```yaml
version: 1
repos:
  - ../conduit
  - ../toothpick
  - ~/dev/ampersand
```

It is found by searching up from the current directory, so the command works
from the fleet directory or from inside any of its repos; `--fleet PATH` names
one explicitly. `--repo NAME` narrows the run and can be repeated, matching
either a repo's directory name or the path as written in the file — a selector
that matches nothing stops the run and lists what the file holds, rather than
quietly running a smaller fleet than you asked for.

Each repo is entered and the ordinary per-repo command runs there, exactly as
if you had typed it in that directory: it discovers that repo's own
`.deliver.yml`, resolves that repo's release, and runs relative commands
against that repo's files. `--service NAME` applies inside every repo.

`deliver fleet deploy` stops at the first repo that fails — that repo has
already unwound itself, and the ones after it are reported as `not attempted`.
Pass `--keep-going` to run them anyway. `fleet preflight` and `fleet status`
never stop early, because the whole answer is the point of asking. `--dry-run`
and `--yes` mean on a fleet deploy what they mean on a single one; without
`--yes` each repo asks about its own release in turn.

Every run ends with a summary naming each repo and what happened to it, and the
fleet exits with the worst code any repo returned — the same answer you would
get running them one at a time and keeping the worst.

```
▸ Fleet summary
    ✓ conduit    ok
    ✗ toothpick  failed (exit 1)
    - ampersand  not attempted
```

This is a loop, not a scheduler: no state is kept, nothing runs in parallel, and
there is no cross-repo dependency graph. It is the multi-app view of one host —
run history, schedules and approvals across a fleet are what
[Teams](../teams/) is for.

## `deliver secrets`

Show every declared secret and whether a configured provider can resolve it. Values are not printed.

## `deliver clean`

Remove Delivery Boy build artifacts from the system temporary directory.

## Exit codes

- `0` — the command completed successfully.
- `1` — a release step or live check failed.
- `2` — the config, command use, or release guard was invalid.
