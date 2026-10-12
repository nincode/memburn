#!/usr/bin/env bash
# Usage: run-qemu.sh [memburn.cfg] [qemu args...]; with a cfg it runs headless and prints the debugcon log.
set -euo pipefail
cd "$(dirname "$0")/.."
cfg=${1:-}
shift || true
scripts/build-efi.sh >/dev/null
rm -f target/esp/EFI/BOOT/memburn.cfg target/esp/memburn.log
[[ -n $cfg ]] && cp "$cfg" target/esp/EFI/BOOT/memburn.cfg
ovmf=/usr/share/OVMF
cp $ovmf/OVMF_VARS_4M.fd target/ovmf-vars.fd
accel=(-cpu max)
[[ -w /dev/kvm ]] && accel=(-enable-kvm -cpu host)
display=()
[[ -n $cfg ]] && display=(-display none)
: > target/debugcon.log
qemu-system-x86_64 -machine q35 "${accel[@]}" -smp "${SMP:-4}" -m "${MEM:-2G}" \
  -drive if=pflash,format=raw,readonly=on,file=$ovmf/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=target/ovmf-vars.fd \
  -drive format=raw,file=fat:rw:target/esp \
  -debugcon file:target/debugcon.log -net none "${display[@]}" "$@"
cat target/debugcon.log
