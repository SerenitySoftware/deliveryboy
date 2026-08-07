# Delivery Boy

Go ship yourself.

Delivery Boy is a local-first release CLI. It reads `.deliver.yml`, prints the
exact release plan, checks tools and access, then runs the release from your
machine. No Delivery Boy account, daemon, or hosted runner is required.

## Status

The CLI is preparing for its first public beta. macOS and Linux are the current
development hosts. Native Windows support is a release gate, not a completed
claim.

## Install from source

```bash
git clone https://github.com/SerenitySoftware/deliveryboy.git
cd deliveryboy
cargo install --path .
deliver --version
```

Versioned macOS, Linux, and Windows binaries, Homebrew, WinGet, and
`cargo install deliver` are planned for the public beta.

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
