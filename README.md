# Delivery Boy

Go ship yourself.

Delivery Boy is a local-first release CLI. It reads `.deliver.yml`, prints the
exact release plan, checks tools and access, then runs the release from your
machine. No Delivery Boy account, daemon, or hosted runner is required.

## Status

The CLI is in public beta. Releases include macOS and Linux binaries built and
tested on the release operator's machine. Windows support still needs native
command execution and a local Windows test host.

## Install

Install the `deliveryboy` package to get the `deliver` command:

```bash
cargo install deliveryboy
deliver --version
```

You can also install from a source checkout:

```bash
git clone https://github.com/SerenitySoftware/deliveryboy.git
cd deliveryboy
cargo install --path .
deliver --version
```

Versioned macOS and Linux archives with SHA-256 checksums are attached to GitHub
releases. Homebrew and WinGet packages will follow. Every install method
provides the `deliver` command.

## Start a release

From the repository you want to ship:

```bash
deliver init
deliver validate
deliver plan
deliver preflight
deliver deploy
```

`deliver init` prints the config it would create. Review it before passing
`--write`. Treat `.deliver.yml` as release code: it can run local commands,
upload files, and run commands on remote hosts.

## What is live right now

Every release-based deploy writes its own record on the target — the live path
is a symlink named after the deploy id, retained releases sit beside it, and
`.deliver/history.tsv` gets a row per deploy. `deliver status` and
`deliver history` read that back, so "what version is on prod?" does not mean an
ssh session and a `readlink`.

```
▸ web  → production (box.example.com)
    live          v0.2.0 · 20260202-1000-bbb2222
    deployed      2026-02-02T10:00:00Z · sha bbb2222dea
    path          /var/universal/demo/web → /var/universal/demo/releases/20260202-1000-bbb2222
    retained      5 release(s)
    history       12 deploy(s) recorded
```

Both are read-only and take one connection per target. `--json` gives the same
answer for scripts; a target that cannot be read is reported and exits `1`,
never rendered as "nothing is deployed".

`deliver logs` finishes that loop — it tails a Compose service with the deploy's
own `docker compose` invocation, and any other service with the `logs:` block
that says where its logs are.

## Every app on the box, one command

The founding case is a handful of apps sharing one host, so `deliver fleet`
runs the per-repo command across all of them. `deliver.fleet.yml` is a list of
repo paths; each repo is entered and run exactly as if you had `cd`-ed into it,
with its own config, its own release and its own relative paths.

```yaml
version: 1
repos:
  - ../conduit
  - ../toothpick
```

```
$ deliver fleet status
▸ Fleet summary
    ✓ conduit    ok
    ✓ toothpick  ok
```

`fleet deploy` stops at the first repo that fails (`--keep-going` runs the
rest), `fleet preflight` and `fleet status` always report on every repo, and
the run exits with the worst code any repo returned. It is a loop, not a
scheduler — no state, no daemon, no cross-repo graph.

## What a release changes on the target

Before `deliver deploy` executes anything, it reads the nginx vhost and Docker
Compose file that are live on the target and prints a unified diff against what
is about to replace them, then asks before applying it. Files that already match
say `no changes to live config`. The read is read-only and never fails a
release, and resolved secrets are elided from both sides of the diff.

```
▸ Live config on the target
    stack → box.example.com:/var/universal/demo/docker-compose.yml
       4       environment:
       5 -       - LOG=info
         +       - LOG=debug
```

## One release at a time

`deliver deploy` and `deliver rollback` take an advisory lock on each target
before the first step that changes anything, and give it back on every way out —
including the rollback unwind after a failed step. A second run refuses and names
who holds it, since two releases interleaving on one host can leave the box
running one release's artifact behind another's config.

```
▸ Deploy lock
    ✗ production [box.example.com]:/var/universal/demo.deliver-lock — a deploy is already running
    ✗ holder:   jordan@laptop (pid 51234)
    ✗ started:  2026-02-02T14:02:11Z (7m ago)
```

A dry run takes no lock, `--force` takes one another run still holds, and a lock
older than an hour is treated as abandoned and taken over.

## Secrets in output

Values resolved from your provider chain (environment, dotenv, SOPS, Keychain,
1Password) are recorded for the length of the run and elided from everything
`deliver` prints — the human plan, `deliver plan --json`, step labels during a
deploy, and error messages — as `[redacted:NAME]`. The name is kept so the line
still tells you what was there.

Values shorter than six characters are not scrubbed: replacing them would
corrupt unrelated output while protecting a value with no entropy to protect.

```
$ deliver plan
   1. [command] build (.)  — export VITE_ANALYTICS_KEY='[redacted:BUILD_TOKEN]'; npm run build
```

## Built-in deployers

- Hugo sites and prepared files
- Docker Compose applications
- nginx virtual hosts and TLS setup
- signed and notarized macOS applications
- explicit local and SSH commands

`deliver init` scaffolds a config for every one of these it finds, reading the
repo for the details — the Compose file's Postgres service and named volumes
become the backup block, an Xcode project or fastlane lane becomes the macOS
build strategy. What it cannot work out it prints as a note instead of guessing.

Delivery Boy uses the tools each release needs, such as Git, SSH, Hugo, Docker,
or Xcode. The `deliver` binary itself does not require a hosted service.

The full guide lives at [deliveryboy.app/docs](https://deliveryboy.app/docs/).

## License

MIT
