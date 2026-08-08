#!/bin/sh
set -eu

version=${1:?usage: build-local.sh VERSION WORK_DIR}
work=${2:?usage: build-local.sh VERSION WORK_DIR}
dist="$work/release"

case "$version" in
  *[!0-9A-Za-z.+-]*)
    echo "unsafe release version: $version" >&2
    exit 1
    ;;
esac

mkdir -p "$dist"
if find "$dist" -mindepth 1 -print -quit | grep -q .; then
  echo "release output already exists: $dist" >&2
  exit 1
fi

command -v cargo >/dev/null
command -v docker >/dev/null
command -v rustup >/dev/null
docker info >/dev/null

release_toolchain=1.94.0
rustup toolchain install --profile minimal "$release_toolchain"
rustup target add --toolchain "$release_toolchain" aarch64-apple-darwin x86_64-apple-darwin
release_cargo=$(rustup which --toolchain "$release_toolchain" cargo)
release_rustc=$(rustup which --toolchain "$release_toolchain" rustc)

echo "building macOS Apple Silicon"
RUSTC="$release_rustc" "$release_cargo" build --release --locked --target aarch64-apple-darwin
target/aarch64-apple-darwin/release/deliver --version

echo "building macOS Intel"
RUSTC="$release_rustc" "$release_cargo" build --release --locked --target x86_64-apple-darwin
arch -x86_64 target/x86_64-apple-darwin/release/deliver --version

for spec in \
  "linux/arm64:aarch64-unknown-linux-gnu" \
  "linux/amd64:x86_64-unknown-linux-gnu"
do
  platform=${spec%%:*}
  target=${spec#*:}
  output="$work/docker-$target"
  echo "building $target in local Docker"
  docker buildx build \
    --platform "$platform" \
    --file scripts/release/Dockerfile \
    --progress plain \
    --output "type=local,dest=$output" \
    .
  test -x "$output/deliver"
  docker run --rm --platform "$platform" \
    --mount "type=bind,source=$output/deliver,target=/deliver,readonly" \
    debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 \
    /deliver --version
done

package_archive() {
  target=$1
  binary=$2
  name="deliveryboy-$version-$target"
  stage="$work/package-$target"
  mkdir -p "$stage/$name"
  cp "$binary" "$stage/$name/deliver"
  cp README.md LICENSE "$stage/$name/"
  tar -C "$stage" -czf "$dist/$name.tar.gz" "$name"
}

package_archive aarch64-apple-darwin target/aarch64-apple-darwin/release/deliver
package_archive x86_64-apple-darwin target/x86_64-apple-darwin/release/deliver
package_archive aarch64-unknown-linux-gnu "$work/docker-aarch64-unknown-linux-gnu/deliver"
package_archive x86_64-unknown-linux-gnu "$work/docker-x86_64-unknown-linux-gnu/deliver"

(cd "$dist" && shasum -a 256 deliveryboy-*.tar.gz > SHA256SUMS)
(cd "$dist" && shasum -a 256 -c SHA256SUMS)

echo "local release files are ready in $dist"
