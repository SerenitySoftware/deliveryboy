---
title: "Deployers"
description: "Built-in release types and when to use them."
weight: 5
---

A deployer turns one service block into ordered steps. Deployers do not hide the commands: `deliver plan` shows the result before it runs.

## `hugo`

Build a Hugo site, package its output, and pass the release to the file deployer. Releases use new directories and an atomic live-path switch.

## `files`

Ship a file, directory, or prepared archive. Use it for static output and other complete artifacts that do not need a container.

## `docker-compose`

Build an image locally, ship it as a tarball or through a registry, preserve the Compose files byte-for-byte, start infrastructure, run release commands, and check the live service.

## `nginx-vhost`

Install nginx configuration safely. The managed mode can prepare Certbot, issue or expand certificates, stage the vhost, run `nginx -t`, reload, and restore the prior configuration if validation fails.

## `macos-app`

Build, sign, notarize, and publish a macOS app and Sparkle appcast. Delivery Boy calls the platform tools instead of replacing them.

This deployer needs the Apple signing and notarization tools on a macOS release host. Review its plan for the archive, appcast, and publish paths before the first live release.

## `commands`

Run explicit local and SSH steps when a built-in deployer does not cover a small project-specific need.

```yaml
services:
  warm-cache:
    deployer: commands
    config:
      steps:
        - command: ./scripts/build-cache.sh
        - ssh: cd /var/www/example && ./bin/warm-cache
```

You can also place `pre` and `post` command steps around another deployer.

Command steps run with the rights of the local user or configured SSH user. Use fixed commands from the repository; do not build command strings from untrusted input.
