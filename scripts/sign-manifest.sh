#!/usr/bin/env bash
# Prints the signed release manifest (the body of PUT /memburn/api/latest) for a stick-files tar.
# Usage: sign-manifest.sh <version> <stick-files.tar> <tar-url> <key.pem> [release files...]
set -euo pipefail
[[ $# -ge 4 ]] || { echo "usage: sign-manifest.sh <version> <stick-files.tar> <tar-url> <key.pem> [files...]" >&2; exit 2; }
ver=$1 tar=$2 url=$3 key=$4; shift 4
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
files=$(tar -tf "$tar" | grep -v '/$' | while read -r p; do
  tar -xOf "$tar" "$p" > "$work/f"
  jq -n --arg path "$p" --argjson size "$(stat -c %s "$work/f")" --arg sha256 "$(sha256sum "$work/f" | cut -d' ' -f1)" \
    '{path: $path, size: $size, sha256: $sha256}'
done | jq -s .)
assets=$(for f in "$@"; do jq -n --arg name "$(basename "$f")" --argjson size "$(stat -c %s "$f")" '{name: $name, size: $size}'; done | jq -s .)
jq -cn --arg version "$ver" --arg url "$url" --argjson size "$(stat -c %s "$tar")" \
  --arg sha256 "$(sha256sum "$tar" | cut -d' ' -f1)" --argjson files "$files" --argjson assets "$assets" \
  '{version: $version, tar: {url: $url, size: $size, sha256: $sha256}, files: $files, assets: $assets}' |
  tr -d '\n' > "$work/manifest.json"
openssl pkeyutl -sign -inkey "$key" -rawin -in "$work/manifest.json" -out "$work/sig"
jq -cn --rawfile manifest "$work/manifest.json" --arg sig "$(od -An -tx1 "$work/sig" | tr -d ' \n')" \
  '{manifest: $manifest, sig: $sig}'
