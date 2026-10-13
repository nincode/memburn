#!/usr/bin/env bash
# Fetches the Ubuntu source packages of the stick's pinned shim, GRUB, kernel and BusyBox (with what they
# were built from) from Launchpad into target/usb/sources/, and packs target/usb/<tag>.tar.
# Downloads are cached in ~/.cache/memburn/sources/<package>_<version>/ and reused when they match the .dsc.
# Usage: stick-sources.sh [--list | --tag]; --list prints the source packages and their Launchpad pages,
# --tag the release tag of this pin set: stick-sources-<first 12 hex digits of the pins' SHA-256>.
set -euo pipefail
cd "$(dirname "$0")/.."
cache=${MEMBURN_SOURCES_CACHE:-$HOME/.cache/memburn/sources}
arg() { sed -n "s/^ARG $1=//p" docker/usb.Dockerfile; }
base=$(sed -n 's/^FROM \(ubuntu:[^ ]*\) AS stick$/\1/p' docker/usb.Dockerfile)
kernel=$(arg KERNEL)
pins=("shim-signed=$(arg SHIM)" "grub-efi-amd64-signed=$(arg GRUB)" "linux-image-$kernel=$(arg KERNEL_PKG)"
  "linux-modules-$kernel=$(arg KERNEL_PKG)" "busybox-static=$(arg BUSYBOX)")

tag=stick-sources-$(printf '%s\n' "$base" "${pins[@]}" | sha256sum | cut -c1-12)
if [[ ${1:-} == --tag ]]; then echo "$tag"; exit 0; fi

# "name version" per source package: each binary's Source and Built-Using, in the stick's own Docker base.
mkdir -p "$cache"
list=$cache/$tag.list
if [[ ! -s $list ]]; then
  docker run --rm "$base" bash -c '
  apt-get update -qq >/dev/null
  for p in "$@"; do
    apt-cache show "$p" | awk -v v="${p#*=}" -v b="${p%%=*}" "
      /^Source:/ { s = \$2; sv = (\$3 ~ /^\\(/) ? substr(\$3, 2, length(\$3) - 2) : v }
      /^Built-Using:/ { n = split(substr(\$0, 14), a, \",\"); for (i = 1; i <= n; i++) {
        split(a[i], f, \" \"); bu[f[1]] = substr(f[3], 1, length(f[3]) - 1) } }
      END { if (s == \"\") { s = b; sv = v } print s, sv; for (k in bu) print k, bu[k] }"
  done' _ "${pins[@]}" | sort -u > "$list.part"
  mv "$list.part" "$list"
fi
mapfile -t srcs < "$list"

out=target/usb/sources
if [[ ${1:-} == --list ]]; then
  for s in "${srcs[@]}"; do set -- $s; echo "$1 $2 https://launchpad.net/ubuntu/+source/$1/$2"; done
  exit 0
fi
rm -rf "$out"; mkdir -p "$out"
: > "$out/SOURCES.txt"
fetched=0 reused=0
fetch() { curl -fsSL --retry 3 -o "$2.part" "$1" && mv "$2.part" "$2"; }
for s in "${srcs[@]}"; do
  set -- $s
  name=$1 ver=$2
  lp="https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/$name/$ver"
  dsc="${name}_${ver#*:}.dsc"
  dir=$cache/${name}_${ver#*:}
  mkdir -p "$dir"
  echo "$name $ver" >&2
  if [[ -s $dir/$dsc ]]; then reused=$((reused + 1)); else fetch "$lp/$dsc" "$dir/$dsc"; fetched=$((fetched + 1)); fi
  ln -f "$dir/$dsc" "$out/$dsc" 2>/dev/null || cp "$dir/$dsc" "$out/$dsc"
  # Checksums-Sha256 lines of the .dsc: " <sha256> <size> <file>".
  while read -r sum file; do
    if echo "$sum  $dir/$file" | sha256sum -c --quiet - 2>/dev/null; then
      reused=$((reused + 1))
    else
      fetch "$lp/$file" "$dir/$file"
      echo "$sum  $dir/$file" | sha256sum -c --quiet -
      fetched=$((fetched + 1))
    fi
    ln -f "$dir/$file" "$out/$file" 2>/dev/null || cp "$dir/$file" "$out/$file"
  done < <(awk '/^Checksums-Sha256:/ {on = 1; next} /^[^ ]/ {on = 0} on && NF == 3 {print $1, $3}' "$dir/$dsc")
  echo "$name $ver https://launchpad.net/ubuntu/+source/$name/$ver" >> "$out/SOURCES.txt"
done
echo "downloaded $fetched files, reused $reused from $cache" >&2
tar=target/usb/$tag.tar
tar -cf "$tar" -C target/usb sources
du -h "$tar" | cut -f1 >&2
echo "$tar"
