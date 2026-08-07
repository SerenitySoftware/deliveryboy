#!/bin/sh
set -eu

version=${1:?usage: publish-crate.sh VERSION}
url="https://crates.io/api/v1/crates/deliveryboy/${version}"
agent="deliveryboy-release/${version} (https://github.com/SerenitySoftware/deliveryboy)"
status=$(curl -sS -A "$agent" -o /dev/null -w '%{http_code}' "$url")

case "$status" in
  200)
    echo "deliveryboy $version is already on crates.io"
    ;;
  404)
    cargo publish --locked
    ;;
  *)
    echo "crates.io returned HTTP $status while checking deliveryboy $version" >&2
    exit 1
    ;;
esac
