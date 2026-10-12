# memburn

Memory test (memtest86+-style patterns) and FFT burn-in (Prime95-style, exact result checks), in Rust.
It runs from a bootable USB stick (Secure Boot on), as bare-metal UEFI, or as a Windows/Linux program.
Every front end draws the same full-screen dashboard; networked runs upload to a live page with a QR code.
Downloads: https://reljam.com/memburn/ and the GitHub releases.

| Crate | What |
|---|---|
| `core` | `no_std` tests: memory patterns, parallel phase engine, SSE2/AVX2 FFT kernels, error log, dashboard renderer, run history |
| `cli` | `memburn` / `memburn.exe`: terminal dashboard, plain-text mode, uploader |
| `uefi` | `memburn.efi`: bare-metal build that uses every CPU and nearly all RAM; no network |
| `linux-init` | PID 1 on the stick: loads drivers, starts DHCP, runs `memburn --boot` |
| `proto` | JSON snapshot shared by the CLI and the server |

## Modes

| Key | Mode | Contents | Default length |
|---|---|---|---|
| Enter | Quick check | Address + block moves, 8-bit walk, random sequence ×5, hashed pattern, then FFT burn-in | ~2 min burn-in |
| F | Full memory test | Nine memory tests (`--tests all` adds bit fade and address + block moves) | ~6 s per GiB |
| B | Burn-in | FFT sizes 2^10..2^20 on every thread | 30 min |
| S | Stress | Memory test (without the hashed pattern) on half the threads and FFTs on the other half, together | 30 min |
| C | Core cycler | FFT sizes 2^10..2^13 on one logical CPU at a time (Linux and Windows pin a thread; UEFI runs one CPU in APIC id order), alternating AVX2 and SSE2 every 3 s, idle 5 s each minute; one row per CPU | 2 min per CPU, until Esc |
| A | All, repeating | Full memory test, then 10 min burn-in, until Esc | until Esc |

The menu starts the quick check after 10 s. Esc stops after the current step; Q enlarges the QR code.
Stress runs only under Linux and Windows (the stick's default entry), not bare-metal UEFI.

## Bootable USB

```sh
scripts/mkusb.sh            # Docker build → target/usb/memburn-<version>-usb.img
```

| Step | |
|---|---|
| Write the image | Rufus in **DD mode**, balenaEtcher, or `dd of=/dev/sdX` |
| Boot | UEFI only; Secure Boot may stay on (Ubuntu's signed shim, GRUB and kernel) |
| GRUB entries | `memburn <version>` (Linux, network, live view) and `memburn <version> bare-metal UEFI` (Secure Boot off, no network) |
| Volume label | `MB 0-5-0` for 0.5.0 |
| Network | Wired Ethernet via DHCP: Intel, Realtek, Broadcom, Aquantia, Atheros and common USB adapters |
| Unattended | Rename `memburn.cfg.example` at the stick's root to `memburn.cfg`; `server=URL` picks another live-view server |
| Results | Screen, live page, `memburn.log` and `memburn-history.txt` at the stick's root |
| Temperatures | CPU (k10temp, coretemp; bare-metal UEFI reads the same registers directly) and memory modules with a DDR4 JC-42 or DDR5 SPD hub sensor on the chipset SMBus; RAM temp shows only when a sensor answers |
| Displays | Bare-metal UEFI draws on every graphics output, 1920×1080 or the current mode if larger, centred at the smallest screen's size; the firmware text console only when none works |
| Crashes | A kernel panic reboots after 30 s; its log is saved as `memburn-crash-<date>.txt` at the stick's root and the menu then waits for a key |

On the stick, R restarts and P powers off from the menu or result screen.

### Updates

With a network the stick's Linux entry asks the live-view server for the latest release at start
(3 s per try, retried for 10 s; no answer, no update). A newer release whose manifest carries a valid
signature of the release key adds a row `U  Update to <version>` to the menu and the result screen;
without U the 10 s countdown starts the quick check as usual. `update=auto` in `memburn.cfg` installs it before an unattended run.

| | |
|---|---|
| Manifest | version, the stick's files as one tar (URL, size, SHA-256), and each file's path, size and SHA-256; signed with Ed25519, public key in `proto/src/release.rs` |
| Install | downloads the tar, checks its hash and every file's, writes each file as `<file>.new` beside the old one with fsync, renames them into place with `grub.cfg` last, syncs and restarts |
| Layout | `EFI/memburn/<version>/` holds each version's `vmlinuz`, `initrd.img` and `memburn.efi`; GRUB boots the new version by default and keeps `memburn <old> (previous)` entries; older versions are deleted |
| Never written | `memburn.cfg`, `memburn.log`, `memburn-history.txt`, `memburn-crash-*.txt` |
| Refused | a bad signature, a version not newer than the running one and the stick's, a hash mismatch, a short download; the stick stays unchanged |
| Too little space | the row reads `Update <version>  re-flash needed` and offers no key |

## Windows / Linux

```sh
cargo build --release                                     # Linux
cargo build --release --target x86_64-pc-windows-gnu      # Windows exe
memburn                                                   # dashboard and menu
memburn -m quick --plain                                  # one run, plain lines
```

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

Exit codes: 0 pass, 1 errors found, 2 usage, 3 stopped early; on the stick 10 restart, 11 power off.
The OS keeps some RAM, so only the stick tests nearly all memory. Under Linux the test memory asks for
huge pages; the log says how much got them. If it cannot be locked, it shrinks in 256 MiB steps down to
95% of available RAM. Locking test memory needs administrator rights (Linux: `ulimit -l`); without it
the header shows "memory not locked".

Coverage is the share of System RAM that a run tested, by physical address: the test buffer's pages
(Linux as root, from `/proc/self/pagemap`) plus the kernel's early test, or the claimed pages on
bare-metal UEFI. The stick's Linux entry boots with `memtest=2`, so the kernel tests its free memory
with two patterns before anything else runs; `early_memtest=N` in `memburn.cfg` changes the count
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
addresses, and how many earlier runs flipped the same bit. The same bit at several addresses points
to a data line (timing or voltage), not a single cell. Bits flipping only one way three or more times
are tagged as stuck. Each bit sits in byte lane bit/8 (lane 2 is bits 16-23), one of the eight
data bytes of a 64-bit channel.

The log, the summary and the live view list the memory modules from SMBIOS: slot, size, rated and
running speed, maker, part number and serial. Under Linux this needs root (the stick); Windows and
UEFI always have it. The address alone does not name the stick. To find it:

1. Note the failing bits and lanes, and which module sits in which slot.
2. Power off and swap two modules between slots (or channels).
3. Run the same test again. If the errors follow a module to its new slot, that module is faulty;
   if they stay with the slot, suspect the slot, the board or the memory controller settings.
4. With one module left to check, test it alone in a known-good slot.

Run history is a start line and a result line per run in `memburn-history.txt`: next to `--log` (the
stick's root), else `%LOCALAPPDATA%\memburn\` or `~/.local/state/memburn/`. A start line without a
result is a run that did not finish (crash, reset or power cut); the menu names the last one.
Result lines also carry `ram=` (System RAM, MiB), `host=` (a hash of the module list) and `cov=`, the
tested ranges as `<shift>:<gap>+<length>,...` in hex units of 2^shift bytes, coarsened until the
line stays under 4 KiB.

## Live view

With a network the client creates a run on the server, shows its URL and QR code, and uploads a
snapshot every 3 s. When the job ends it sends one final snapshot with the verdict and finish time
(up to 4 tries) and stops; the next job from the result screen gets a new run. The page then shows
PASS, FAIL or STOPPED and the finish time, and stops polling; a running run silent for 30 s shows
"No updates since". Runs are kept 30 days.

The live view is a hosted service at reljam.com. `--no-upload` (stick: `upload=0`) turns it off;
`--server URL` sends to another server that speaks the JSON of `memburn-proto`.


## Development

```sh
cargo test                                   # core, proto; fault injection included
scripts/run-qemu.sh                          # bare-metal UEFI build in QEMU/OVMF
scripts/run-qemu.sh tests/qemu-smoke.cfg     # headless smoke run
scripts/run-usb-qemu.sh                      # the stick image with Secure Boot and e1000e
cargo run --release -p memburn --example render -- out/   # dashboard scenes as cell dumps
```

`memburn-uefi` builds only with `--target x86_64-unknown-uefi` (`scripts/build-efi.sh`).
`inject=1` in a cfg (or `--inject` on the CLI) injects memory and CPU faults to check reporting.
`crash_after=N` in a stick cfg panics the kernel N s into the first run, to check crash logs.
`scripts/mkusb.sh --build-arg MEMBURN_RELEASE_KEY=<hex>` builds a stick that trusts another release
key (`scripts/release-key.sh KEY` prints it); `scripts/sign-manifest.sh` signs a manifest for it, and a
server built with the same `MEMBURN_RELEASE_KEY` serves it.

## Licence

MIT, see `LICENSE`. `THIRD-PARTY.md` lists the Rust crates and the stick's Ubuntu components (shim,
GRUB, Linux, BusyBox) with their licences; `scripts/stick-sources.sh` fetches the stick components'
exact sources, and each release attaches them.
