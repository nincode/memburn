#!/usr/bin/env bash
# Boots a copy of target/usb/memburn-<version>-usb.img as a USB stick in QEMU/OVMF; SECURE_BOOT=0 turns Secure Boot off, NET=0 the e1000e NIC.
# Usage: run-usb-qemu.sh [memburn.cfg] [qemu args...]; the stick copy stays at target/usb/test.img, and KEEP=1 boots it again.
set -euo pipefail
cd "$(dirname "$0")/.."
cfg=${1:-}
shift || true
ovmf=/usr/share/OVMF
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[[ ${KEEP:-0} == 1 ]] || cp "target/usb/memburn-$version-usb.img" target/usb/test.img
[[ -n $cfg ]] && mcopy -o -i target/usb/test.img@@1M "$cfg" ::/memburn.cfg
if [[ ${SECURE_BOOT:-1} == 1 ]]; then
  code=$ovmf/OVMF_CODE_4M.secboot.fd; cp $ovmf/OVMF_VARS_4M.ms.fd target/usb/vars.fd
  machine=(-machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on)
else
  code=$ovmf/OVMF_CODE_4M.fd; cp $ovmf/OVMF_VARS_4M.fd target/usb/vars.fd
  machine=(-machine q35)
fi
net=(-nic user,model=e1000e)
[[ ${NET:-1} == 0 ]] && net=(-net none)
accel=(-cpu max)
[[ -w /dev/kvm ]] && accel=(-enable-kvm -cpu host)
qemu-system-x86_64 "${machine[@]}" "${accel[@]}" -smp "${SMP:-4}" -m "${MEM:-4G}" \
  -drive if=pflash,format=raw,readonly=on,file=$code -drive if=pflash,format=raw,file=target/usb/vars.fd \
  -device qemu-xhci -drive if=none,id=stick,format=raw,file=target/usb/test.img \
  -device usb-storage,drive=stick,bootindex=0 "${net[@]}" "$@"
