# Homebrew tap preparation

`deliveryboy.rb` installs the `deliver` binary from the four macOS/Linux release
archives. The public `serenitysoftware/homebrew-tap` repository does not exist yet;
creating it needs Jordan's approval. No tap is published by these scripts.

After each GitHub release, regenerate the formula from its published SHA256SUMS:

```sh
task homebrew:update VERSION=0.4.0
```

Or use a checksum file downloaded from that release:

```sh
python3 scripts/release/update-homebrew.py v0.4.0 --sha256sums /path/to/SHA256SUMS
```

The generator requires all four release archives and rejects missing, malformed
or duplicate checksums before writing the formula. It uses only Python's standard
library (Python 3.9 or later). Checksums detect damaged downloads; they do not
authenticate a release's publisher.

Run the local checks (Homebrew on macOS; Docker for Linux):

```sh
task homebrew:test:macos
task homebrew:test:linux
```

Each check uses a temporary local tap, installs the release binary, checks
`deliver --version`, runs `brew audit --strict`, and uninstalls and untaps on
exit. The Linux container removes itself. The checks refuse to replace an existing
Delivery Boy Homebrew install. They need network access to GitHub and Homebrew.

Once Jordan approves and creates the public tap repository, copy
`deliveryboy.rb` into its `Formula/` directory and test the public install:

```sh
brew install serenitysoftware/tap/deliveryboy
```
