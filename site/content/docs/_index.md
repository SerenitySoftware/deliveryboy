---
title: "Documentation"
description: "Install Delivery Boy, describe your release in .deliver.yml, preview the plan, and ship it."
weight: 1
---

Delivery Boy reads a `.deliver.yml` from your repository and turns it into an ordered release plan. The CLI works on its own: there is no account, daemon, or hosted service to install.

Start with [Getting started](/docs/getting-started/), then use the configuration and deployer guides as your release grows.

## Start here

- [Getting started](/docs/getting-started/) — install the CLI and preview your first release.
- [Configuration](/docs/configuration/) — targets, services, secrets, checks, and version rules.
- [Commands](/docs/commands/) — what each CLI command does and when to use it.
- [Deployers](/docs/deployers/) — built-in release types for sites, files, Compose apps, nginx, macOS, and custom commands.
- [Safety](/docs/safety/) — trust boundaries, secrets, remote access, and rollback limits.
- [Teams](/docs/teams/) — the planned paid service and what remains local.

## How Delivery Boy approaches a release

Delivery Boy runs quick checks before expensive work and delays live changes until the end. It checks local tools, input files, secrets, and server access before it builds or uploads anything. A file release goes to a new directory and becomes live through one link swap. Live checks run after that switch; a failure restores the prior release.
