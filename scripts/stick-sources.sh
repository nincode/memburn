#!/usr/bin/env bash
# Fetches the Ubuntu source packages of the stick's pinned shim, GRUB, kernel and BusyBox (with what they
# were built from) from Launchpad into target/usb/sources/, and packs memburn-<version>-stick-sources.tar.
# Usage: stick-sources.sh [--list]; --list only prints the source packages and their Launchpad pages.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
arg() { sed -n "s/^ARG $1=//p" docker/usb.Dockerfile; }
base=$(sed -n 's/^FROM \(ubuntu:[^ ]*\) AS stick$/\1/p' docker/usb.Dockerfile)
kernel=$(arg KERNEL)
pins=("shim-signed=$(arg SHIM)" "grub-efi-amd64-signed=$(arg GRUB)" "linux-image-$kernel=$(arg KERNEL_PKG)"
  "linux-modules-$kernel=$(arg KERNEL_PKG)" "busybox-static=$(arg BUSYBOX)")

# "name version" per source package: each binary's Source and Built-Using, in the stick's own Docker base.
mapfile -t srcs < <(docker run --rm "$base" bash -c '
  apt-get update -qq >/dev/null
  for p in "$@"; do
    apt-cache show "$p" | awk -v v="${p#*=}" -v b="${p%%=*}" "
      /^Source:/ { s = \$2; sv = (\$3 ~ /^\\(/) ? substr(\$3, 2, length(\$3) - 2) : v }
      /^Built-Using:/ { n = split(substr(\$0, 14), a, \",\"); for (i = 1; i <= n; i++) {
        split(a[i], f, \" \"); bu[f[1]] = substr(f[3], 1, length(f[3]) - 1) } }
      END { if (s == \"\") { s = b; sv = v } print s, sv; for (k in bu) print k, bu[k] }"
  done' _ "${pins[@]}" | sort -u)

out=target/usb/sources
if [[ ${1:-} == --list ]]; then
  for s in "${srcs[@]}"; do set -- $s; echo "$1 $2 https://launchpad.net/ubuntu/+source/$1/$2"; done
  exit 0
fi
rm -rf "$out"; mkdir -p "$out"
: > "$out/SOURCES.txt"
for s in "${srcs[@]}"; do
  set -- $s
  name=$1 ver=$2
  lp="https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/$name/$ver"
  dsc="${name}_${ver#*:}.dsc"
  echo "$name $ver" >&2
  curl -fsSL --retry 3 -o "$out/$dsc" "$lp/$dsc"
  # Checksums-Sha256 lines of the .dsc: " <sha256> <size> <file>".
  awk '/^Checksums-Sha256:/ {on = 1; next} /^[^ ]/ {on = 0} on && NF == 3 {print $1, $3}' "$out/$dsc" |
    while read -r sum file; do
      curl -fsSL --retry 3 -o "$out/$file" "$lp/$file"
      echo "$sum  $out/$file" | sha256sum -c --quiet -
    done
  echo "$name $ver https://launchpad.net/ubuntu/+source/$name/$ver" >> "$out/SOURCES.txt"
done
tar=target/usb/memburn-$version-stick-sources.tar
tar -cf "$tar" -C target/usb sources
du -h "$tar" | cut -f1 >&2
echo "$tar"
