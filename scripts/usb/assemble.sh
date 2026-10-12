#!/usr/bin/env bash
# Runs in the build container: /in holds the binaries, /out receives memburn-$VERSION-usb.img, its file tree as a tar, the exe and the Linux binary.
set -euo pipefail
[[ -n ${VERSION:-} ]] || { echo "VERSION not set" >&2; exit 1; }
# FAT label: 11 characters at most, no dots.
label="MB ${VERSION//./-}"
(( ${#label} <= 11 )) || { echo "label '$label' is over 11 characters" >&2; exit 1; }
work=/work; root=$work/root; ir=$work/initramfs
mkdir -p $work $root $ir/{bin,dev,proc,sys,stick,etc,run,lib/modules} /out
cd $work
apt-get update -qq
apt-get download -q "shim-signed=$SHIM" "grub-efi-amd64-signed=$GRUB" \
  "linux-image-$KERNEL=$KERNEL_PKG" "linux-modules-$KERNEL=$KERNEL_PKG" "busybox-static=$BUSYBOX"
for d in *.deb; do dpkg-deb -x "$d" $root; done
[[ -e $root/lib ]] || ln -s usr/lib $root/lib
depmod -b $root "$KERNEL"

# USB keyboards and sticks, SATA, SD readers, laptop I2C keyboards; FAT, xHCI and virtio-net are built in.
mods=(usbhid hid_generic hid_multitouch i2c_hid_acpi usb_storage uas ahci mmc_block sdhci_pci sdhci_acpi nls_iso8859-1)
# Kernel panic logs kept in EFI variables across the reboot.
mods+=(efi_pstore)
# CPU temperature (AMD, Intel) and memory module sensors on the chipset SMBus (DDR4 TS, DDR5 SPD hub).
mods+=(k10temp coretemp i2c_piix4 i2c_i801 jc42 spd5118)
# Wired Ethernet for the live view: Intel, Realtek, Broadcom, Aquantia, Atheros, USB adapters.
mods+=(e1000 e1000e igb igc ixgbe r8169 tg3 bnxt_en atlantic alx r8152 cdc_ether cdc_ncm ax88179_178a asix)
: > $ir/lib/modules/load.list
for m in "${mods[@]}"; do
  modprobe -d $root -S "$KERNEL" --show-depends "$m" | awk '$1 == "insmod" {print $2}' | while read -r ko; do
    base=$(basename "$ko")
    grep -qx "$base" $ir/lib/modules/load.list && continue
    cp "$ko" $ir/lib/modules/
    echo "$base" >> $ir/lib/modules/load.list
  done
done

install -m755 /in/memburn-init $ir/init
install -m755 /in/memburn $ir/bin/memburn
install -m755 $root/usr/bin/busybox $ir/bin/busybox
for a in sh ip udhcpc; do ln -s busybox $ir/bin/$a; done
install -m755 /usb/udhcpc.script $ir/etc/udhcpc.script
mknod -m600 $ir/dev/console c 5 1
(cd $ir && find . | cpio -o -H newc -R 0:0 --quiet | zstd -19 -q) > initrd.img

img=/out/memburn-$VERSION-usb.img
rm -f $img
truncate -s 96M $img
echo 'start=2048, type=ef, bootable' | sfdisk -q $img
fat="$img@@1M"
mformat -i "$fat" -F -v "$label" ::
# The FAT tree: boot files, this version's kernel, initrd, binaries and UEFI build, and the Windows exe.
tree=$work/tree; ver=$tree/EFI/memburn/$VERSION
mkdir -p $tree/EFI/BOOT $tree/EFI/ubuntu $ver $tree/windows
cp $root/usr/lib/shim/shimx64.efi.signed.latest $tree/EFI/BOOT/BOOTX64.EFI
cp $root/usr/lib/shim/mmx64.efi $tree/EFI/BOOT/mmx64.efi
cp $root/usr/lib/grub/x86_64-efi-signed/grubx64.efi.signed $tree/EFI/BOOT/grubx64.efi
sed "s/@VERSION@/$VERSION/g" /usb/grub.cfg > $tree/EFI/BOOT/grub.cfg
cp $tree/EFI/BOOT/grub.cfg $tree/EFI/ubuntu/grub.cfg
cp $root/boot/vmlinuz-"$KERNEL" $ver/vmlinuz
cp initrd.img $ver/initrd.img
cp /in/memburn-uefi.efi $ver/memburn.efi
# The initrd's memburn and init again, so an update can restart memburn in place when only it changed.
cp /in/memburn $ver/memburn
cp /in/memburn-init $ver/memburn-init
cp /in/memburn.exe $tree/windows/memburn.exe
cp /usb/memburn.cfg.example $tree/memburn.cfg.example
sed 's/$/\r/' /usb/README.txt > $tree/README.txt
mcopy -s -i "$fat" $tree/* ::/
# The same tree for the stick's self-update.
(cd $tree && find . -type f | sed 's|^\./||' | LC_ALL=C sort |
  tar --format=ustar --owner=0 --group=0 --numeric-owner --mtime=@0 -cf /out/memburn-$VERSION-stick-files.tar -T -)
# Release files beside the image.
install -m755 /in/memburn /out/memburn-$VERSION-linux-x86_64
install -m644 /in/memburn.exe /out/memburn-$VERSION.exe
echo "label: $label"
echo "modules: $(tr '\n' ' ' < $ir/lib/modules/load.list)"
echo "initrd: $(du -h initrd.img | cut -f1)"
