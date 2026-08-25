# Reboot into a freshly self-built kernel, and the `akuma-boot` crate

**Date: 2026-08-25.** Motivated by wanting to develop kernel features *inside*
Akuma itself: self-host a build (`docs/runbooks/selfhost-kernel-build.md`), then
get QEMU to boot the freshly built kernel without a human re-running
`cargo run` by hand every cycle.

## The headline result

> **A plain PSCI `SYSTEM_RESET` is enough — no kexec.** QEMU's own machine
> reset already tears every core and device back down to the exact clean state
> `boot.rs` assumes, so a guest-triggered reboot needs none of a self-hosted
> kexec's hard parts (SMP park/quiesce handshake, cache/MMU teardown, a
> self-overwriting relocation stub). The only wrinkle: QEMU reads `-kernel`
> once at process startup and caches those bytes, so an in-process reset alone
> replays the *same* kernel. Fixed by pointing `-kernel` and a `virtio-blk`
> `-drive` at the **same host file** (the guest can `dd`/objcopy a rebuilt
> kernel directly onto the file QEMU will re-parse) plus `-action
> reboot=shutdown` (turns the guest's reset into a full process exit) plus a
> host-side relaunch loop. Verified working end to end at the QEMU level,
> three times over (below) — blocked short of a live guest-driven cycle only by
> a pre-existing, unrelated gap: this userspace has no `/dev` at all.

## What is where

| path | what |
|---|---|
| `crates/akuma-boot` | Linux `reboot(2)` ABI decode: magic/cmd → `Action`. Plain `#![no_std]`, zero deps, 7 host tests. Also owns the PSCI `SYSTEM_OFF`/`SYSTEM_RESET` function-ID constants (pure data, no reason to duplicate them kernel-side). |
| `src/syscall/reboot.rs` | `sys_reboot` — kernel glue with no decision logic of its own: unpacks `args[]`, calls `akuma_boot::decode`, dispatches to `smp_shared::system_reset`/`system_off` (diverging, unsafe) or returns `EINVAL`. |
| `src/smp_shared.rs` | `system_reset`/`system_off`: the actual PSCI SMC/HVC call, reusing the *existing* `psci_call` helper and `USE_HVC` conduit state that `bringup_secondaries`'s `CPU_ON` already uses — so there is still exactly one PSCI call site in the kernel, not two. |
| `scripts/cargo_runner.sh` | `KERNEL_DROPOFF=1` env var: mounts `$BIN` (the file `-kernel` loads) as a second `virtio-blk` drive on `bus.6`, adds `-action reboot=shutdown`, and — only in this mode — replaces the trailing `exec qemu-system-aarch64` with a `while true` relaunch loop (never re-runs `rust-objcopy`, which would stomp the guest's dropped-off kernel with the stale host-built ELF again). |
| `Cargo.toml` feature `sc-reboot` | Off by default; wired into `devbox`/`devbox-smoltcp` only. `nr::REBOOT = 142` (the real Linux `aarch64` syscall number) and the `mod reboot;` declaration are both `#[cfg(feature = "sc-reboot")]`, so `release`/`extreme-size` never carry the syscall. |

```bash
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p akuma-boot --target "$HOST"       # 7 tests
cargo check -p akuma-boot                       # no --target: proves it builds under
                                                 # the kernel's own ambient no_std target too
```

---

## 1. Why not kexec

The first design considered was a real warm reboot: the running kernel copies
a rebuilt image over itself and jumps to the entry point, the way Linux's
`kexec` does. Scoped and rejected before writing any code, because the starting
state is the opposite of what `boot.rs` expects: QEMU hands a fresh boot an MMU
off, caches cold, secondaries PSCI-parked, DTB pointer freshly in `x0`. A
kernel that's been running has the MMU on, dirty cache lines everywhere, N
cores mid-execution with live threads, a GIC with active/pending interrupts,
and virtio rings mid-transaction. Getting back to boot-clean from there is the
whole feature: park every secondary with an IPI, `CPU_OFF` each one (a new
SMCCC call — only `CPU_ON` exists today, in `smp_shared.rs`) so the new
kernel's own `bringup_secondaries` can `CPU_ON` them again without hitting
`ALREADY_ON`, quiesce every virtio device's status register, copy the new
image through a relocation stub because you can't `memcpy` over the code
you're currently executing, clean+invalidate the D-cache to PoC over the
copied range, invalidate the I-cache, disable the MMU, and re-enter at the
fixed boot entry with `x0` intact. Multi-day work with real regression risk on
a live, currently-relied-upon SMP bring-up path — this codebase's own history
(the BKL/SMP phases, `debug-futex-lost-wakeup.md`, the mapped-page-refcount
fixes) says exactly that class of change needs its own dedicated A/B pass
before it's trusted.

A plain PSCI `SYSTEM_RESET` sidesteps the entire list: it's a *real* hardware
reset, so QEMU does all of the above for free, the same way it does for the
very first boot.

## 2. The catch, and the fix

`-kernel <path>` is parsed once, at QEMU process startup, into QEMU's own
process memory (the generic ROM-loader machinery that also backs `-initrd`/
`-dtb`/`-bios`); an in-process machine reset re-copies that cached blob into
guest RAM, it does not re-`stat`/re-`read` the host file. So a bare
`reboot(2)` from the guest — even with `SYSTEM_RESET` wired up correctly —
reboots into the exact kernel that was already running.

Two flags close the gap:

- **The same host file as both `-kernel` and a `virtio-blk` `-drive`.** The
  guest can `dd`/objcopy a freshly self-built kernel directly onto the file
  QEMU will read on its *next* process start. Verified with no locking
  conflict at the QEMU level (§3) — the two roles open the file independently
  and don't collide.
- **`-action reboot=shutdown`.** Converts a guest-triggered reset from an
  in-process reset (replays the cached blob) into a full QEMU process exit, so
  a relaunch is forced to re-parse `-kernel` from the file's current contents.

`scripts/cargo_runner.sh`'s `KERNEL_DROPOFF=1` wires both, plus a `while true`
relaunch loop so the whole `dd` + `reboot(2)` cycle needs no human
intervention between builds — see the table above for exactly what it does and
doesn't touch.

## 3. Verification

Three separate QEMU boots, each stricter than the last, each isolated from any
already-running instance (private ports, private disk via `cp -c` — an APFS
clone, instant regardless of size):

1. **Bare smoke test**, no real rootfs: `-kernel`/`-drive` on the same file,
   no network. Booted clean through MMU/PMM/threading init;
   `[Block] Found virtio-blk at slot 1` / `[I] found a block device of size
   1916KB` matched the kernel binary's exact byte size — proves the two roles
   don't lock-conflict and the block device really does expose the same bytes
   `-kernel` loaded.
2. **Full devbox-smoltcp boot** with the drop-off drive on `bus.6` (net=0,
   hd0=1, rng=2, kdrop=6 — the DISK2 convention's bus.5 stays free). Kernel
   enumerated both drives correctly (`vda` = 6144 MB disk, `vdb` = 1 MB
   drop-off) and booted to `herd` normally.
3. **SSH into it.** `uname -a` → `Akuma akuma 0.0.7 b930217e-release-smp-shared
   aarch64 Linux`, over a real DHCP lease (`10.0.2.15/24`), through the
   userspace `sshd`.

What's *not* verified: the actual guest-side half of the loop (`dd` a rebuilt
kernel onto the drop-off device from a shell, then call `reboot(2)`). Blocked
on §5.

## 4. Traps hit along the way (none of them the feature)

Every one of these cost real time and turned out to be unrelated to the
reboot mechanism itself — recorded so the next session doesn't re-derive them.

- **`--no-default-features` is for `devbox` (rump), not `devbox-smoltcp`.**
  `smoltcp` only comes from the workspace's `default` feature set;
  `devbox-smoltcp` *layers on top of* defaults (see its own Cargo.toml
  comment and `scripts/build_devbox_smoltcp.sh`, which deliberately omits the
  flag). Building with `--no-default-features --features
  devbox-smoltcp,no-tests` compiles the network stack out entirely and
  produces a smaller-than-expected binary (1.2 MB vs. the correct ~2.0 MB).
  The kernel then does exactly the right thing with no network stack:
  `sys_bind` returns `ENETDOWN` (`"Network is down"`), 100% reproducibly,
  every single attempt — which reads exactly like a hung/broken network stack
  and is not one. `src/syscall/net.rs`'s own comment already named this
  failure mode ("`ENETDOWN` when it is compiled out"); the fix is
  `scripts/build_devbox_smoltcp.sh`, which exists precisely to prevent this.
- **`cargo build` doesn't regenerate `akuma.bin`.** Already known
  (`project_isolated_qemu_verification.md`), rediscovered here: only
  `cargo run` (via `scripts/cargo_runner.sh`) runs `rust-objcopy`. A plain
  `cargo build --release` after a feature-set change leaves the stale `.bin`
  in place with no error. `rust-objcopy -O binary <elf> <bin>` by hand is the
  fix when driving qemu directly instead of through the runner.
- **A `cp -c` clone of a disk another QEMU is actively writing can carry real,
  reproducible ext2 corruption** — `Data consistency error` on a subdirectory,
  confirmed and fixed by `e2fsck -fy -D` inside a `--privileged` Alpine
  container (the macOS host has no native ext2 driver). This did **not**
  turn out to be the cause of the `ENETDOWN` failures above (those persisted
  identically after repair) — two independent problems that happened to
  overlap in the same investigation. `e2fsck` needs re-running until it
  exits 0; `EXIT=1` means "errors were fixed", not "done"
  (`docs/runbooks/selfhost-kernel-build.md` §5.5 already documents this for
  the self-host campaign case).
- **Docker's `mount: can't setup loop device: No space left on device`
  wasn't a resource problem.** `losetup -a` showed nothing attached and 8 free
  `/dev/loopN` nodes; the real cause was a dropped `--privileged` flag on the
  retry after a Docker Desktop restart. `docker system df`/`image prune`/
  `builder prune` are red herrings for this specific error text.
- **`/etc/herd/enabled/sshd.conf`'s `start_delay_ms = 10000` is rump-specific
  tuning** (waits out `rump_server`'s DHCP handshake, per its own comment),
  living in the ONE herd config file shared by both `run.sh` (rump) and
  `run-smoltcp.sh` (smoltcp) via the same `devbox.img`. Under smoltcp the
  network is already up synchronously — `[Main] Network Initialization Done`
  prints before `herd` is even started — so the delay was pure dead time on
  every boot. Set to `0` in
  `overlays/devbox/rootfs/etc/herd/enabled/sshd.conf`, with a comment noting
  the rump case needs it restored if this file is ever pressed into service
  there again (see `bootstrap/etc/herd/core2/sshd-rump.conf`'s
  `start_delay_ms = 10000` for that value). `start_delay_ms` only applies to a
  service's first start (`userspace/herd/src/main.rs`); `restart=true`
  respawns after a crash with no re-applied delay, which is why the
  `ENETDOWN` crash-loop above never had a 10 s gap between attempts after the
  first.

## 5. Open: no `/dev` at all

> **Half closed 2026-08-25, and re-validated the same day.** The "no `/dev` at
> all" premise below is **no longer true** — `/dev` now lists and `stat`s, so
> the devtmpfs-equivalent half of this section's two options is done
> (`DEVFS_MISSING.md`, [`../reference/subsystems/vfs.md`](../reference/subsystems/vfs.md)
> "/dev"). **The loop is still blocked**, on the *other* half.
>
> Measured on a `devbox-smoltcp` boot with `KERNEL_DROPOFF=1` (`INSTANCE=1`,
> `DISK=devbox.img`, ssh `:2322`). The kernel enumerates both drives —
> `[Block] Registered vda: 6144 MB` and `[Block] Registered vdb: 1 MB`, the
> drop-off drive — and in-guest:
>
> ```
> # ls -l /dev
> brw-rw----  1 root root  254,  0  vda
> brw-rw----  1 root root  254, 16  vdb        <- the drop-off drive, now visible
> # stat /dev/vdb
>   Size: 0   Blocks: 0   IO Block: 4096   block special file
>   Device type: fe,10   Inode: 33   Access: (0660/brw-rw----)
> # dd if=/dev/zero of=/dev/vdb bs=512 count=1
> dd: can't open '/dev/vdb': No such device      <- rc=1
> # dd if=/dev/vdb of=/dev/null bs=512 count=1
> dd: can't open '/dev/vdb': No such device      <- rc=1, reads fail too
> ```
>
> So the remaining blocker is now exact and singular: **`open()` on a block
> device node returns `ENODEV`**, by deliberate design —
> `DEVFS_MISSING.md` §4 ruled a raw block fd out of scope for having "no
> consumer today", and `sys_openat` refuses those nodes explicitly so the
> generic path cannot hand out a `File` fd that fails on first `read()`.
> **This loop is that consumer**, which retires §4's rationale.
>
> What it takes to finish, all of it contained (the block layer already
> exposes byte-granular, arbitrary-offset `read_bytes_at(idx, off, buf)` /
> `write_bytes_at(idx, off, buf)`, so no sector-alignment work lands on the
> caller):
>
> 1. A `FileDescriptor::BlockDev { idx, pos }` variant — it needs a position,
>    since `dd` reads and writes sequentially.
> 2. `sys_openat`: for a block `dev_node`, resolve the name through
>    `block::device_index_by_name` and allocate that fd instead of returning
>    `ENODEV`.
> 3. `sys_read` / `sys_write` arms at the fd's position, advancing it and
>    clamping at `capacity_bytes()`; `sys_lseek` for `SEEK_END` sizing.
> 4. `fstat` on the fd, and a name for it in `/proc/<pid>/fd` (`vfs/proc.rs`).
>
> One design question to settle first, which is *not* mechanical: a raw write
> to `vda` goes behind `Ext2Filesystem`'s cache, leaving it stale against the
> disk. Harmless for `vdb` (never mounted), silently corrupting for a mounted
> device. Either refuse `O_WRONLY`/`O_RDWR` on a mounted device or invalidate
> the cache on write — decide before wiring step 3.
>
> The original text follows verbatim; note its claim that `mount /dev/vdb /mnt
> -t ext2` "does not actually work today" is about the *path* not existing.
> `mount(2)` itself resolves its source by name, never by path
> (`DEVFS_MISSING.md` §0), so multi-disk mounting worked all along.

`ls /dev` on a live devbox-smoltcp SSH session returns "No such file or
directory" — the directory doesn't exist, and creating it (`mkdir -p /dev`)
doesn't populate it: no `/sys`, no `mdev`, no devtmpfs, and grepping
`src/`, `crates/`, and `userspace/` for `devtmpfs`/`mknod`/anything that would
populate `/dev` on boot turns up nothing. `scripts/cargo_runner.sh`'s own
`DISK2` doc comment ("mount it in-guest with `mkdir /mnt && mount /dev/vdb
/mnt -t ext2`") describes a workflow that does not actually work today — there
is no `/dev/vdX` path a userspace process can open, by any name, on any
virtio-blk device including `vda` (the root disk) itself.

This blocks the guest-side half of the `KERNEL_DROPOFF` loop: nothing in
userspace can currently `dd` onto the drop-off drive, because nothing in
userspace can open it. Closing this needs either a minimal devtmpfs-equivalent
(kernel populates `/dev/vdN` nodes for every registered `virtio-blk` at boot)
or a dedicated syscall path for raw block access — scoped as a follow-up, not
attempted here.

## Background

- `docs/runbooks/selfhost-kernel-build.md` — the self-host compile workflow
  this feature is meant to close the loop on; its "Verify" section is the
  proven-but-manual host-side extract+relaunch this feature replaces for
  iteration (still the right recipe for a one-off pull).
- `docs/reference/subsystems/boot.md` — the boot sequence `system_reset`
  relies on QEMU replaying exactly, on every reset.
- `docs/archive/AKUMA_SCHEDULING_EXTRACTION.md` — the extraction precedent
  this session's `akuma-scheduler` → `no_std` conversion follows (unrelated to
  reboot; done in the same session because the pre-commit hook's
  `default-members` coverage came up while reviewing `akuma-boot`'s own
  wiring). Its CLI report (`--bin sched-sim`) stayed `std` and is gated by
  `required-features = ["cli"]` rather than split into a second crate — Cargo
  skips an unmet-`required-features` bin target even under the workspace's
  ambient no_std default target, which is simpler than duplicating dependency
  wiring across two `Cargo.toml`s for one bin.
