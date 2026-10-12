memburn USB stick

Boot this stick from the PC's boot menu (often F8, F11 or F12). The GRUB menu offers:
  memburn                  Linux; works with Secure Boot on; wired network for the live view
  memburn bare-metal UEFI  needs Secure Boot off; tests nearly all RAM; no network

Keys: Enter quick check, F full memory test, B burn-in, S stress (Linux entry), A all repeating,
Esc stop, Q larger QR code, R restart, P power off.
With a network the menu offers U to update the stick when a newer signed release exists.

With a wired network the screen shows a QR code and a reljam.com link to follow the run from a phone.
Results: on screen, in memburn.log, and per-run bit statistics in memburn-history.txt at this stick's root.
After a kernel crash the stick restarts in 30 s and saves the crash log as memburn-crash-<date>.txt.
The Linux entry first runs the kernel's own memory test (memtest=2; early_memtest=N in memburn.cfg, 0 = off).
Unattended runs: rename memburn.cfg.example to memburn.cfg and edit it.
Windows: windows\memburn.exe runs the same dashboard from a terminal (memburn.exe --help).
