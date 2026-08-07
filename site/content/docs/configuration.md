---
title: "Configuration"
description: "Define targets, services, secrets, checks, and release rules in .deliver.yml."
weight: 3
---

Delivery Boy keeps the release contract in `.deliver.yml` at the root of your repository.

Treat this file as executable release code. Review changes to it with the same care as a shell script: deployers can run local commands, upload files, and run commands on remote hosts.

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
```

Tags are created only after the release and its checks succeed.

Use `versioning.require_clean` and `versioning.require_pushed` for production releases. They keep a local edit or an unpushed commit from becoming a release no one else can reproduce.
