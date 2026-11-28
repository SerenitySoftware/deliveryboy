#!/usr/bin/env bash
# Exercise the same formula on macOS and in the Homebrew Linux image.
set -euo pipefail
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_ANALYTICS=1 HOMEBREW_DEVELOPER=1
export HOMEBREW_NO_AUTOREMOVE=1

root=$(cd "$(dirname "$0")/../.." && pwd)
tap="local/deliveryboy-test-$$"
formula="$tap/deliveryboy"
tap_created=false
install_started=false
cleanup() {
  result=$?
  trap - EXIT
  set +e
  if "$install_started" && brew list --formula "$formula" >/dev/null 2>&1; then
    brew uninstall --formula "$formula" || result=1
  fi
  if "$tap_created"; then
    brew untap "$tap" || result=1
  fi
  exit "$result"
}
trap cleanup EXIT

if brew list --formula deliveryboy >/dev/null 2>&1; then
  echo "Refusing to replace an existing deliveryboy installation" >&2
  exit 1
fi
set -x
brew tap-new --no-git "$tap"
tap_created=true
cp "$root/packaging/homebrew/deliveryboy.rb" "$(brew --repository "$tap")/Formula/deliveryboy.rb"
install_started=true
brew install --formula "$formula"
brew test "$formula"
"$(brew --prefix "$formula")/bin/deliver" --version
brew audit --strict "$formula"
