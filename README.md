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

## Built-in deployers

- Hugo sites and prepared files
- Docker Compose applications
- nginx virtual hosts and TLS setup
- signed and notarized macOS applications
- explicit local and SSH commands

Delivery Boy uses the tools each release needs, such as Git, SSH, Hugo, Docker,
or Xcode. The `deliver` binary itself does not require a hosted service.

The full guide lives at [deliveryboy.app/docs](https://deliveryboy.app/docs/).

## License

MIT
