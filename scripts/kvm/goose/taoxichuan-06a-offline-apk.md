# Taoxichuan stage 6, session A: run apk.static and install a package OFFLINE - finish as much syscall wiring as the quota allows

You are goose-glm, the metal track of the experiment in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen
box. Stage 4 passed and you planned stage 6 (`docs/archive/TAOXICHUAN_STAGE6_PLAN.md`, `taoxichuan-s4` @ `f61ca77e`).
The owner wants you to use what is left of this quota window (roughly 15-18% at best; it resets at 02:39 IDT and
unused quota is lost) on a variant that **needs no networking**: the supervisor has put `apk.static` and a real package
on a disk image, so every syscall apk makes while starting and unpacking gets exercised without the relay, DNS,
a mirror or TLS. You may stop at any point: a precise PARTIAL result with everything committed is a good outcome.

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps are UTC (IDT - 3 h). Read the plan's
sections 1, 2 and 5 and the M3/M4/M5 rows; do not re-derive them. You are on `glm-5.3-flash` at low effort.

## What the supervisor staged (verified, do not redo)

- `/boot/akuma-dev-root-apk.img` (16 MiB ext2, fsck-clean, 2,053 free 4 KiB blocks) and a pristine copy
  `/boot/akuma-dev-root-apk.img.orig`. It is the stage-4 root plus: `/bin/apk.static` (apk-tools-static 3.0.8-r0, mode
  0755, 5,470,888 B, **static-PIE: ELF type ET_DYN, which the loader does not support yet**) and `/bin/apk` symlink to it;
  `/pkgs/ca-certificates-bundle-20261020-r0.apk` (real, signed by Alpine, no dependencies, no install scripts);
  `/etc/apk/{arch,repositories,world}` and the three Alpine release keys in `/etc/apk/keys/`; empty dirs
  `/lib/apk/db`, `/var/cache/apk`; `/pkgs/README` with the commands.
- The stage-4 image `/boot/akuma-dev-root-ssh.img` is untouched: **never write it**.
- Independent reference, computed on the Mac with plain `tar` (not by Akuma):
  `/root/dev-cycle/taoxichuan-s6/EXPECTED.txt` - after a correct install, `/etc/ssl/certs/ca-certificates.crt` has
  sha256 `5961c0a3...3f568b` and three symlinks exist (`/etc/ssl/cert.pem`, `/etc/ssl1.1/cert.pem`,
  `/etc/ssl1.1/certs`).
- `vm-serve-mc` is running; `vm-dev` is disabled and its conf is the stage-4 one.

## Method

1. **Each boot starts from a fresh disk:** `cp /boot/akuma-dev-root-apk.img.orig /boot/akuma-dev-root-apk.img`. The
   guest will write to it once ext2 write wiring exists, so never reuse a written image.
2. Back up `/etc/herd/available/vm-dev.conf` (to `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`), then set
   `vm_disk = /boot/akuma-dev-root-apk.img`. Leave `vm_outbound` off and `vm_forward = 2224:22`. **Restore the backup
   when you finish.**
3. Boot dance, every time: `herd stop vm-serve-mc; herd enable vm-dev; herd start vm-dev`, read
   `/var/log/herd/vm-dev.log`, then `herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc`. One kvm0 VM at a
   time. **Do not leave `vm-dev` running when you finish:** the Taoxichuan guest never halts its vCPU (its sleep loop
   polls the NIC), so it pins a host core at 100% and the owner's fans spin. Never `pkill` by name; never touch
   `/boot/akuma-guest`, `/boot/akuma-dev-root-ssh.img` or `/root/.config/goose`.
4. Gates, over `/bin/ssh -p 2224 root@localhost '<cmd>'` (exec-only; the akuma client has no `-T`), in order:
   - **G1** `/bin/apk.static --version` prints a version line, exit 0. First blocker: the ET_DYN static-PIE loader
     (plan M3, loader half): accept `ET_DYN`, choose a fixed base, apply it to entry/phdr, no relocations needed for
     static-PIE except musl's self-relocation which runs in userspace. Also needed early: `fstat`, `uname` and the
     other phase-A arms; print the syscall *name* instead of the bare number in the `unhandled nr` arm first, so every
     boot reads as a checklist (and add the `UPTIME` arm, `0x1000 + 319`).
   - **G2** `/bin/apk.static --root / --initdb --no-network --no-cache --allow-untrusted add
     /pkgs/ca-certificates-bundle-20261020-r0.apk` exits 0 (phase C arms plus the ext2 create/write/rename/unlink/
     symlink wiring from plan section 2; `cargo test -p akuma-ext2` on the host first if you touch that crate).
   - **G3** `sha256sum /etc/ssl/certs/ca-certificates.crt` equals EXPECTED.txt, and `ls -l /etc/ssl/cert.pem
     /etc/ssl1.1/cert.pem /etc/ssl1.1/certs` show the three symlinks.
   - **G4** the same install on a fresh image **without** `--allow-untrusted` (apk verifies the RSA signature against
     `/etc/apk/keys`, in userspace), then `apk.static info -L ca-certificates-bundle` lists the files.
5. Zero `[Fault]`/`PANIC`/`[sig!]` lines in the guest console for a passing boot.

## Rules

- **Commit and `git push litter taoxichuan-s4` every 10 minutes whenever anything is uncommitted** (WIP is fine). All
  `asm!`/port I/O/MSR code stays behind `#[cfg(target_arch = "x86_64")]`; do not add an amd64-only crate to the root
  `Cargo.toml` `default-members`; shared crates you touch need gated, host-tested changes. Never push to `origin`; do not
  touch the dirty submodules.
- Stop when a gate fails in a way you cannot see past in two boots, or the quota error appears, or you have used about
  25 minutes. Then write `/root/dev-cycle/taoxichuan-s6/RESULT-A.md`: which gates passed, the exact failing step with
  evidence (console lines), the syscalls added (names), the list still missing, and what you would do next. Commit,
  push to litter, restore `vm-dev.conf` and `vm-serve-mc`, end the turn.
