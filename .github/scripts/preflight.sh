#!/usr/bin/env bash
# Release preflight: refuses a tag that does not match Cargo.toml and
# package.json, or whose CHANGELOG section is missing or empty, BEFORE anything
# is built or published. Usage: preflight.sh <tag> [notes-output-file]
set -euo pipefail

tag="${1:?usage: preflight.sh <tag> [notes-file]}"
notes="${2:-notes.md}"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
npm_version=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' package.json | head -n 1)

if [ "$tag" != "v$version" ]; then
  echo "Tag $tag does not match the version in Cargo.toml ($version)" >&2
  exit 1
fi
if [ "$npm_version" != "$version" ]; then
  echo "package.json is at $npm_version, Cargo.toml at $version" >&2
  exit 1
fi

# The release notes are this version's section of CHANGELOG.md.
awk -v heading="## [$version]" '
  index($0, heading) == 1 { found = 1; next }
  found && /^## \[/ { exit }
  found { print }
' CHANGELOG.md > "$notes"
if ! grep -q '[^[:space:]]' "$notes"; then
  echo "CHANGELOG.md has no section for $version" >&2
  exit 1
fi
echo "Preflight OK for $tag"
