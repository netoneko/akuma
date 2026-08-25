# Raw block-device fd — the last piece of the `KERNEL_DROPOFF` loop

**Date:** 2026-08-25
**Status:** **Implemented and verified end-to-end.** `open()`/`read()`/`write()`/
`lseek()`/`fstat()` on `/dev/vdX` all work now; the full `KERNEL_DROPOFF` cycle
(guest `dd`s a kernel onto the drop-off drive, `reboot(2)`, QEMU relaunches with
it) was driven live in QEMU, not just built.
**Scope:** made `open("/dev/vdX")` return a working fd, so a self-hosted kernel
build can `dd` itself onto the drop-off drive and `reboot(2)` into it.

---

## 0. Where this sits

Two halves were needed to unblock the guest-side `KERNEL_DROPOFF` cycle
(`docs/archive/AKUMA_BOOT_EXTRACTION.md` §5 named both). Both are done now:

| Half | State |
|---|---|
| Device nodes exist — `/dev` lists, `stat`s, reports `b`/`c` and major:minor | Done 2026-08-25 (`docs/archive/DEVFS_MISSING.md`, `docs/reference/subsystems/vfs.md` "/dev") |
| `open()` on a block node returns a usable fd | **Done 2026-08-25 — this document** |

`DEVFS_MISSING.md` §4 explicitly ruled a raw block fd out of scope, on the
grounds that "a raw block fd has no consumer today". That rationale is now
retired: this loop is the consumer. Nothing else about §4 changes — `mount(2)`
still resolves its source by name, never by fd.

## 1. The blocker, as measured before the fix

`devbox-smoltcp`, `KERNEL_DROPOFF=1`, `INSTANCE=1`, `DISK=devbox.img`. Kernel
enumerated both drives (`[Block] Registered vda: 6144 MB`, `[Block] Registered
vdb: 1 MB` — the drop-off drive). In-guest, before this fix:

```
# ls -l /dev
brw-rw----  1 root root  254,  0  vda
brw-rw----  1 root root  254, 16  vdb          <- visible since the devfs work
# dd if=/dev/zero of=/dev/vdb bs=512 count=1
dd: can't open '/dev/vdb': No such device      rc=1
# dd if=/dev/vdb of=/dev/null bs=512 count=1
dd: can't open '/dev/vdb': No such device      rc=1   <- reads fail too
```

`ENODEV` was deliberate: `sys_openat` refused table nodes with no `open()`
handler, because `crate::fs::exists` said the path existed and the generic
path would otherwise have handed out a `File` fd whose first `read()` fails
against ext2 — an error at the wrong syscall.

## 2. What shipped

**`crates/akuma-exec/src/process/types.rs`** — a new `FileDescriptor` variant:

```rust
BlockDev { idx: u32, pos: u64, writable: bool }
```

`idx` is the `akuma_virtio::block` device index, `pos` is the byte offset
`read`/`write`/`lseek` advance, `writable` is fixed at open time (an
`O_RDONLY` fd can never be upgraded later).

**`src/syscall/fs.rs`** — the old unconditional `ENODEV` refusal for block
nodes is now a two-way branch: `dev_node(&path).is_block` resolves the device
through `crate::block::device_index_by_name`, applies §3's mounted-device
check, and allocates a `BlockDev` fd. Every non-block table node (the
chardevs with no fd behavior — `/dev/net/tap0` already handled above them)
still gets `ENODEV`, unchanged. `sys_read`, `sys_write`, `sys_lseek` and
`sys_fstat` each gained a `BlockDev` arm:

- `read`/`write` clamp to `capacity_bytes()` — a read past the end returns 0
  (EOF), a write past the end returns `ENOSPC`; both run under the same
  `VfsBklGuard` the `File` arms use, since it's the same disk I/O
  `crate::vfs::ext2` already drives BKL-free.
- `lseek(SEEK_END)` asks the block driver's `capacity_bytes()` directly
  (`DevNode` carries no size field, only the block driver does).
- `fstat` reuses the `dev_node` lookup `newfstatat` already had for
  mode/major/minor/ino, and adds `st_size`/`st_blocks` from `capacity_bytes()`.

**`src/vfs/mod.rs`** — `device_is_mounted(name)`: scans the global mount
table's recorded sources (`MountSet::for_each_mount`, no allocation),
stripping an optional `/dev/` prefix. Root mounts with source `/dev/vda`
(`src/fs.rs`), so this sees it. Only the global table needs scanning — block
nodes are invisible inside a box (`DevProbe::in_box` empties the table before
a box process ever reaches `dev_node`), so a raw block open never reaches this
check from a namespace mount.

**`src/vfs/proc.rs`** — `fd_description` gained a `BlockDev` arm so
`/proc/<pid>/fd/<n>` resolves to `/dev/vdX` (falling back to `blockdev:[idx]`
if the device somehow no longer names, which cannot happen in practice since
`idx` only ever comes from a successful `device_index_by_name` at open time).

## 3. The one decision that was not mechanical — settled as recommended

**A raw write to a *mounted* device goes behind `Ext2Filesystem`'s cache.**
`vda` is mounted at `/`; a `dd` onto it would leave the kernel's cached blocks
disagreeing with the disk — silent corruption. **Shipped as recommended:**
write-open of a mounted device is refused with `EBUSY`, checked once at
`sys_openat` time (`device_is_mounted`), not on every `write()`. `vdb` is
never mounted, so the drop-off loop is unaffected; the rootfs is protected by
construction. Reads are unrestricted on every device, mounted or not.

## 4. Verified live, not just built

Boot: `devbox-smoltcp`, `KERNEL_DROPOFF=1 INSTANCE=1 DEVBOX_DISK=devbox.img
overlays/devbox/run-smoltcp.sh`, ssh `:2322`.

**Read/write round-trip on the unmounted drive:**

```
# dd if=/dev/urandom of=/tmp/randchunk bs=512 count=4 && md5sum /tmp/randchunk
211bb9b1e03b62129335e27ad650a1c6  /tmp/randchunk
# dd if=/tmp/randchunk of=/dev/vdb bs=512 count=4
4+0 records out
# dd if=/dev/vdb of=/tmp/vdb_readback bs=512 count=4 && md5sum /tmp/vdb_readback
211bb9b1e03b62129335e27ad650a1c6  /tmp/vdb_readback
```

**Mounted-device write refusal:**

```
# dd if=/dev/zero of=/dev/vda bs=512 count=1
dd: can't open '/dev/vda': Resource busy
```

**The full drop-off loop, reboot included:** with a freshly-built
`akuma.bin` sitting at `target/aarch64-unknown-none/release/akuma.bin`
(already the content of `/dev/vdb` under `KERNEL_DROPOFF`), `reboot -f` from
inside the guest produced, on the host side:

```
[cargo_runner] qemu exited rc=0 — relaunching with the current .../akuma.bin (KERNEL_DROPOFF loop; Ctrl-C to stop)
Akuma Kernel starting...
...
[Block] Registered vdb: 3 MB (6401 sectors)
[herd] Started sshd
```

— QEMU exited cleanly (`-action reboot=shutdown`), `cargo_runner.sh`'s loop
relaunched with the same file, and the new instance booted clean and answered
ssh. This is the mechanism the self-host build closes the loop with; see
`docs/runbooks/selfhost-kernel-build.md` § "Swap the running kernel in place"
for the guest-side procedure.

Two operational notes surfaced only by actually running the loop:

- **`busybox reboot` (no flag) fails `EPERM`** — it tries to signal init
  first, which this kernel doesn't support the way it expects. **Use
  `reboot -f`**, which calls `reboot(2)` directly. Nothing to fix here; this
  is a busybox/init-model mismatch, not a kernel bug.
- **`KERNEL_DROPOFF=1`'s drop-off drive is `akuma.bin` itself, live, not
  snapshotted.** A raw write test against `/dev/vdb` *is* a write to the host
  file QEMU will `-kernel` boot next. Back it up, or be ready to rebuild it
  (`cargo build --release`, then `rust-objcopy -O binary <elf> <elf>.bin`,
  matching what `scripts/cargo_runner.sh` does unconditionally on every
  `cargo run`) before trusting the next relaunch.

## 5. Unrelated blocker hit and fixed along the way

Verifying this required `overlays/devbox/run-smoltcp.sh`, which does a bare
`cargo run --release --features ...`. That now fails workspace-wide:

```
error: `cargo run` could not determine which binary to run.
available binaries: akuma, sched-sim
```

`crates/akuma-scheduler`'s `sched-sim` CLI binary sits in `default-members`
alongside the root `akuma` package (`docs/README.md`'s own layout notes), and
cargo refuses to guess between two candidate binaries in a `cargo run` with no
`--bin`. Every `overlays/devbox/run*.sh` script, and the bare `cargo run
--release` CLAUDE.md documents as the primary dev loop, hit this. Fixed with
one line in the root `Cargo.toml`:

```toml
[package]
default-run = "akuma"
```

This disambiguates `cargo run` without touching `cargo build`/`cargo test`
(which never needed disambiguation — they build everything). Unrelated to the
raw block fd work, but it blocked verifying it, so it's fixed in the same
change.

## 6. Out of scope, still

- Partitions (`vda1`). Minors are spaced by 16 to leave room; nothing parses a
  partition table.
- `BLKGETSIZE64` and the rest of the block ioctls. `lseek(SEEK_END)` covers the
  one thing `dd` needs.
- `mknod(2)`, hotplug, `/sys`. Unchanged from `DEVFS_MISSING.md` §4.
- Invalidating `Ext2Filesystem`'s cache on a raw write instead of refusing it.
  Would need a device→`Ext2Filesystem` back-reference that doesn't exist and
  its own invalidation-granularity design; `EBUSY` was sufficient for the one
  real consumer (§3).

## Background

- `docs/archive/AKUMA_BOOT_EXTRACTION.md` — the reboot/PSCI work and the
  `KERNEL_DROPOFF` wiring; §5 carries the blocker this doc closes.
- `docs/archive/DEVFS_MISSING.md` — the device table; §4 is the scope call this
  work reverses, and only for block devices.
- `docs/reference/subsystems/vfs.md` "/dev" — current state of the table,
  including the raw block fd.
- `docs/runbooks/selfhost-kernel-build.md` — the in-guest build that produces
  the kernel being dropped off, and § "Swap the running kernel in place" for
  the end-to-end procedure this fd makes possible.
