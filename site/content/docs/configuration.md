---
title: "Configuration"
description: "Define targets, services, secrets, checks, and release rules in .deliver.yml."
weight: 3
---

Delivery Boy keeps the release contract in `.deliver.yml` at the root of your repository.

Treat this file as executable release code. Review changes to it with the same care as a shell script: deployers can run local commands, upload files, and run commands on remote hosts.

## Editor support

A JSON Schema for the file is published at `https://deliveryboy.app/schema/v1.json`, and `deliver schema` prints the same schema offline. `deliver init` starts the file it writes with the modeline that YAML language servers read (the VS Code YAML extension, Zed, Helix, Neovim with `yamlls`):

```yaml
# yaml-language-server: $schema=https://deliveryboy.app/schema/v1.json
```

That gives completion, inline descriptions and an underline on a mistyped key while you write the file. Add the line to an existing config to get the same. The schema is as strict as `deliver validate` on the file's own structure — an unknown key under a target, service or `versioning:` is an error in both — and describes each deployer's `config:` keys without refusing ones it does not list. `deliver validate` is still the final check.

## A small Hugo site

```yaml
version: 1
app: example-site

defaults:
  target: production

targets:
  production:
    hosts: [example.com]
    dir: /var/www/example
    ssh:
      user: deploy
      key: ~/.ssh/example.pem

services:
  web:
    deployer: hugo
    config:
      source: .
      minify: true
      remote_subdir: web
      owner: www-data:www-data
    verify:
      - http:
          url: https://example.com/
          expect_status: 200
```

## Targets

A target describes where a release goes. A target can name more than one host. SSH defaults to port 22 and supports a key or the running SSH agent.

```yaml
targets:
  production:
    hosts: [app-1.example.com, app-2.example.com]
    dir: /var/www/example
    ssh:
      user: deploy
      port: 22
      key: ~/.ssh/example.pem
```

A target is also the unit `deliver deploy` and `deliver rollback` lock, so two runs never change it at once. An abandoned lock is taken over after an hour; `lock: stale_after` sets that window in seconds, and `0` means a lock is never taken over automatically.

```yaml
targets:
  production:
    host: app-1.example.com
    dir: /var/www/example
    lock:
      stale_after: 7200
```

## Services and order

Each service uses one deployer. Use `needs` to order related services:

```yaml
services:
  web:
    deployer: hugo
    config: {source: apps/site, remote_subdir: web}

  nginx:
    deployer: nginx-vhost
    needs: [web]
    config:
      conf: nginx/example.conf
```

Run only part of a release with `--service`:

```bash
deliver plan --service web
deliver deploy --service web
```

## Secrets

Declare names in the config and resolve values from the environment, a gitignored file, the macOS Keychain, 1Password, or SOPS. Plans show names and hidden placeholders, never values.

```yaml
secrets:
  providers:
    - env
    - file: .env.deploy
    - keychain: {prefix: "example-"}
  define:
    DATABASE_PASSWORD: {}
    OPTIONAL_TOKEN: {required: false}
```

Check them without deploying:

```bash
deliver secrets
```

Keep secret values out of `.deliver.yml`. Use a provider and commit only the secret names. `deliver plan --json` also hides resolved values.

## Checks

Checks run after activation. A failed check fails the release and rolls back reversible steps.

```yaml
verify:
  - remote_file: web/index.html
  - contains:
      remote_file: web/index.html
      text: Example
  - http:
      url: https://example.com/health
      expect_status: 200
      retries: 5
      interval: 5
```

A check is the only thing that can roll a bad release back on its own, so
`deliver init` scaffolds one for every deployer it writes, built from what it
was actually told: the site root on the `--host` it was given for `hugo` and
`files`, the appcast URL it just wrote for `macos-app`, `nginx -t` for
`nginx-vhost`, and for `docker-compose` a container-up probe using the same
`-f` files and `-p` project the deploy will bring the project up with. What it
still cannot know — whether the site is served from `/`, whether the deploy
user can reach `docker` without `sudo` — is printed as a note under the
finding rather than guessed at inside the check, because a check that fails on
a *good* release is worse than no check at all.

Services that end up with none are named by `deliver plan`, so shipping blind
is a decision rather than an oversight:

```
▸ Compiling plan
    2 service(s), 7 step(s): migrate → web
  no verification step: migrate — a failed release there cannot roll itself back.
```

## Where the logs are

`deliver logs` tails a `docker-compose` service with the deploy's own compose
invocation. Any other service says where to look, with exactly one of `unit:`,
`files:` or `command:`:

```yaml
services:
  api:
    deployer: commands
    logs:
      unit: example-api        # journalctl -u example-api
  site:
    deployer: hugo
    logs:
      files:                   # tail -F
        - /var/log/nginx/example.com.access.log
        - /var/log/nginx/example.com.error.log
  worker:
    deployer: commands
    logs:
      command: "docker logs {follow} --tail {tail} worker"
```

`command:` runs verbatim on the target: `{tail}` becomes the line count and
`{follow}` becomes `-f` when `--follow` is given. `unit:` and `files:` are read
with the target's own `sudo`, because that is how the deploy wrote them;
`command:` is not, on the grounds that whoever wrote the command wrote all of
it.

A `logs:` block wins over what the deployer declares, so a Compose project
fronted by nginx can be pointed at the access log instead.

## Version rules

Projects can refuse releases from the wrong branch, a dirty tree, or a commit that does not match its upstream:

```yaml
versioning:
  require_clean: true
  require_pushed: true
  branch: main
  tag:
    enabled: true
    name: "v{version}"
    push: true
  after_tag:
    - command: ./scripts/publish-release.sh {version} {work}
```

Tags are created only after the release and its checks succeed.
`after_tag` commands run only after the tag exists and a configured tag push
succeeds. They can publish artifacts that earlier steps placed in `{work}`.
They run only for a full deploy, not one selected with `--service`. Make these
commands safe to retry: the tag and any earlier public step may already exist.

Use `versioning.require_clean` and `versioning.require_pushed` for production releases. They keep a local edit or an unpushed commit from becoming a release no one else can reproduce.
