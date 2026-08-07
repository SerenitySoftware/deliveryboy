#!/bin/sh
set -eu

package_id=$(cargo pkgid)
version=${package_id##*#}
version=${version##*@}
tag="v${version}"
url="https://crates.io/api/v1/crates/deliveryboy/${version}"
agent="deliveryboy-release/${version} (https://github.com/SerenitySoftware/deliveryboy)"

status=$(curl -sS -A "$agent" -o /dev/null -w '%{http_code}' "$url")
if [ "$status" != 200 ]; then
  echo "deliveryboy $version is not visible on crates.io (HTTP $status)" >&2
  exit 1
fi

is_draft=$(gh release view "$tag" --repo SerenitySoftware/deliveryboy --json isDraft --jq .isDraft)
if [ "$is_draft" != false ]; then
  echo "GitHub release $tag is still a draft" >&2
  exit 1
fi

asset_count=$(gh release view "$tag" --repo SerenitySoftware/deliveryboy --json assets --jq '.assets | length')
if [ "$asset_count" -lt 6 ]; then
  echo "GitHub release $tag has only $asset_count assets" >&2
  exit 1
fi

install_root=$(mktemp -d)
trap 'rm -rf "$install_root"' EXIT HUP INT TERM
release_root="$install_root/release"
mkdir "$release_root"
gh release download "$tag" --repo SerenitySoftware/deliveryboy --dir "$release_root"
(cd "$release_root" && shasum -a 256 -c SHA256SUMS)
for asset in "$release_root"/deliveryboy-*; do
  gh attestation verify "$asset" --repo SerenitySoftware/deliveryboy >/dev/null
done

cargo install deliveryboy --version "$version" --locked --root "$install_root"
"$install_root/bin/deliver" --version

echo "deliveryboy $version is published, attested, and installs deliver"
