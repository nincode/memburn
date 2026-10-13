#!/usr/bin/env bash
# Usage: run-qemu.sh [memburn.cfg] [qemu args...]; with a cfg it runs headless and prints the debugcon log.
# Headless runs with no debugcon output after FIRST_OUTPUT_S (30 s) or still running after RUN_S (300 s) are
# killed and retried once; OVMF's serial log of a hung attempt (target/ovmf-serial-hung-<n>.log) tells a firmware hang from memburn's.
set -euo pipefail
cd "$(dirname "$0")/.."
cfg=${1:-}
shift || true
scripts/build-efi.sh >/dev/null
ovmf=/usr/share/OVMF
accel=(-cpu max)
[[ -w /dev/kvm ]] && accel=(-enable-kvm -cpu host)

boot() {
  rm -f target/esp/EFI/BOOT/memburn.cfg target/esp/memburn.log
  [[ -n $cfg ]] && cp "$cfg" target/esp/EFI/BOOT/memburn.cfg
  cp $ovmf/OVMF_VARS_4M.fd target/ovmf-vars.fd
  : > target/debugcon.log
  : > target/ovmf-serial.log
  exec qemu-system-x86_64 -machine q35 "${accel[@]}" -smp "${SMP:-4}" -m "${MEM:-2G}" \
    -drive if=pflash,format=raw,readonly=on,file=$ovmf/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,file=target/ovmf-vars.fd \
    -drive format=raw,file=fat:rw:target/esp \
    -debugcon file:target/debugcon.log -net none "$@"
}

if [[ -z $cfg ]]; then
  status=0
  (boot "$@") || status=$?
  cat target/debugcon.log
  exit $status
fi

first=${FIRST_OUTPUT_S:-30} limit=${RUN_S:-300}
for attempt in 1 2; do
  boot -display none -serial file:target/ovmf-serial.log "$@" &
  pid=$! start=$SECONDS why=""
  while kill -0 $pid 2>/dev/null; do
    t=$((SECONDS - start))
    if [[ ! -s target/debugcon.log ]] && ((t >= first)); then
      if grep -q "starting Boot" target/ovmf-serial.log; then
        why="no output from memburn ${first} s after OVMF started it"
      else
        why="OVMF did not start memburn within ${first} s"
      fi
    elif ((t >= limit)); then
      why="still running after ${limit} s"
    fi
    [[ -n $why ]] && break
    sleep 0.5
  done
  if [[ -z $why ]]; then
    status=0
    wait $pid || status=$?
    cat target/debugcon.log
    exit $status
  fi
  kill $pid 2>/dev/null
  wait $pid 2>/dev/null || true
  cat target/debugcon.log
  cp target/ovmf-serial.log "target/ovmf-serial-hung-$attempt.log"
  echo "run-qemu: attempt $attempt: $why; OVMF log in target/ovmf-serial-hung-$attempt.log" >&2
done
echo "run-qemu: QEMU hung twice; see target/debugcon.log and target/ovmf-serial-hung-*.log" >&2
exit 1
