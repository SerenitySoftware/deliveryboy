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

## `deliver validate`

Check the config schema, service references, deployer names, and target names.

## `deliver plan`

Compile and print every step without running it. Add `--json` for machine-readable output or `--service NAME` to select services.

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
