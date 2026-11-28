#!/bin/sh
set -eu

version=${1:?usage: check-artifacts.sh VERSION DIST_DIR}
dist=${2:?usage: check-artifacts.sh VERSION DIST_DIR}

for suffix in \
  aarch64-apple-darwin.tar.gz \
  x86_64-apple-darwin.tar.gz \
  aarch64-unknown-linux-gnu.tar.gz \
  x86_64-unknown-linux-gnu.tar.gz \
  x86_64-pc-windows-gnu.zip
do
  asset="deliveryboy-$version-$suffix"
  test -s "$dist/$asset"
  # A valid checksum file must cover every required archive.
  awk -v name="$asset" '$2 == name { found=1 } END { exit !found }' "$dist/SHA256SUMS"
done
(cd "$dist" && shasum -a 256 -c SHA256SUMS)
unzip -Z1 "$dist/deliveryboy-$version-x86_64-pc-windows-gnu.zip" | grep -Fx deliver.exe
