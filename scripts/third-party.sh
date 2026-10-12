#!/usr/bin/env bash
# Rewrites the Rust table in THIRD-PARTY.md from the dependencies of the shipped binaries; --check fails if it is stale.
set -euo pipefail
cd "$(dirname "$0")/.."
rows=$(
  for t in "memburn x86_64-unknown-linux-musl" "memburn x86_64-pc-windows-gnu" \
    "memburn-init x86_64-unknown-linux-musl" "memburn-uefi x86_64-unknown-uefi"; do
    read -r pkg target <<< "$t"
    cargo tree -q --locked -e normal --prefix none --format '{p}|{l}' -p "$pkg" --target "$target"
  done | grep -v ' (/' | sed 's/ ([^)]*)//g; s/ v\([^|]*\)|/|\1|/' | sort -u |
    awk -F'|' '{printf "| %s | %s | %s |\n", $1, $2, $3}'
)
table=$(printf '| Crate | Version | Licence |\n|---|---|---|\n%s\n' "$rows")
new=$(awk -v t="$table" '/<!-- rust-begin -->/ {print; print t; skip = 1; next} /<!-- rust-end -->/ {skip = 0} !skip' THIRD-PARTY.md)
if [[ ${1:-} == --check ]]; then
  [[ $new == "$(cat THIRD-PARTY.md)" ]] || { echo "THIRD-PARTY.md is stale: run scripts/third-party.sh" >&2; exit 1; }
else
  printf '%s\n' "$new" > THIRD-PARTY.md
fi
