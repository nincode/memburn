# memburn development

Memory test (memtest86+-style patterns) and FFT burn-in (Prime95-style, exact result checks), in Rust.
Every front end draws the same full-screen dashboard; networked runs upload to a live page with a QR code.

| Crate | What |
|---|---|
| `core` | `no_std` tests: memory patterns, parallel phase engine, SSE2/AVX2 FFT kernels, error log, dashboard renderer, run history |
| `gfx` | `no_std` pixel dashboard on 0x00RRGGBB canvases with Inter and JetBrains Mono (fontdue); changed spans per row |
| `cli` | `memburn` / `memburn.exe`: terminal dashboard, plain-text mode, uploader |
| `uefi` | `memburn.efi`: bare-metal build that uses every CPU and nearly all RAM; no network |
| `linux-init` | PID 1 on the stick: loads drivers, starts DHCP, runs `memburn --boot` |
| `proto` | JSON snapshot shared by the CLI and the server |

## Build and test

```sh
cargo build --release                                     # Linux
cargo build --release --target x86_64-pc-windows-gnu      # Windows exe
scripts/mkusb.sh                                          # Docker build → target/usb/memburn-<version>-usb.img
cargo test --workspace --exclude memburn-uefi             # core, proto, cli; fault injection included
scripts/run-qemu.sh                                       # bare-metal UEFI build in QEMU/OVMF
scripts/run-qemu.sh tests/qemu-smoke.cfg                  # headless smoke run
scripts/run-usb-qemu.sh [memburn.cfg]                     # the stick image with Secure Boot and e1000e
scripts/stick-test.sh                                     # the stick image headless: 30 s quick check, upload, PASS
cargo run --release -p memburn --example render -- out/   # dashboard scenes as cell dumps
cargo run --release -p memburn --example visuals -- out/  # dashboard renders as PNG, GIF, WebP (ffmpeg)
cargo run --release -p memburn --example visuals -- out/ states  # every fixture state as PNG, text and pixel
cargo run --release -p memburn --example visuals -- out/ anim    # animated pixel dashboard WebPs for the README and web page
scripts/shots.sh tests/shots/*.plan                       # QEMU screenshots of the stick and UEFI, in parallel
MEMBURN_GFX_OUT=out/ cargo test -p memburn-gfx renders    # pixel dashboard scenes as PPM at four sizes
cargo test --release -p memburn-gfx frame_time -- --ignored --nocapture  # pixel frame times, 1080p and 4K
cargo run --release -p memburn --example burnbench -- [rounds] [secs]  # what the FFT lanes cost the burn-in
```

The plan panel's free rows hold a map of the memory under test: the tested regions back to back, lit as
the sweep passes, failing words in red, the error ranges on its title row. It draws braille where the font
has it (terminal emulators, Windows Terminal, bare-metal UEFI on GOP) and half blocks elsewhere (Linux
console, conhost, the UEFI text console). A fire burns under the QR code.

The stick's `memburn --boot` and bare-metal UEFI on GOP draw the pixel dashboard from `gfx` instead:
every 32-bit `/dev/fb*` (console switched to `KD_GRAPHICS`) or every GOP display of at least 640×480,
painted once per distinct size, only changed spans copied. Without one they keep the cell dashboard; the
Windows exe and `memburn` in a terminal always do. Snapshot hashes in `gfx/src/tests.rs` change with any
pixel; update them after checking the renders.

About one OVMF boot in 20 (Ubuntu's `OVMF_CODE_4M.fd`, KVM) stalls after console setup and before
`BdsDxe: starting Boot0002`, so memburn never runs: 9 of 200 boots, the same with `-smp 1` and with a
FAT image instead of `fat:rw`. `run-qemu.sh` kills a run with no output after 30 s or still running
after 300 s and retries it once, keeping the hung attempt's OVMF log in `target/ovmf-serial-hung-<n>.log`.

`memburn-uefi` builds only with `--target x86_64-unknown-uefi` (`scripts/build-efi.sh`).
`inject=1` in a cfg (or `--inject` on the CLI) injects memory and CPU faults to check reporting.
`crash_after=N` in a stick cfg panics the kernel N s into the first run, to check crash logs.
`scripts/mkusb.sh --build-arg MEMBURN_RELEASE_KEY=<hex>` builds a stick that trusts another release
key (`scripts/release-key.sh KEY` prints it); `scripts/sign-manifest.sh` signs a manifest for it, and a
server built with the same `MEMBURN_RELEASE_KEY` serves it.
`mkusb.sh` runs in two trees share one cargo cache: build them one after another, and force a rebuild
with `--no-cache-filter build,stick`.

## Screenshots

| | |
|---|---|
| Headless | `cargo run --release -p memburn --example visuals -- <dir> states [filter]` writes `<state>-text160x50.png`, `-text80x25.png` (the stick's 8×16 font), `-1280x800.png` and `-1920x1080.png` for every state in `gfx/src/fixtures.rs` in under a second, and checks the snapshot states against their hashes. A new dashboard state gets a fixture there first |
| Animated | `cargo run --release -p memburn --example visuals -- <dir> anim [filter]` writes `anim-memtest-fail.webp` (a failing Modulo 20 test, the sweep finding both error ranges each pass) and `anim-burn-in-fail.webp` (FFT burn-in, CPU 6 returning a wrong result) from the gfx fixtures: lossless animated WebP, 1280×800, one frame per 250 ms like the dashboard's redraw, under 400 KB each. Copied to `docs/img/pixel-*.webp` and the web page's images |
| QEMU | `scripts/shots.sh [-j N] [-o DIR] PLAN...` boots each plan's stick image or UEFI build, follows its steps and writes `DIR/<plan>-<shot>.png` (default `target/shots`), N plans at once (default 4), each with its own image copy, OVMF vars and monitor socket; it kills its QEMUs on exit and stops a plan at the first step that times out, with the log's last lines |
| Plans | `tests/shots/*.plan`, one step per line: `target stick\|uefi`, `size WxH` (OVMF PlatformConfig; bare-metal UEFI picks its own mode), `cfg LINE`, `smp`, `mem`, `net 0\|1`, `secure-boot 0\|1`, `qemu ARGS`, then `wait log\|serial REGEX [S]`, `sleep S`, `key KEY...` (QEMU sendkey names), `shot NAME`, `exit [S]`. `log` is the stick's `memburn.log` or UEFI's debugcon |
| Needs | `target/usb/memburn-<version>-usb.img` for stick plans (`scripts/mkusb.sh`); `virt-fw-vars` on the PATH or `uv` |

Review shots come from the headless renders; QEMU proves one real boot per target. The two shipped plans
(stick quick check → S → stress, UEFI smoke run) take about 45 s together.


## Modes

| Key | Mode | Contents | Default length |
|---|---|---|---|
| Enter | Quick check | Address + block moves, 8-bit walk, random sequence ×5, hashed pattern, then FFT burn-in | 2 min burn-in + ~2.5 s per GiB |
| F | Full memory test | Nine memory tests (`--tests all` adds bit fade and address + block moves) | ~6 s per GiB |
| B | Burn-in | FFT sizes 2^10..2^20 on every thread | 30 min |
| S | Stress | Memory test (without the hashed pattern) on half the threads and FFTs on the other half, together | 30 min |
| C | Core cycler | FFT sizes 2^10..2^13 on one logical CPU at a time (Linux and Windows pin a thread; UEFI runs one CPU in APIC id order), alternating AVX2 and SSE2 every 3 s, idle 5 s each minute; one row per CPU | 2 min per CPU, until Esc |
| A | All, repeating | Full memory test, then 10 min burn-in, until Esc | until Esc |

Stress runs only under Linux and Windows (the stick's default entry), not bare-metal UEFI.

## The stick

| | |
|---|---|
| Boot | UEFI only; Secure Boot may stay on (Ubuntu's signed shim, GRUB and kernel) |
| GRUB entries | `memburn <version>` (Linux, network, live view) and `memburn <version> bare-metal UEFI` (Secure Boot off, no network) |
| Volume label | `MB 0-9-3` for 0.9.3 |
| Network | Wired Ethernet via DHCP: Intel, Realtek, Broadcom, Aquantia, Atheros and common USB adapters |
| Config | `memburn.cfg` at the stick's root, keys as in `uefi/memburn.cfg.example`; `server=URL` picks another live-view server |
| Temperatures | CPU (k10temp, coretemp; bare-metal UEFI reads the same registers directly) and memory modules with a DDR4 JC-42 or DDR5 SPD hub sensor on the chipset SMBus; RAM temp shows only when a sensor answers |
| Displays | Bare-metal UEFI draws on every graphics output, 1920×1080 or the current mode if larger, centred at the smallest screen's size; the firmware text console only when none works |
| Crashes | A kernel panic reboots after 30 s; its log is saved as `memburn-crash-<date>.txt` at the stick's root and the menu then waits for a key |

### Updates

The stick's Linux entry asks the live-view server for the latest release from a background thread
(nice 19, idle I/O) once a default route exists, retrying every 2 s doubling to 60 s for as long as memburn
runs. A newer release whose manifest carries a valid signature of the release key is downloaded and staged
right away; U on the menu, the run or the result screen (or `update=auto`) opts in. `update=off` sends no check
until U: then memburn checks, downloads, installs and starts a newer release as an opt-in does, or shows "up to
date". While no check runs and nothing is on offer (`update=off`, up to date, failed), the menu and result screens
list U, which checks again.

| | |
|---|---|
| Manifest | version, the stick's files as one tar (URL, size, SHA-256), and each file's path, size and SHA-256; signed with Ed25519, public key in `proto/src/release.rs` |
| Stage | streams the tar, writes each changed file as `<file>.new` beside the old one with fsync, and checks every file's hash and the tar's, then reads the written files back from the stick past the page cache and checks them again; any error removes the `.new` files |
| Install | renames the `.new` files into place with `grub.cfg` last and syncs: without opt-in when the run ends (R starts it), with opt-in at once, stopping a run |
| Start | memburn copies the new `memburn` to `/memburn-next`, checks its hash and execs it in its own process with this run's arguments plus `--updated-from` and `--job` (or `--menu`), so init sees no exit; a new kernel and `memburn-init` start at the next boot. Only when the exec fails does memburn exit 12, and init (every version since 0.6) runs `/memburn-next` with the arguments in `/memburn-next.args` |
| Keys | R execs memburn again at the menu (the installed update's binary, if any); O exits 10 and init reboots; P exits 11 and init powers off |
| Layout | `EFI/memburn/<version>/` holds each version's `vmlinuz`, `initrd.img`, `memburn`, `memburn-init` and `memburn.efi`; GRUB boots the new version by default and keeps `memburn <old> (previous)` entries; older versions are deleted |
| Never written | `memburn.cfg`, `memburn.log`, `memburn-history.txt`, `memburn-crash-*.txt` |
| Refused | a bad signature, a version not newer than the running one and the stick's, a hash mismatch, a short download; the stick stays unchanged |
| Too little space | the row reads `Update <version>  re-flash needed` and offers no key |
| Init | `memburn-init` has its own fixed version and no workspace dependency, so its binary changes only with its code |

## CLI options

| Option | |
|---|---|
| `-m quick\|mem\|burn\|both\|stress\|cores` | skip the menu |
| `--mem 85%\|8G\|-512M` | amount of RAM to test: share or size, or available less a size (the stick: `-384M`) |
| `-p N`, `--tests` | memory passes; tests `addr,mi01,mi8,mirand,mi64,block,random,mod20,hash,fade,addrmove` or `all` |
| `-d MIN`, `--fft small\|large\|mixed` | burn-in or stress minutes (core cycler: per CPU), FFT sizes |
| `--isa auto\|sse2\|avx2`, `-t N`, `-c N` | kernel, threads, cycles (0 = forever) |
| `--plain` | no dashboard; also used when output is not a terminal |
| `--no-upload`, `--server URL` | live view off, or another server |
| `--log FILE` | append the run log |

Exit codes: 0 pass, 1 errors found, 2 usage, 3 stopped early; on the stick 10 restart, 11 power off, 12 start the updated memburn.
Under Linux the test memory asks for huge pages; the log says how much got them. If it cannot be
locked, it shrinks in 256 MiB steps down to 95% of available RAM. Locking test memory needs
administrator rights (Linux: `ulimit -l`); without it the header shows "memory not locked".

## Time left

Memory test time scales with bus traffic, not sweeps: `TestId::traffic` weighs each phase (a write
counts 2, a read 1, a copy 1.5), and every test costs about the same per traffic unit (0.86–0.91 s per
30 GiB on a 5950X). `core::eta` divides the traffic still to move (this test; the job's passes, later
cycles when finite, plus burn-in and bit-fade holds) by the measured rate: the average for the first
10 s, then an exponential average over about 10 s. It shows "estimating" for the first 3 s, and the
shown time counts down by itself until a new estimate differs by more than 5 % or 5 s. The menu's
estimates use 29 ms per GiB of traffic.

## Coverage

Coverage is the share of System RAM that a run tested, by physical address: the test buffer's pages
(Linux as root, from `/proc/self/pagemap`) plus the kernel's early test, or the claimed pages on
bare-metal UEFI. The stick's Linux entry boots with `memtest=2`, so the kernel tests its free memory
with two patterns before anything else runs, about 3 s per pattern per 16 GiB, while GRUB's
"Kernel memory test" line stays on screen; `early_memtest=N` in `memburn.cfg` changes the count
(0 turns it off). Its result is the "Kernel early test" row, and bad ranges it finds count as memory
errors. Bare-metal UEFI moves the 64 MiB it leaves the firmware to tested memory after the main tests
and tests that too. The summary adds the union over the last 10 runs on the same machine and RAM size,
and the log lists the largest untested ranges.

## Errors and bit statistics

The hashed pattern (`hash`) writes every word as a hash of its index (six multiply/xorshift rounds, SSE2 or AVX2),
then recomputes and checks it: full core load and near full memory bandwidth together.

Errors appear the moment they are found, with address, written and read values, and flipped bits.
The address is physical (`phys`) on bare-metal UEFI and under Linux as root (the stick); otherwise it
is the process's virtual address (`virt`).
The errors panel lists each flipped bit with its direction counts, the number of distinct failing
addresses, and how many earlier runs flipped the same bit. Bits flipping only one way three or more
times are tagged as stuck. Each bit sits in byte lane bit/8 (lane 2 is bits 16-23), one of the eight
data bytes of a 64-bit channel.

The log, the summary and the live view list the memory modules from SMBIOS: slot, size, rated and
running speed, maker, part number and serial. Under Linux this needs root (the stick); Windows and
UEFI always have it.

`core/src/dram.rs` names the likely module on single-node AMD Data Fabric 2 and 3 (Zen to Zen 3).
It reads the DRAM address maps from PCI config of 00:18.0/1 (sysfs on Linux, the root bridge
protocol on UEFI), applies the channel hash, and maps the channel (UMC n = letter A+n) to the SMBIOS
locator. With one module per channel it names the slot, with more it says "channel X"; it does not
decode chip selects. It refuses on DF3.5 and later, multi-node, die or socket interleave, unknown modes,
a size mismatch with SMBIOS and channel letters that disagree with the map; Intel, VMs, QEMU and
Windows fall back to no hint.
memburn.log holds the raw registers once per boot (`DRAM map registers: cpuid ... 18.0:110=...`)
and the result (`DRAM map: Df3, channels A, B` or `DRAM map: not decoded, <reason>`).
`Dump::parse` reads that line back, so a user's log becomes a unit test. The hint shows in error
lines, the errors table of the live view, the summary, the modules panel and the "Find the stick" line.

## Run history

Run history is a start line and a result line per run in `memburn-history.txt`: next to `--log` (the
stick's root), else `%LOCALAPPDATA%\memburn\` or `~/.local/state/memburn/`. A start line without a
result is a run that did not finish (crash, reset or power cut); the menu names the last one.
Result lines also carry `ram=` (System RAM, MiB), `host=` (a hash of the module list) and `cov=`, the
tested ranges as `<shift>:<gap>+<length>,...` in hex units of 2^shift bytes, coarsened until the
line stays under 4 KiB.

## Live view

With a network the client creates a run on the server once a default route exists (retrying after 1 s,
doubling to 15 s), shows its URL and QR code, and uploads a snapshot every 3 s. Until then the Live view
panel shows "waiting for network", "connecting" or the error. When the job ends it sends one final snapshot with the verdict and finish time
(up to 4 tries) and stops; the next job from the result screen gets a new run. The page then shows
PASS, FAIL or STOPPED and the finish time, and stops polling; a running run silent for 30 s shows
"No updates since". Runs are kept 30 days.

The live view is a hosted service at reljam.com. `--no-upload` (stick: `upload=0`) turns it off;
`--server URL` sends to another server that speaks the JSON of `memburn-proto`.
