#!/usr/bin/env bash
# Boots target/usb/memburn-<version>-usb.img headless with a 30 s quick check that uploads and powers off, then
# checks the stick's memburn.log for a PASS and a live-view URL. Usage: stick-test.sh [extra cfg lines...]
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
printf '%s\r\n' mode=quick seconds=30 upload=1 shutdown=1 "$@" > "$tmp/memburn.cfg"
timeout "${STICK_TIMEOUT:-600}" scripts/run-usb-qemu.sh "$tmp/memburn.cfg" -display none ||
  { echo "stick-test: QEMU did not power off within ${STICK_TIMEOUT:-600} s" >&2; exit 1; }
mtype -i target/usb/test.img@@1M ::/memburn.log > "$tmp/memburn.log"
grep -a -E '^Live view: https://|^Result:' "$tmp/memburn.log"
grep -aq '^Result: PASS' "$tmp/memburn.log" || { echo "stick-test: no PASS in memburn.log" >&2; exit 1; }
grep -aq '^Live view: https://' "$tmp/memburn.log" || { echo "stick-test: no live-view URL in memburn.log" >&2; exit 1; }
