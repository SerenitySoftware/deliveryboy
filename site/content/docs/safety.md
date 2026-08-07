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

## Secrets

Declare secret names in `.deliver.yml` and resolve values from the environment, a gitignored file, SOPS, 1Password, or the macOS Keychain. Delivery Boy hides resolved values in text and JSON plans, but a command you run can still print a secret. Keep logs private and test command output.

Use a release account with only the access that release needs. Avoid a root SSH account when the target can use a narrower account with specific `sudo` rights.

## Rollback limits

File and Hugo releases use complete release directories and a live symlink, so Delivery Boy can restore the prior directory. Docker Compose keeps a rollback image when one exists. Not every command can be reversed.

Database migrations, remote scripts, external API calls, and arbitrary commands may be permanent. Back up data before a migration and write compatible migrations that can run while the old and new app versions overlap.

## Current limits

- Preflight checks SSH access but does not yet prove that every remote tool exists.
- Rollback selects the previous retained release; choosing any older release is planned.
- Two release processes can still overlap on a target; target-side deploy locking is planned.
- Delivery Boy does not isolate commands from the local user or remote account that runs them.

Track these limits in the release plan and do not treat a successful preflight as permission to skip review.
