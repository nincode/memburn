#!/usr/bin/env bash
# QEMU screenshots of the stick and bare-metal UEFI from plan files, several plans at once.
# Usage: shots.sh [-j N] [-o DIR] PLAN...   writes DIR/<plan>-<shot>.png (DIR default target/shots, N default 4)
# Plan lines (# comments), run in order:
#   target stick|uefi        stick: copy of target/usb/memburn-<version>-usb.img; uefi: copy of target/esp
#   size WxH                 OVMF PlatformConfig resolution (default 1280x800); bare-metal UEFI picks its own mode
#   smp N | mem SIZE | net 0|1 | secure-boot 0|1 | qemu ARGS...   before the first wait
#   cfg LINE                 memburn.cfg line
#   wait log|serial REGEX [S]   log: memburn.log (stick) or debugcon (uefi); default 90 s
#   sleep S | key KEY... (QEMU sendkey names: s, ret, shift-u, esc) | shot NAME | exit [S] (QEMU powers off)
# Each plan gets its own image copy, vars file and monitor socket; a failed plan runs once more (RETRY=0 turns it off).
set -euo pipefail
cd "$(dirname "$0")/.."
jobs=4 out=target/shots
while getopts j:o: o; do
  case $o in
    j) jobs=$OPTARG ;;
    o) out=$OPTARG ;;
    *) exit 2 ;;
  esac
done
shift $((OPTIND - 1))
(($#)) || { sed -n '2,12s/^# \{0,1\}//p' "$0"; exit 2; }
mkdir -p "$out"
out=$(realpath "$out")
work=$(mktemp -d "${TMPDIR:-/tmp}/shots.XXXXXX")
ovmf=/usr/share/OVMF
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
fwvars=(virt-fw-vars)
command -v virt-fw-vars >/dev/null || fwvars=(uv tool run --from virt-firmware virt-fw-vars)

cleanup() {
  kill $(jobs -p) 2>/dev/null || true
  for f in "$work"/*/qemu.pid; do
    [[ -s $f ]] && kill "$(cat "$f")" 2>/dev/null || true
  done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if grep -qh "^target *uefi" "$@"; then scripts/build-efi.sh >/dev/null 2>&1 || scripts/build-efi.sh; fi
if grep -qh '^target *stick' "$@" && [[ ! -f target/usb/memburn-$version-usb.img ]]; then
  echo "shots: target/usb/memburn-$version-usb.img missing; run scripts/mkusb.sh" >&2
  exit 1
fi

# HMP command over the plan's monitor socket.
mon() {
  python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(10)
buf = b""
while not buf.endswith(b"(qemu) "): buf += s.recv(4096)
s.sendall(sys.argv[2].encode() + b"\n"); buf = b""
while not buf.endswith(b"(qemu) "): buf += s.recv(4096)
' "$1" "$2"
}

# Runs one plan in directory $2; prints its failure reason on stderr.
run() {
  local plan=$1 w=$2 name
  name=$(basename "$plan" .plan)
  local target="" size=1280x800 smp=4 mem="" net=1 sb=1 extra=() cfg=() pid="" started=0 n=0
  local img log serial=$w/serial.log sock=$w/mon.sock
  fail() {
    echo "shots: $name: line $n: $*" >&2
    [[ -n $pid ]] && kill "$pid" 2>/dev/null
    return 1
  }
  logtext() {
    if [[ $target == stick ]]; then mtype -i "$img@@1M" ::/memburn.log 2>/dev/null || true; else cat "$log" 2>/dev/null || true; fi
  }
  start() {
    local w_ h_ vars=$w/vars.fd code machine
    w_=${size%x*} h_=${size#*x}
    if [[ $target == stick ]]; then
      img=$w/stick.img
      cp --reflink=auto "target/usb/memburn-$version-usb.img" "$img"
      if ((${#cfg[@]})); then
        printf '%s\r\n' "${cfg[@]}" > "$w/memburn.cfg"
        mcopy -o -i "$img@@1M" "$w/memburn.cfg" ::/memburn.cfg
      fi
      if [[ $sb == 1 ]]; then
        code=$ovmf/OVMF_CODE_4M.secboot.fd base=$ovmf/OVMF_VARS_4M.ms.fd
        machine=(-machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on)
      else
        code=$ovmf/OVMF_CODE_4M.fd base=$ovmf/OVMF_VARS_4M.fd
        machine=(-machine q35)
      fi
      disk=(-device qemu-xhci -drive "if=none,id=stick,format=raw,file=$img" -device usb-storage,drive=stick,bootindex=0)
      nic=(-nic user,model=e1000e)
      [[ $net == 0 ]] && nic=(-net none)
      mem=${mem:-3G}
    else
      log=$w/debugcon.log
      cp -r target/esp "$w/esp"
      rm -f "$w/esp/EFI/BOOT/memburn.cfg" "$w/esp/memburn.log"
      ((${#cfg[@]})) && printf '%s\n' "${cfg[@]}" > "$w/esp/EFI/BOOT/memburn.cfg"
      code=$ovmf/OVMF_CODE_4M.fd base=$ovmf/OVMF_VARS_4M.fd
      machine=(-machine q35)
      disk=(-drive "format=raw,file=fat:rw:$w/esp" -debugcon "file:$log")
      nic=(-net none)
      mem=${mem:-2G}
    fi
    printf '{"version": 2, "variables": [{"name": "PlatformConfig", "guid": "7235c51c-0c80-4cab-87ac-3b084a6304b1", "attr": 3, "data": "%s%s"}]}\n' \
      "$(swap32 "$w_")" "$(swap32 "$h_")" > "$w/platform.json"
    "${fwvars[@]}" -i "$base" --set-json "$w/platform.json" -o "$vars" >/dev/null 2>&1 || fail "virt-fw-vars failed" || return 1
    local accel=(-cpu max)
    [[ -w /dev/kvm ]] && accel=(-enable-kvm -cpu host)
    qemu-system-x86_64 "${machine[@]}" "${accel[@]}" -smp "$smp" -m "$mem" \
      -drive "if=pflash,format=raw,readonly=on,file=$code" -drive "if=pflash,format=raw,file=$vars" \
      "${disk[@]}" "${nic[@]}" -display none -serial "file:$serial" -monitor "unix:$sock,server,nowait" \
      "${extra[@]}" > "$w/qemu.out" 2>&1 &
    pid=$!
    echo "$pid" > "$w/qemu.pid"
    started=1
    local t=0
    while [[ ! -S $sock ]]; do
      kill -0 "$pid" 2>/dev/null || fail "QEMU exited: $(cat "$w/qemu.out")" || return 1
      sleep 0.1
      ((++t < 100)) || fail "no monitor socket after 10 s" || return 1
    done
  }
  while IFS= read -r line || [[ -n $line ]]; do
    n=$((n + 1))
    line=${line%$'\r'}
    [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
    read -r op rest <<< "$line"
    case $op in
      target) target=$rest ;;
      size) size=$rest ;;
      smp) smp=$rest ;;
      mem) mem=$rest ;;
      net) net=$rest ;;
      secure-boot) sb=$rest ;;
      qemu) read -ra more <<< "$rest"; extra+=("${more[@]}") ;;
      cfg) cfg+=("$rest") ;;
      *)
        [[ $target == stick || $target == uefi ]] || fail "target stick or uefi first" || return 1
        ((started)) || start || return 1
        case $op in
          wait)
            local src re secs=90 t0=$SECONDS
            read -r src re <<< "$rest"
            if [[ $re =~ ^(.*)[[:space:]]+([0-9]+)$ ]]; then re=${BASH_REMATCH[1]} secs=${BASH_REMATCH[2]}; fi
            while :; do
              if [[ $src == serial ]]; then grep -aqE -- "$re" "$serial" 2>/dev/null && break
              else logtext | grep -aqE -- "$re" && break; fi
              kill -0 "$pid" 2>/dev/null || fail "QEMU exited before /$re/ in the $src log" || return 1
              ((SECONDS - t0 < secs)) || fail "no /$re/ in the $src log after $secs s; last lines:
$( if [[ $src == serial ]]; then tail -5 "$serial"; else logtext | tail -5; fi)" || return 1
              sleep 0.5
            done
            ;;
          sleep) sleep "$rest" ;;
          key) for k in $rest; do mon "$sock" "sendkey $k" >/dev/null || fail "sendkey $k" || return 1; sleep 0.15; done ;;
          shot) mon "$sock" "screendump $out/$name-$rest.png -f png" >/dev/null || fail "screendump" || return 1 ;;
          exit)
            local t0=$SECONDS
            while kill -0 "$pid" 2>/dev/null; do
              ((SECONDS - t0 < ${rest:-60})) || fail "QEMU still running after ${rest:-60} s" || return 1
              sleep 0.5
            done
            pid=""
            ;;
          *) fail "unknown step '$op'" || return 1 ;;
        esac
        ;;
    esac
  done < "$plan"
  [[ -n $pid ]] && kill "$pid" 2>/dev/null
  if [[ $target == stick ]]; then logtext > "$out/$name-memburn.log"; else cp "$log" "$out/$name-debugcon.log"; fi
  return 0
}

swap32() { printf '%08x' "$1" | sed 's/\(..\)\(..\)\(..\)\(..\)/\4\3\2\1/'; }

# One plan with a retry, its result line on stdout.
plan() {
  local p=$1 name t0=$SECONDS try
  name=$(basename "$p" .plan)
  for try in 1 2; do
    local w=$work/$name.$try
    mkdir -p "$w"
    if (run "$p" "$w"); then
      echo "shots: $name: ok in $((SECONDS - t0)) s"
      : > "$work/$name.ok"
      return 0
    fi
    [[ ${RETRY:-1} == 1 ]] || break
  done
  echo "shots: $name: FAILED after $((SECONDS - t0)) s"
}

t0=$SECONDS
for p in "$@"; do
  while (($(jobs -rp | wc -l) >= jobs)); do wait -n || true; done
  plan "$p" &
done
wait
status=0
for p in "$@"; do [[ -f $work/$(basename "$p" .plan).ok ]] || status=1; done
echo "shots: $# plans in $((SECONDS - t0)) s, PNGs in $out"
exit $status
