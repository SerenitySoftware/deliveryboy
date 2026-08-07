---
title: "Getting started"
description: "Install Delivery Boy and preview your first deployment."
weight: 2
---

Delivery Boy is a single `deliver` binary that runs on your machine and connects directly to your release targets. It does not need a Delivery Boy account, hosted runner, or control server. It finds `.deliver.yml` by walking up from your current directory, so every command works from anywhere inside a repository.

## Availability

Install the public beta from crates.io. The `deliveryboy` package provides the
`deliver` command:

```bash
cargo install deliveryboy
deliver --version
```

Cargo installs the command into its binary directory, normally `~/.cargo/bin`. Make sure that directory is on your `PATH`.

Versioned macOS, Linux, and experimental Windows archives are also available
from the GitHub release. Each release includes SHA-256 checksums and build
attestations. Homebrew and WinGet packages will follow.

Do not copy a binary from an untrusted source. Check its release checksum and
attestation before running it.

## Detect your project

From the repository you want to deploy:

```bash
deliver init
```

Delivery Boy detects supported project shapes and shows the config it would write. Detection is a starting point, not proof that the generated release matches your production setup. Review it, then run:

```bash
deliver init --write --host your-server.example.com --dir /var/www/your-app
```

## Preview the release

The plan command compiles the full release without running it:

```bash
deliver validate
deliver plan
deliver deploy --dry-run
```

`plan` prints the ordered steps. `--dry-run` walks every phase but makes no changes.

## Check the target

```bash
deliver preflight
```

Preflight checks the required local tools, input files, secrets, and SSH access. When it fails, the release has not built, uploaded, or changed anything.

Preflight confirms that it can reach the server. It does not yet check every tool used by later remote commands. Review the plan and confirm the target has tools such as Docker, nginx, or Certbot when your release uses them.

## Deploy

Commit your work, review `deliver plan`, then run:

```bash
deliver deploy
```

Delivery Boy asks which release version you are shipping when `HEAD` is not tagged and your config uses tags for release versions. It builds locally, stages the result, activates it, and runs the checks from your config.

## Roll back

For release-based file and site deploys:

```bash
deliver rollback
```

Delivery Boy points the live path at the prior complete release. A failed check during a deploy triggers the same rollback on its own.

Next: [write your configuration](/docs/configuration/), review the [command reference](/docs/commands/), or read the [safety guide](/docs/safety/).
