#!/bin/sh
set -eu

version=${1:?usage: check.sh VERSION}
package_id=$(cargo pkgid)
cargo_version=${package_id##*#}
cargo_version=${cargo_version##*@}

if [ "$cargo_version" != "$version" ]; then
  echo "Cargo.toml is $cargo_version, but the release is $version" >&2
  exit 1
fi

if ! grep -Eq "^## ${version} — [0-9]{4}-[0-9]{2}-[0-9]{2}$" CHANGELOG.md; then
  echo "CHANGELOG.md needs a dated ${version} heading" >&2
  exit 1
fi

if grep -Eq "^## ${version} — unreleased$" CHANGELOG.md; then
  echo "CHANGELOG.md still marks ${version} unreleased" >&2
  exit 1
fi

grep -Fq 'cargo install deliveryboy' README.md || {
  echo "README.md does not show the Cargo install command" >&2
  exit 1
}

test -x scripts/release/build-local.sh
test -x scripts/release/publish-github.sh
test -f scripts/release/Dockerfile

command -v cargo >/dev/null
command -v git >/dev/null
command -v hugo >/dev/null
command -v curl >/dev/null
command -v docker >/dev/null
command -v gh >/dev/null
command -v rustup >/dev/null

docker info >/dev/null
gh auth status >/dev/null

echo "release metadata agrees on $version"
