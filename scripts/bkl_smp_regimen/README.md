# BKL SMP stress + attribution regimen

Harness for the campaign in `docs/archive/BKL_VFS_CARVE_OUT.md` §11: drive a multi-process
net + VFS workload on a `devbox-smoltcp` VM at SMP=N and collect (a) stability signals from the
kernel log and (b) the `[BKLPROF]` per-tag BKL-hold histogram from a `bkl-profile` build.

Re-run it after each of Phase 2b/2c/2d (the remaining VFS syscall conversions), and after any
change to the dropped-window ledger or the ext2 guards.

## Pieces

| file | role |
|---|---|
| `payload/job.sh` | the in-VM regimen (`net4` → `read4` → `cp2` → `rm`), fetched over HTTP |
| `gen_payload.py` | writes the deterministic payload + prints its reference sha256 |
| `vm.py` | run one command in the VM over ssh (see caveats below) |
| `drive.py` | stage `job.sh` into the VM, run it detached, poll it to completion |
| `analyze.py` | count stability signals + summarize `[BKLPROF]` windows from a kernel log |

## Run

```bash
# 1. payload + host server (the guest reaches the host as 10.0.2.2)
./venv/bin/python scripts/bkl_smp_regimen/gen_payload.py /tmp/bklpay
( cd /tmp/bklpay && python3 -m http.server 8899 --bind 127.0.0.1 & )

# 2. kernel. Stress verdict uses the SHIPPING config; attribution adds bkl-profile.
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile

# 3. boot (nothing else may hold devbox.img open — see below)
DISK=devbox.img MEMORY=4096 SMP=4 scripts/cargo_runner.sh \
    target/aarch64-unknown-none/release-smp-shared/akuma > run.log 2>&1 &

# 4. drive + analyze
./venv/bin/python scripts/bkl_smp_regimen/drive.py 1800
./venv/bin/python scripts/bkl_smp_regimen/analyze.py run.log
```

## Caveats that cost real debugging time

- **Nothing else may have `devbox.img` open.** QEMU takes a write lock; a leftover VM from an
  earlier session both blocks the boot and (if the image was resized meanwhile) writes through a
  stale superblock. Check with `lsof devbox.img`.
- **Grow the image, don't let it fill.** 1 GB fills up across sessions and ENOSPC masquerades as
  a network bug (`curl -s` swallows it). 4 GB via Homebrew e2fsprogs:
  `e2fsck -fp devbox.img && truncate -s 4G devbox.img && resize2fs devbox.img && e2fsck -fp devbox.img`.
  Akuma's ext2 driver handles the 32-block-group image fine. The recurring `HTREE … invalid root
  node / HTREE INDEX CLEARED` fsck complaint is cosmetic — Akuma's ext2 writes don't maintain
  htree indexes.
- **`ssh` always exits 255.** This sshd never sends exit-status; key on stdout, never `$?`.
- **Disable ssh keepalives.** They time out when the cores are pegged and the server then tears
  the channel down mid-command. Hence: run the regimen detached (`nohup sh /tmp/job.sh &`) and
  poll it over short-lived connections.
- **`sh /tmp/job.sh`, not `/tmp/job.sh`.** busybox resolves a bare executable path with no
  recognised interpreter as an *applet* name → `applet not found`.
- **Never use the shell's `wait` builtin.** It never returns on Akuma (the kernel delivers no
  SIGCHLD — that doc §11.3), so parallel phases join on sentinel files instead.
- **Every fork is expensive under load.** Thread slots are reclaimed tens of seconds after a
  process exits (that doc §11.4), so a fork can stall for minutes and eventually fail with
  `can't fork: Out of memory` while GBs are free. `job.sh` therefore avoids command
  substitution, per-item loops, and frequent `sleep`; keep it that way, and keep the poll
  interval in `drive.py` high — each poll is an ssh session, i.e. more process churn.
