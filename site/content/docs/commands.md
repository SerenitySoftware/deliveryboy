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

It scaffolds every shape it can map to a deployer: a Hugo site, an nginx vhost, a Docker Compose project, and a macOS app. What it writes comes from what the repo says — the Compose file's own Postgres service and named volumes become the `backup:` block, an Xcode project becomes the `xcodebuild:` block, a `fastlane/Fastfile` becomes a lane — and `--host` fills in the URLs.

Anything it cannot work out is printed as a `!` note under the finding rather than guessed at. In particular no `env_file:` block is scaffolded: that block makes Delivery Boy *render* the env file from literals and resolved secrets, so an empty one would ship an empty `.env` over a working one. Add it yourself with `from_secrets:` once the secret names are declared.

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

## `deliver verify`

Run only the checks from the selected services.

## `deliver rollback`

Restore the previous release for services that support rollback.

## `deliver secrets`

Show every declared secret and whether a configured provider can resolve it. Values are not printed.

## `deliver clean`

Remove Delivery Boy build artifacts from the system temporary directory.

## Exit codes

- `0` — the command completed successfully.
- `1` — a release step or live check failed.
- `2` — the config, command use, or release guard was invalid.
