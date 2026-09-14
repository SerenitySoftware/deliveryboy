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

## Secrets

Declare secret names in `.deliver.yml` and resolve values from the environment, a gitignored file, SOPS, 1Password, or the macOS Keychain. Delivery Boy hides resolved values in text and JSON plans, but a command you run can still print a secret. Keep logs private and test command output.

Use a release account with only the access that release needs. Avoid a root SSH account when the target can use a narrower account with specific `sudo` rights.

## Rollback limits

File and Hugo releases use complete release directories and a live symlink, so Delivery Boy can restore the prior directory. Docker Compose keeps a rollback image when one exists. Not every command can be reversed.

Database migrations, remote scripts, external API calls, and arbitrary commands may be permanent. Back up data before a migration and write compatible migrations that can run while the old and new app versions overlap.

## Current limits

- Preflight checks SSH access but does not yet prove that every remote tool exists.
- The live config diff covers nginx vhosts and Compose files; other deployers do not yet declare the long-lived files they replace.
- Rollback selects the previous retained release; choosing any older release is planned.
- Two release processes can still overlap on a target; target-side deploy locking is planned.
- Delivery Boy does not isolate commands from the local user or remote account that runs them.

Track these limits in the release plan and do not treat a successful preflight as permission to skip review.
