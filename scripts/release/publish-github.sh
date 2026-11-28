#!/bin/sh
set -eu

version=${1:?usage: publish-github.sh VERSION WORK_DIR}
work=${2:?usage: publish-github.sh VERSION WORK_DIR}
tag="v$version"
repo=SerenitySoftware/deliveryboy
dist="$work/release"
notes="$work/release-notes.md"

case "$version" in
  *[!0-9A-Za-z.+-]*)
    echo "unsafe release version: $version" >&2
    exit 1
    ;;
esac

command -v gh >/dev/null
gh auth status >/dev/null
git rev-parse -q --verify "refs/tags/$tag" >/dev/null
test "$(git rev-list -n 1 "$tag")" = "$(git rev-parse HEAD)"
scripts/release/check-artifacts.sh "$version" "$dist"

awk -v version="$version" '
  $0 ~ "^## " version " — " { found=1; next }
  found && /^## / { exit }
  found { print }
' CHANGELOG.md > "$notes"
test -s "$notes"

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  gh release upload "$tag" "$dist"/* --repo "$repo" --clobber
else
  gh release create "$tag" "$dist"/* \
    --repo "$repo" \
    --verify-tag \
    --title "Delivery Boy $version" \
    --notes-file "$notes"
fi

echo "GitHub release $tag contains the local build files"
