#!/usr/bin/env bash
# Builds BOOTX64.EFI into target/esp/EFI/BOOT/.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p memburn-uefi --target x86_64-unknown-uefi --release
mkdir -p target/esp/EFI/BOOT
cp target/x86_64-unknown-uefi/release/memburn-uefi.efi target/esp/EFI/BOOT/BOOTX64.EFI
echo target/esp/EFI/BOOT/BOOTX64.EFI
