---
title: "Safety"
description: "Understand what Delivery Boy can change and how to review a release safely."
weight: 6
---

Delivery Boy is a release runner. A config can run local commands, read build inputs, upload files, connect over SSH, and run remote commands. Only run a config you trust.

## Review before execution

Use this order for a production release:

```bash
deliver validate
deliver plan
deliver secrets
deliver preflight
deliver deploy
```

`validate`, `plan`, and `secrets` do not execute release steps. `preflight` checks requirements and network access without building or uploading. `deploy` is the command that changes systems.

Read the plan for:

- the target host and remote directory;
- every selected service and its order;
- local commands and input paths;
- uploaded files and remote commands;
- the live switch and post-release checks.

## What the release changes on the target

`plan` shows the steps and `verify` proves the result. Neither answers the question that matters most on a shared host: what does this release change in the infrastructure config already running there?

Before it executes anything, `deliver deploy` reads the live nginx vhost and Docker Compose file from the target and prints a unified diff against what is about to replace them:

```
▸ Live config on the target
    stack → box.example.com:/var/universal/demo/docker-compose.yml
       ...
       4       environment:
       5 -       - LOG=info
         +       - LOG=debug
       6       ports:
       7 -       - "9090:80"
         +       - "8080:80"
```

A file that already matches says `no changes to live config`, and a file that is not on the target yet is reported as a new file. When anything does change, `deploy` asks before applying it; `--yes` and a run with no terminal skip the question, and `--dry-run` prints the diff without asking.

The read is read-only and never fails a release. If the file cannot be read — no route to the host, a path the release account cannot open — `deliver` says so on that line and carries on. Resolved secrets are elided from both sides of the diff, because the value already live on the target is the same secret as the one replacing it.

## One release at a time

The release model is an atomic symlink swap into `releases/<stamp>` on a single shared host, and two runs reaching that host together do not produce a clean loser: they interleave swaps, restarts and health checks, and the box can end up running one release's artifact behind another's config, with `verify:` passing against whichever won.

`deliver deploy` and `deliver rollback` therefore take an advisory lock on each target before the first step that changes anything, and give it back on every way out — including the rollback unwind after a failed step. A second run refuses and says who has it:

```
▸ Deploy lock
    ✗ production [box.example.com]:/var/universal/demo.deliver-lock — a deploy is already running
    ✗ holder:   jordan@laptop (pid 51234)
    ✗ services: web, nginx
    ✗ release:  v0.4.1 (20260202-bbb2222)
    ✗ started:  2026-02-02T14:02:11Z (7m ago)
```

The lock is a directory beside the target directory, so it is never carried away by the swap it is protecting. `--dry-run` and `deliver verify` change nothing and are not gated on it; `--force` takes a lock another run still holds.

A lock older than an hour is treated as abandoned and taken over, because a Ctrl-C kills `deliver` outright and a lock nobody can release would be worse than one that is briefly too generous. Set `lock: {stale_after: 7200}` on a target whose releases run longer, or `0` to require `--force` instead.

A lock that cannot be taken at all — no route to the host, a directory the release account cannot create — prints a line and the deploy continues. That is not evidence another release is running, and it is deliberately not treated as such.

## Secrets

Declare secret names in `.deliver.yml` and resolve values from the environment, a gitignored file, SOPS, 1Password, or the macOS Keychain. Delivery Boy hides resolved values in text and JSON plans, but a command you run can still print a secret. Keep logs private and test command output.

Use a release account with only the access that release needs. Avoid a root SSH account when the target can use a narrower account with specific `sudo` rights.

## Rollback limits

File and Hugo releases use complete release directories and a live symlink, so Delivery Boy can restore the prior directory. Docker Compose keeps a rollback image when one exists. Not every command can be reversed.

Database migrations, remote scripts, external API calls, and arbitrary commands may be permanent. Back up data before a migration and write compatible migrations that can run while the old and new app versions overlap.

## Current limits

- Preflight checks the remote tools Delivery Boy's own deployers run (Docker and `docker compose`, nginx and `systemctl`, `tar`, `curl` for an on-target health check). Commands you write yourself — `ssh:` steps, `script:`, a Compose `remote_command` — are not inspected.
- The live config diff covers nginx vhosts and Compose files; other deployers do not yet declare the long-lived files they replace.
- Rollback selects the previous retained release; choosing any older release is planned.
- Delivery Boy does not isolate commands from the local user or remote account that runs them.

Track these limits in the release plan and do not treat a successful preflight as permission to skip review.
