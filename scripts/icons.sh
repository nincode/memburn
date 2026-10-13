#!/usr/bin/env bash
# Renders docs/img/icon.svg (icon-16.svg at 16 px) to the server's favicons and cli/memburn.ico; needs google-chrome and python3.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
chrome=${CHROME:-google-chrome}

# render SVG SIZE OUT [BLEED]: BLEED fills the square with the tile colour and crops the border (apple-touch-icon).
render() {
  local style="width:100vw;height:100vh" bg=transparent
  if [[ ${4:-} ]]; then style="width:calc(100vw + 4%);height:calc(100vh + 4%);margin:-2%"; bg="#1e1d1b"; fi
  cat > "$tmp/page.html" <<EOF
<html><body style="margin:0;overflow:hidden;background:$bg"><img src="$PWD/docs/img/$1" style="display:block;$style"></body></html>
EOF
  "$chrome" --headless --disable-gpu --hide-scrollbars --default-background-color=00000000 \
    --window-size="$2,$2" --screenshot="$3" "file://$tmp/page.html" >/dev/null 2>&1
  [[ $(file -b "$3") == "PNG image data, $2 x $2,"* ]] || { echo "render of $1 at $2 px failed" >&2; exit 1; }
}

render icon.svg 32 server/src/icon-32.png
render icon.svg 180 server/src/apple-touch-icon.png bleed
ico=()
for n in 16 24 32 48 256; do
  render "$([[ $n == 16 ]] && echo icon-16.svg || echo icon.svg)" "$n" "$tmp/$n.png"
  ico+=("$tmp/$n.png")
done
python3 - cli/memburn.ico "${ico[@]}" <<'EOF'
import struct, sys
out, pngs = sys.argv[1], [open(p, "rb").read() for p in sys.argv[2:]]
offset = 6 + 16 * len(pngs)
head = struct.pack("<HHH", 0, 1, len(pngs))
for png in pngs:
    w, h = struct.unpack(">II", png[16:24])
    head += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(png), offset)
    offset += len(png)
open(out, "wb").write(head + b"".join(pngs))
EOF
ls -l server/src/icon-32.png server/src/apple-touch-icon.png cli/memburn.ico
