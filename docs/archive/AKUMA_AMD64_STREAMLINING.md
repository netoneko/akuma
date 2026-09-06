# amd64: streamlining candidates, and the road to self-hosting

**Date:** 2026-09-06
**Scope:** a read of all 18,458 lines of `amd64/src/` plus `amd64/Cargo.toml`,
taken at the point where the USB/xHCI disk work is nearly done
(`docs/archive/AKUMA_AMD64_USB_XHCI.md`, memory note *amd64 persistent disk =
USB/xHCI*).
**Status:** survey. §4 is the one piece done — `crates/akuma-dmesg` is wired
into both kernels (amd64 adopted it 2026-09-06, replacing the `static mut`);
what is left there is `syslog(2)` on AArch64. Nothing else here is fixed yet.

The question this answers is two questions that turned out to overlap:

1. Once the disk lands, what in `amd64/` is duplicated, obsolete, or about to
   become wrong?
2. What actually gets this target to **self-hosting** — `cargo`+`rustc`
   building the kernel on amd64, the way `scripts/run_selfhost_kernelbuild.py`
   does on AArch64?

Three items are on both lists. Those are the ones to do first.

---

## 0. The shape of the target today

```
   3513  usermode.rs      ring-3 entry, syscall dispatch, fork/exec/spawn, 802 lines of selftests
   2237  fd.rs            descriptor table + the file syscalls
   1323  xhci.rs          USB 3.0 controller, enumeration, BOT/SCSI, block facade
    951  sched.rs         context switch + round-robin across cores
    933  smp.rs           BKL, secondary bring-up, per-CPU
    926  net.rs           NetRuntime hooks, netpoll daemon, RNG
    833  idt.rs           traps
    761  multiboot2.rs    the GRUB/bare-metal boot path (kmain_mb2)
    692  lapic.rs
    660  paging.rs
    630  loader.rs        ELF -> address space
    486  main.rs          the PVH/VMM boot path (kmain)
    ...
  18458  total, 246 `unsafe` occurrences, ~2068 lines of co-located selftests
```

35 crates are consumed, and the manifest's per-dependency comments are unusually
good — most of the "why not the shared crate" questions are already answered in
place. What follows is what those comments *don't* cover, or where the answer
they gave has expired.

---

## 1. The two boot paths have already diverged

`kmain` (`amd64/src/main.rs:143-439`, ~296 lines) and `kmain_mb2`
(`amd64/src/multiboot2.rs:207-582`, ~375 lines). The second one's own header
says:

> Deliberately parallel to [`crate::kmain`], and in the same order, because that
> order is load-bearing and documented there.

They are no longer parallel. Measured differences in the *spine*:

| step | `kmain` | `kmain_mb2` |
|---|---|---|
| physical memory | `mem::init` | `mem::init_reserving` |
| machine facts | `machine::describe` + `machine::report` | `machine_from(info)` |
| block device | `blk::init(devices)` | `try_usb_root()` / `RamDisk` |
| mount | `fs::mount_root()` | `fs::mount_root_on(dev, name)` |
| network | `net::init(dhcp)` | `net::init_bare_metal(cmdline)` |
| LAPIC / SMP | after the first suite | before the first suite |

And in the **self-test registration**, which each path spells out longhand:

- `kmain` registers ~40 entries: `mem`, `paging`, `idt`, `idt::user_copy`,
  `uaccess`, `lapic`, `sched`, `reboot`, `pci`, `xhci`, `blk`, `fs`, `fd`, `mm`,
  `net`, `sock`, `usermode::smoke`, `preempt`, `smp`, `smp_parallel`, `elf`,
  `fdprobe`, `spawn`, `console_notify`, `busybox`, `execve`, `fork`,
  `netpoll_drain`, …
- `kmain_mb2` registers a **different subset in a different order** — `pci` and
  `reboot` before `idt`, and it does not reach several of the userspace tests at
  all.

That is the machine that manufactures "works under QEMU, broken on the HP box".
The disk makes it worse, because root-on-`sda1` is the *bare-metal* path and
root-on-`vda` is the *VMM* path, so the two mount sequences stop being
cosmetically different and start being the thing under test.

**Fix.** One `boot::sequence(facts: &MachineFacts) -> !` in `amd64/src/`, with
the two entry points reduced to "collect the facts, call it". One
`selftest::register_all(&mut suite, caps: Caps)` where `Caps` says what this
machine has (disk / net / framebuffer / SMP), so a test is skipped by a
capability bit rather than by being absent from one of two hand-maintained
lists. Not a crate — this is arch- *and* target-specific glue, and the tree's
rule (`REDUCING_PLATFORM_DEPENDENCY.md` §7) argues against a `trait Arch` to
hold it.

**Value:** deletes ~250 lines, and turns the QEMU/metal divergence from a
silent property of two functions into one `Caps` value.

---

## 2. `fd.rs` caches whole files — the disk is what invalidates that

`Entry.data: Vec<u8>` (`amd64/src/fd.rs:135`) holds the **entire file** in
kernel heap for the life of the descriptor. `sys_read`/`sys_lseek` work on that
buffer; `sys_write_file` appends to it and `sys_close` is what finally persists
it (`amd64/src/fs.rs` header, 2026-09-04).

The module header states this as a considered divergence, and it was right at
the time:

> `open` reads the whole file through `fs::read_file` and holds the bytes
> alongside the descriptor. … Doing that here needs the `VfsHooks` plumbing that
> lives in `akuma-exec`, which does not build for this target — so this is a
> stated divergence, not an oversight, and the cost is that a file occupies its
> own size in kernel heap while open.

Against a ramdisk that cost was invisible. Against a real `sda1` it is three
separate problems:

1. **A full-file memcpy on every `open`.** `apk` opening a 40 MB `.apk`, `rustc`
   opening a `.rlib`, `cp` on anything large — each one is a heap allocation of
   the file's size plus a read of the whole thing before the first byte is
   returned.
2. **Writes are lost on abnormal exit.** The flush is in `sys_close`.
   `close_owned_by` (`fd.rs:933`) closes a dead task's descriptors, which
   presumably carries the flush — but a `panic = "abort"` kill, or the box
   losing power, drops every byte written since `open`. On a ramdisk nothing
   survived a reboot anyway. On a persistent disk this is data loss.
3. **`MAX_OPEN: usize = 64`** (`fd.rs:127`) is a cap on *concurrently cached
   files*, not just on descriptors, so the ceiling is really "64 files' worth of
   heap".

**Fix.** Read and write by inode through `akuma-ext2`'s own block cache — what
the AArch64 kernel does. `KernelFile` already addresses a file by
`(mount_id, inode)` and `fd.rs` already uses that type; what it passes is inode
`0` ("read by path"). The work is exposing inode-level read/write from
`fs::with_root` rather than the current `fs::read_file`/`write_file` pair.

**Value:** deletes ~200 lines of caching plus the `install_synthetic_file`
special-casing (`fd.rs:1844`), makes `open` O(1) in memory, and removes a
data-loss window that only exists because the disk was never real.

**This is also self-host blocker #5.** See §11.

---

## 3. `encode_stat` hand-rolls 144 bytes of offsets with no test

`amd64/src/fd.rs:1358-1390` builds `struct stat` from nine named offset
constants and `copy_from_slice`. `crates/akuma-syscalls-linux/src/stat.rs`
exists for exactly this and opens with:

> a wrong offset here does not crash — it makes `ls` print the wrong size, or
> `apk` decide a file is a directory. That is the failure mode this crate exists
> for: invisible at the call site, invisible in a boot log, and caught in a
> millisecond by an `offset_of!` assertion.

amd64 cannot use it as written: that `Stat` is the aarch64 `asm-generic` layout,
128 bytes. x86_64's is 144 with a different field order — which is why the local
copy exists, and why it is the *more* dangerous of the two: the target with the
unusual layout is the one with no assertions.

**Fix.** Add `stat::x86_64::Stat` to `akuma-syscalls-linux` with the same
`offset_of!` assertions the aarch64 one carries, and have `fd.rs` fill it by
field name.

**Value:** small, mechanical, and it is the only remaining place in the tree
where a `struct stat` offset is a bare literal.

---

## 4. The `dmesg` ring is a `static mut` with untested arithmetic — **done on amd64**

`amd64/src/serial.rs:62`:

```rust
const KLOG_CAP: usize = 64 * 1024;
static mut KLOG: [u8; KLOG_CAP] = [0; KLOG_CAP];
static KLOG_LEN: AtomicU64 = AtomicU64::new(0);
```

with `klog_push`, `klog_snapshot_from(skip, out)`, `klog_len`, `klog_clear`,
`klog_only`, `klog_only_dec` around it, backing `sys_syslog` (x86_64 103) so
`busybox dmesg` works over ssh on a box whose console is a write-only
television.

Three things are wrong with it living there:

1. **The wrap/skip arithmetic is the whole point and is untested.** It has
   already been wrong once in a way that mattered: `sys_syslog`'s own comment
   records that a `min(4096)` single-pass read made a 4 KiB staging buffer the
   hard ceiling on `dmesg` while `SIZE_BUFFER` advertised the full 64 KiB — so
   fifteen sixteenths of every boot log was unreachable, silently, on the one
   machine with no scrollback. That is a pure-function bug that a host test
   catches in a millisecond.
2. **It is `static mut` plus raw-pointer arithmetic** in edition 2024, in a
   tree whose stated direction is `#![forbid(unsafe_code)]` across `src/`.
3. **AArch64 has no `dmesg` ring at all.** `akuma_kernel_core::klog` is a
   different thing entirely — a `log` crate sink — and there is no
   `syslog(2)`/`SYSLOG_ACTION_READ_ALL` anywhere on that side. Every kernel
   diagnostic older than the serial scrollback is simply gone.

**Fix — the capture half is done, 2026-09-06.** `crates/akuma-dmesg`: the ring
as a pure, `#![forbid(unsafe_code)]`, dependency-free `Ring<const CAP: usize>`
plus the `syslog(2)` action decode. **20 host tests.**

Wired into the **AArch64** kernel first, since that is the side with no history
at all:

- `akuma_kernel_core::console::emit` — the one choke point every console byte
  passes — tees into a `static DMESG: Spinlock<Ring<DMESG_CAP>>`, and
  `console::dmesg_{len,snapshot_from,clear,total}` are the read side.
- The acquire is **`try_lock`, and a failure drops the bytes**. `emit` already
  runs inside `with_irqs_disabled` and, under `kernel_console_lock`, inside a
  cross-core `Spinlock` with a reentrancy guard. A blocking acquire would add a
  second lock to the one path that must work when everything else is broken: a
  panic landing while this core is inside `emit` would spin on a lock this core
  itself holds and wedge the kernel with no output — exactly what `CONSOLE_OWNER`
  exists to prevent for the first lock. History is best-effort; the live write
  is not.
- `src/tests.rs::test_dmesg_ring_captured_boot` proves the tee is *wired*, which
  no host test can: it prints a marker, drains the ring the way a `syslog(2)`
  reader would (bounded staging buffer, advancing `skip`), and finds it.
  Measured on a real boot: **`captured 5596 bytes, total 5596 - PASS`**, inside a
  `MEMORY=2048M` run that reported `Memory Tests: ALL PASSED` /
  `Threading Tests: ALL PASSED` and 165 `Result: PASS` + 97 `[PASS]`. (The one
  `[FAIL]`, `retired_reclaim_ab`, is a known clean-tree failure unrelated to
  this.)

**amd64 adopted it, 2026-09-06.** `serial.rs`'s `static mut KLOG` / `KLOG_LEN`
and the four raw-pointer helpers are gone: the storage is now
`static KLOG: Spinlock<Ring<64 * 1024>>`, and the six `klog_*` functions are
thin wrappers whose bodies are one `Ring` call each. `sys_syslog` decodes
through `akuma_dmesg::Action` rather than six local `const u64`s, so the two
kernels cannot drift on the action numbers. The public signatures did not
change, so `net.rs`'s memory ticker and `usermode.rs`'s drain loop are
untouched.

The lock discipline differs from AArch64's on the read side, deliberately.
`putb_raw` already runs under `serial.rs`'s best-effort `LOCK`, but that lock is
*best-effort* — a core that cannot take it within `LOCK_BUDGET` prints anyway —
so the ring needs its own, and the write path takes it with `try_lock` for the
same reason `emit` does: a fault landing mid-`putb_raw` must not spin on a lock
this core holds. Readers (`klog_len`, `klog_snapshot_from`, `klog_clear`)
instead retry, bounded by the same budget. A `try_lock` there could return a
spurious `0`, which would break `sys_syslog`'s `while done < want` drain loop
and truncate `dmesg` silently — the exact failure mode this whole section is
about, reintroduced through the lock instead of the arithmetic.

Verified on QEMU `-M microvm`, `INIT=/bin/sshd` (2026-09-06):

- `240 passed, 0 failed` self-tests, `all self-tests passed`.
- `busybox dmesg` over ssh returned **16805 bytes** on a fresh boot and
  **52764** on a longer-lived one — four and thirteen staging chunks, so the
  multi-chunk drain is exercised, not just claimed.
- The returned bytes are **byte-identical to the host's serial capture** for the
  first 12000 bytes after `Akuma/amd64`, which is what proves both the tee and
  the CRLF handling.
- `dmesg -c` (`READ_CLEAR`) returned the log and emptied the ring; the next
  `dmesg` returned 448 bytes and the one after 896, so the ring refills.

**Still to do:** `syslog(2)` on AArch64. It has no `nr::SYSLOG` (asm-generic
116) and no dispatch arm, so `busybox dmesg` cannot yet reach the ring that is
now filling on that side.

**On the name.** The crate is `akuma-dmesg`, not `akuma-klog`, because
`akuma_kernel_core::klog` already exists and is the `log`-facade sink. Two
things called `klog` in one tree, one a ring buffer and one a logger, is the
kind of collision that costs somebody an afternoon.

**"Optional" is `CAP = 0`.** A build that does not want to spend 64 KiB of
`.bss` selects `Ring<0>` by type rather than by `cfg`-ing out every call site,
so the disabled build still type-checks the code it is not keeping. On AArch64
that selection is the existing `kernel_profile_extreme` cfg — no new feature
flag. Two things are pinned by tests rather than by prose:

- `Ring<0>` is **8 bytes, not zero** — the `u64` counter stays; only the buffer
  goes. The first draft of this section claimed "zero-sized", which was wrong,
  and `zero_capacity_ring_costs_only_the_counter` is why it stayed wrong for
  about four minutes.
- `CAP == 0` is guarded explicitly in `push`/`snapshot_from`, because `% CAP`
  there is a division by zero and the guard is not obvious from reading either.

---

## 4b. `ps` showed nothing, and `/proc` was a real empty directory — **done**

`busybox ps` printed its header and no rows, on both a serial console and over
ssh, with **exit status 0 and no error anywhere**. `ls /proc` printed nothing;
`top` said `can't change directory to '/proc': Function not implemented`.

The cause was not a missing `/proc`. `/proc` **exists on the image as a real,
empty ext2 directory** (`mkdisk.sh`, put there so `busybox reboot` can find
init), so `openat` resolved it, `getdents64` succeeded, and it returned zero
entries. `ps` iterated nothing and stopped. Every layer reported success.

**Fix, 2026-09-06 — `crates/akuma-procfs`, shared with the AArch64 kernel.**

The AArch64 side already had a full `ProcFilesystem` (`akuma-vfs-glue/src/proc.rs`,
1547 lines). It cannot be reused here and cannot even be compiled for
`x86_64-unknown-none`: it reaches `akuma-exec`/`akuma-elf`, which are written
against `akuma-mmu`'s `UserAddressSpace` — a type that is itself
`#[cfg(target_arch = "aarch64")]`. So the **filesystem** stayed where it is and
the **formats** moved out, which is the half where a divergence is invisible.
Same seam, same reason, as `akuma-syscalls-net`.

`akuma-procfs` is `no_std`, `#![forbid(unsafe_code)]`, depends only on
`akuma-primitives` (for `FmtBuf`), and holds `ProcStat`/`ProcState` plus
`render_pid_stat`/`render_status`/`render_cmdline`. **17 host tests.** Both
kernels now render through it: `akuma-vfs-glue::proc` maps its `&Process` onto
`ProcStat`, and amd64's `fd.rs` maps its `Spawn`.

Extraction paid for itself twice before it shipped:

1. **The `stat` line emitted 41 fields where its own comment listed 44.** `ps`
   counts to field 14 for `utime` and stops, so nothing downstream had noticed;
   anything reading `rss` or `policy` would have read a neighbour's value or run
   off the end. A/B against a pre-change VM: `fields=41` -> `fields=44`, with
   fields 1-20 byte-identical.
2. **`comm` truncation could panic the kernel.** It sliced `&name[..15]`, which
   panics if byte 15 is inside a multi-byte character — reachable from any
   program that names itself in UTF-8. Now trimmed to the boundary below, with a
   test.

### What amd64 needed on top of the formats

- **A retained argv.** `sys_spawn`/`sys_execve` parsed argv, handed it to the
  ELF loader (which writes it onto the child's initial stack) and dropped it.
  Reading it back out of the child's stack later is not possible — the program
  owns that memory and a shell has overwritten it by the time `ps` asks. `Spawn`
  now keeps a `CMDLINE_MAX`-bounded copy, and `execve` **replaces** it: without
  that, every process reported the name of whatever `fork`ed it and a session
  showed a column of `sh`.
- **A `ppid`.** Recorded at `fork`/`spawn` from the caller's pid.
- **Synthetic directories.** `install_synthetic_dir` pre-seeds the descriptor's
  `dir_cache`, which is the field `sys_getdents64` already consults before it
  would call `fs::read_dir` — so a synthetic directory needed **no branch in
  `getdents64` at all**, and inherits its snapshot-on-open semantics, which is
  what walking a live process table wants anyway.
- **`stat` on a `/proc` path.** `procps_scan` calls `stat("/proc/<pid>")` for
  the USER column and `continue`s on failure.

### The bug that actually blocked it, and how it was found

With all of the above in place `ps` was *still* empty. The syscall trace is what
settled it:

```
nr=2   "/proc"     -> 3          open
nr=217 fd=3        -> 0xc0       getdents64: 192 bytes, the listing is there
nr=4   "/proc/1/"  -> -2         stat: ENOENT      <-- here
nr=217 fd=3        -> 0          getdents64: end
```

**busybox stats `/proc/1/`, with a trailing slash** — it builds the directory
prefix once and reuses it with each filename appended, so the bare-directory
`stat` carries the separator. `strip_prefix("/proc/")` left `"1/"`, which split
into `("1", Some(""))` and matched no arm. `procps_scan` treats a failed `stat`
as "the process exited between the readdir and the stat" and skips it, so every
pid was skipped silently. `normalise_proc` now trims trailing slashes before
anything else, and resolves `self` in the same place.

Getting there required a **tracer fix that is worth keeping**: the syscall trace
printed paths for `open` and the `*at` family but not for `stat`/`lstat`/
`access`/`chdir`, so an ENOENT from a first-arg-path syscall was a number with
nothing attached. Those four now print their path.

`ls /proc` also listed names whose `stat` then said `No such file or directory`,
because the listing and the `open` handler had drifted. `render_proc_file` now
serves `open`, `stat` and the listing from one place, so they cannot.

### Verified on QEMU `-M microvm` (2026-09-06)

```
$ ps                          $ ls /proc
PID   USER     TIME  COMMAND  1  10  meminfo  mounts  net  self
    1 0         0:00 /bin/sshd
    8 0         0:00 ps       $ ls /proc/1
                              cmdline  fd  stat  status
```

`/proc/self/status` resolves and reports `PPid: 1`; 240/0 self-tests. On the
AArch64 side, `ps` still lists five processes and the whole boot suite is
unchanged (165 `Result: PASS`, 97 `[PASS]`, the one known-failing
`retired_reclaim_ab`).

**Still to do:** `top`, which needs `chdir` (x86_64 80, entirely unimplemented —
`getcwd` hardcodes `/`) plus the system-wide `/proc/stat` and `/proc/uptime`.
Those two files already exist as renderers on the AArch64 side and are the
natural next thing to move into `akuma-procfs`. And the USER column prints `0`
rather than `root`, which is a missing `/etc/passwd` on the image, not a kernel
gap.

---

## 5. `syscall_dispatch` is two-tier: 47 raw numbers, ~30 named

`amd64/src/usermode.rs:619-1140`, one function, 520 lines.

- Lines 735-1055: **47 arms matched on the raw x86_64 number** — `158`
  (`arch_prctl`), `99` (`sysinfo`), `103` (`syslog`), `169` (`reboot`), `13`/`14`
  (`rt_sig*`), `102`/`104`/`107`/`108` (credentials), `4`/`6` (`stat`/`lstat`),
  `61` (`wait4`), `57`/`58`/`56` (fork family), `59` (`execve`), …
- Lines 1058+: ~30 arms through `akuma_syscalls_abi::Syscall::from_x86_64`.

The comment at the seam explains the split as "x86-only, or a one-liner that
does not earn an enum variant". That was a reasonable line to draw at the time
and it has drifted: `reboot`, `sysinfo` and `wait4` are not x86-only and are not
one-liners, and the whole point of `akuma-syscalls-abi` (per its own manifest
entry: "Syscall identity across architectures … the amd64 port is why it
exists") is that a syscall's identity should be a name both kernels share.

**Fix.** Extend `Syscall` to cover the non-x86-only members of the raw set. The
genuinely x86-only ones (`arch_prctl`, `stat`, `lstat`) stay raw with the comment
they already have.

**Value:** 47 magic numbers become names; a syscall implemented on one arch and
not the other becomes a missing match arm instead of a silent `ENOSYS`.

---

## 6. `xhci.rs` is four layers in one 1323-line file

`amd64/src/xhci.rs` currently holds:

- MMIO register access (`r32`/`w32`/`w64`, the `Xhci` struct, DMA arenas)
- controller bring-up (`init`, `bios_handoff`, `halt_controller`,
  `wait_cnr_clear`, `find_and_reset_port`)
- USB device enumeration (`enumerate`, `parse_bot_endpoints`,
  `write_input_context`)
- Bulk-Only Transport + SCSI (`bot_run`, `bot_small`, `read_capacity`, `recover`)
- a block-device facade (`read_bytes`, `write_bytes`, `capacity_sectors`,
  `is_initialized`, `mbr_looks_right`)

`amd64/Cargo.toml` already refers to a file that does not exist:

> USB Mass Storage Bulk-Only Transport wire format (CBW/CSW + minimal SCSI),
> driven over `akuma-xhci` by **`src/usb_storage.rs`**'s BOT helpers in
> `src/xhci.rs`.

**Fix, once the disk works and not before.** Split at the transport boundary:
`xhci.rs` keeps the controller and exposes "do a bulk transfer on endpoint N";
`usb_storage.rs` gets BOT, SCSI and the `BlockDevice` impl. Then `fs.rs`'s
`RootDevice::Usb` talks to a block device instead of reaching into
`xhci::read_bytes` — which is what makes a *second* USB disk, or a USB disk plus
virtio, a data structure rather than a rewrite.

---

## 7. `dns.rs` is a fork of shared code carrying a live bug report

`amd64/src/dns.rs`, 295 lines, header:

> `akuma_net::dns::resolve_host_blocking` … hung: measured 2026-09-05, a call to
> `resolve_host_blocking` never returned even after 90 real seconds, with no
> timeout firing. … Nothing on amd64 had ever called `smoltcp_net::dns_query`
> before this feature (checked: zero prior call sites in `amd64/src/`), which
> makes "this specific code path has a real, unexercised bug on this target" a
> live possibility, not fixed here.

This is the honest thing to have done under time pressure, and it is still a
second DNS client in a tree that has one. The bug is in `akuma-net`, which
AArch64 also uses; nobody has hit it there because that path is exercised
differently.

**Fix.** Root-cause the `dns_query` hang. Deleting `amd64/src/dns.rs` is the
by-product; fixing a real hang in the shared stack is the point.

---

## 8. `ramdisk.rs` and `RootDevice::Ram` are about to be dead weight

`amd64/src/ramdisk.rs` (91 lines), `fs::RootDevice::Ram`
(`amd64/src/fs.rs:102`), `kmain_mb2`'s whole "an ext2 image the boot loader left
in RAM" premise, and the `amd64/mkdisk.sh` machinery that bakes that image into
the boot media, all exist for one reason: *the bare-metal box had no storage
driver*. Once `sda1` mounts, they are a fallback for a case that no longer
happens.

**Fix.** Keep `RootDevice::Ram` (a boot with a broken disk should still reach a
shell), but demote it: `mount_root` becomes a single probe in preference order
(USB → virtio → ram) instead of two entry points (`mount_root` hard-wired to
virtio at `fs.rs:149`, `mount_root_on` for everyone else), and the mb2 path stops
being *defined* by the ramdisk.

---

## 9. `usermode.rs`, and one cosmetic

3513 lines, of which **802 are self-tests** (`spawn_test`, `console_notify_test`,
`busybox_test`, `execve_test`, `fork_test`, `smoke_test`, `preempt_test`,
`smp_parallel_test`, `elf_test`, `reject_test`, `fdprobe_test`). Across the
target, ~2068 of 18,458 lines are co-located `Suite` tests — which is a
defensible design and should stay co-located; it is only `usermode.rs` where the
file has become hard to move around in.

**Fix.** `usermode/mod.rs` (entry + dispatch), `usermode/proc.rs`
(fork/exec/spawn/wait/`Spawn` table), `usermode/tests.rs`. Pure mechanics, no
behaviour change.

Also: `amd64/src/fd.rs:1684` is a rustfmt-mangled line —
`fn poll_ready(fd: u64) -> (bool, bool) {    if fd == 0 {`.

---

## 10. Smaller notes

- **`serial.rs` is `akuma-uart`'s x86 twin and should stay separate.** Its header
  already argues this and the argument holds: the two share a register layout and
  nothing else — port I/O vs an MMIO window the MMU must have mapped. Merging
  them means an abstraction over "how a byte reaches a register", which is the
  `trait Arch` shape the tree rejects. Only the `KLOG` ring inside it is shared
  material (§4).
- **`net.rs`'s `has_rdrand`/`weak_fill`/`rng_fill`** (`amd64/src/net.rs:109-218`)
  is a local entropy source. Worth checking against `src/`'s RNG subsystem before
  it grows further, but not obviously duplicated today.
- **`clock.rs`** is correctly scoped and says so: sync-once SNTP, explicitly not
  `akuma-syscalls-time`'s real clock, and it already consumes the extracted
  `akuma-sntp`. Leave it.
- **`mm.rs`, `blk.rs`, `fs.rs`, `loader.rs`** all have accurate headers naming
  what they defer and why. They are the model the rest of the target should be
  read against.

---

## 11. What actually gets to self-hosting

The disk is a prerequisite and nowhere near sufficient. Ranked by how much each
blocks:

### 1. `fork` copies every page eagerly. There is no CoW. — **DONE 2026-09-06 (SMP=1)**

**Closed at SMP=1.** `crates/akuma-cow` holds the shared decision; the marker is
PTE bit 9; three teardown sites moved to `cow_ref_dec`. 2000 forks, 0 KiB drift.
Needs a TLB shootdown before SMP>1. See `docs/archive/AKUMA_AMD64_COW.md`.


`Process::fork_from` (`amd64/src/usermode.rs:1767`):

> A `fork` child: a full eager copy of `parent`'s address space (every user page
> in fresh frames — **no CoW on this target**)

`amd64/src/paging.rs:387` confirms there is no per-page refcount to build one on.

`cargo` forks `rustc`; `rustc` forks the linker; every build script forks. A full
frame-by-frame copy of a `rustc` address space, per fork, fails on time and on
RAM — and `MAX_PROC_FRAMES` returns `ENOMEM` before it even gets to fail slowly.
Nothing else on this list matters until this is fixed.

**Work:** per-page refcount in the PMM (AArch64's is in `akuma-mmap`/`akuma-exec`
and is entangled with `MmapRegion`), write-fault CoW break in `idt.rs`'s `#PF`
handler, and the `is_write` distinction `akuma-mmap` already documents — a
`mprotect(PROT_READ)` page and a CoW-demoted page are both read-only in the PTE,
and breaking CoW on the wrong one silently defeats `mprotect`
(`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`).

### 2. Threads and futex are both entirely absent — **DONE 2026-09-06**

**Closed.** A real Rust `std` binary spawns and joins a thread on this kernel
and exits with the status Linux gives it, from the same bytes. `clone`,
`futex`, `gettid` and a real `exit`/`exit_group` split are wired;
`akuma-syscalls-sync` supplies the decisions and `amd64/src/{thread,futex}.rs`
the effects. Boot suite 240 → 245 (`usermode::thread_test` +
`userspace/amd64/threadprobe`). See `docs/archive/AKUMA_AMD64_RUST_STD.md`.

Three corrections this section needed, all found by measuring rather than
reading:

- **futex was not the wall and could not have been.** A single-threaded Rust
  `std` program calls futex **zero** times — musl only syscalls into one on
  contention — so futex was unreachable until `clone` worked. Implementing it
  first would have been implementing something untestable.
- **"per-task FS/GS base save/restore on switch (currently *not* saved)" was
  already false when this was written.** `UserCtx::{fs_base, gs_base}` are
  per-task and the scheduler `wrmsr`s both on every switch; the CoW `fork` work
  did it. Half of this item was closed and the list did not know.
- **The scheduler needed no change at all.** `Task::space_root` has been
  per-task since Stage I, so two tasks sharing one address space was already
  expressible.

The one non-obvious cost was `syscall_entry` dropping Linux's `a6`: `futex`
takes six arguments and `FUTEX_WAIT_BITSET`'s `val3` is the bitset Rust `std`
uses for every timed wait. Fixed by turning an alignment pad (`sub rsp, 8`)
into `push r9` — System V's seventh argument, same eight bytes.

The AArch64 kernel half was **not** reusable, and this is measured rather than
assumed: `akuma-syscalls-glue::sync` is 947 lines of which 22 references are
`akuma_exec::threading`, and `cargo check -p akuma-syscalls-glue --target
x86_64-unknown-none` fails on the whole `UserAddressSpace` page-table walker.
Reusing it means porting `akuma-mmu` — §11.4's prerequisite, not this one's.

Verified on real silicon under Firecracker/KVM as well as QEMU: VCPUS=1
**235/0**, VCPUS=4 **244/0**, `ruststd` 6/6 and `futexops` 5/5 + `futextest`
7/7 at both. SMP=4 exercises *threads* rather than CoW `fork` — `clone(CLONE_VM)`
demotes no PTE, so it never needed the shootdown that keeps §11.1 at SMP=1.

Running the tree's **existing** futex gate (`scripts/futex_suite.py`, whose C
probes cross-build for x86_64 unchanged) found one real bug that none of the
probes written for this change could: **`nanosleep` was a no-op `yield_now`** —
documented as an honest approximation of a coarse clock, and actually *no sleep
at all*, so every program using a sleep to sequence against another thread
silently lost its ordering. Fixing it exposed a second: `uptime_us` is the LAPIC
tick counter, a syscall runs with `IF` clear, and the only place the scheduler
re-enables it is the idle loop — so two tasks spinning in the kernel freeze the
clock they are both waiting on. `sched::allow_tick` is the fix. Both were
required; either alone still hangs. See `AKUMA_AMD64_RUST_STD.md` §8.

Two of the four probes (`futexkey`, `futexkill`) still cannot run: they need
**`pipe(2)`**, which this target does not implement.

What is **left** here, and is now signals' problem rather than threads':

- `exit_group` cannot reach a thread in a syscall-free ring-3 loop, because
  there are no signals to interrupt it with. `thread::drain` is bounded and
  says so on the console; that is containment, not a fix. → §11.7.
- `sigaltstack` is still `ENOSYS`. musl tolerates it. → §11.7.
- `FUTEX_REQUEUE` and `FUTEX_WAKE_OP` are implemented over the crate's algebra
  and `futexops` now covers all four ops against Linux semantics.
- **`pipe(2)` is absent**, which blocks `futexkey` and `futexkill` — the two
  probes that cover cross-address-space key leaks and kill-while-parked, the
  two futex bugs that historically cost the most to find. Same root as the
  known-broken `cmd | cmd`: fds 0-2 are handled by number below `fd.rs`'s
  table (`FIRST_FILE_FD = 3`), so `dup2` onto them has nowhere to land.

### 3. One global 64-entry fd table — **DONE 2026-09-06**

**Closed.** `fd.rs` is now the POSIX two-level split: `FDS[row][fd]` is one
descriptor row per `PROCS` slot (plus a `KERNEL_ROW` for the boot suite, which
runs with `current_proc_slot() == usize::MAX`), and `FILES` is the machine-wide
table of open file **descriptions**, reference counted. 64 KiB of `.bss`.

The doc predicted "this should be a relocation — the operations don't care
where the array lives". It was not, and the reason is worth keeping: moving the
array fixes the *budget* and nothing else. Three defects needed the split:

- one 64-descriptor budget for the whole machine (`apk` ran it dry installing
  14 packages, and the next `apk` started from a full table);
- a `close` in a forked child reached into its parent, because both named the
  same array slot — `sh -c 'prog > file'` could not work even in principle;
- `dup` was a **value copy**, a pinned divergence: separate cursors, and
  closing one dup released the socket under the other. It is now a second name
  for one description.

`fork` copies a row and increments; exit drops a row and decrements; only the
last name going away releases the description and persists a written file.
`close_owned_by` survives as the exit hook, but is a row release rather than a
search for an owner.

**What came out of it that was not in this item.** With rows in place,
descriptors 0/1/2 could be *bound*, which is what `dup2` needs — see §11.8.

### 4. `mmap` is a global monotonic bump with no VA reuse

`amd64/src/mm.rs:69`:

```rust
static NEXT_VA: AtomicU64 = AtomicU64::new(MMAP_BASE);   // MMAP_BASE = 0x1_0000_0000
```

One `AtomicU64` shared by **every process** — `release_anon_frames`'s own doc
says "another process's slice of the shared bump range". `munmap` frees frames
but never returns virtual address space. `MAP_FIXED` and file-backed mappings are
refused outright. A long `rustc` run walks off the end of the window.

**Work:** adopt `akuma-mmap`. **The gate is gone as of 2026-09-06** —
`REDUCING_PLATFORM_DEPENDENCY.md` §1 is done: `MmapRegion` records a neutral
`akuma_mmap::Prot` instead of a raw AArch64 PTE `u64`, and the bit tables moved
down to `akuma_mmu::types`. `akuma-mmap` keeps its empty `[dependencies]` table
and `#![forbid(unsafe_code)]`, builds for `x86_64-unknown-none`, and amd64
already depends on it (`amd64/Cargo.toml:125`), so adoption needs no new edge.

Two notes for whoever does the adoption:

- **There are now two types called `Prot`.** `akuma_mmap::Prot` is what a
  *region* records — the owner's read/write/exec, six opaque variants.
  `amd64::paging::Prot` is what a *page table* takes — it carries `user` and
  `cow`, which a region never names. They are genuinely different things, which
  is why they were not merged; rename the amd64 one to `PteProt` when adopting,
  or the collision will read as an oversight.
- amd64's half of the encoding is the mirror of `akuma_mmu::user_flags::to_pte`:
  a total match from `Prot` to x86 bits, living in `amd64/src/paging.rs`. The
  AArch64 side is pinned by `prot_roundtrips_to_todays_bits`; write the
  equivalent, because a permission table is exactly the thing that looks right
  and is not.

### 5. Files are cached whole in kernel heap

§2. `rustc` reads large `.rlib`s and metadata; `open` currently means "allocate
the file's size and read all of it".

### 6. `PT_INTERP` is refused — static-PIE only — **DONE 2026-09-06**

**Closed.** The loader places the interpreter at `INTERP_BASE` and reports
`AT_BASE`; a stock Alpine dynamically-linked `busybox` runs.
See `docs/archive/AKUMA_AMD64_DYNAMIC_LINKING.md`.


`amd64/src/loader.rs:404`:

```rust
return Err("PT_INTERP present — this kernel has no dynamic linker, only static-PIE");
```

A stock `rustc` is dynamically linked against `librustc_driver.so` and
`libLLVM.so`. Two ways out: build a fully static `rustc` (hard — LLVM), or
implement the interpreter path. `akuma-elf` has `interp.rs` on the AArch64 side,
and `loader.rs`'s header already names the right extraction shape — a
**parse/place split**, where `ElfSource` + `parse_headers` is neutral and the
mapping half is not.

### 7. No signals

`rt_sigaction` returns 0 and does nothing (`usermode.rs:859`). `wait4` is a
`yield_now` spin loop (`usermode.rs:861-874`) that burns a core per waiting
`cargo`. `rustc` needs a real `SIGSEGV`→abort for ICE handling; `cargo` needs
`SIGCHLD` semantics for reaping.

### 8. Shell redirection and pipelines — **DONE 2026-09-06**

Not in the original eight, because until §11.3 landed it was not reachable.
`dup2`/`dup3`/`pipe`/`pipe2` were **not dispatched at all**, and descriptors
0/1/2 were routed by number below `fd`'s table, so `dup2` had nowhere to land.
The symptoms were `echo x > file` leaving a **zero-length file** and
`cmd | cmd` reporting *can't create pipe* — read as filesystem or resource
problems, and neither.

The fix is a **row override**, not console descriptions: `FDS[row][0..3]` is
`NO_FILE` by default and every by-number console path is now guarded on
`!is_bound(fd)`, so an unbound 0/1/2 behaves exactly as before and a bound one
wins. That is why this was a fill-in rather than a redesign.

`pipe(2)` needed a lifetime rule of its own. A spawn-owned pipe is freed when
its *read* end closes (only one end is ever a descriptor); a `pipe(2)` pair has
two, either may be closed first, so it is freed when the last goes —
`pipe::alloc_pair` and `Slot::ends`. The counter counts **descriptions, not
descriptors**: `dup` and `fork` add a name, and `FILES`' own refcount keeps the
description alive, so counting names here would double-count every inherited
pipe and leak it. `MAX_PIPES` 16 → 64.

**Two bugs this exposed**, both invisible while nothing could redirect:

- `open_flags` read neither `O_APPEND` nor `O_TRUNC`, so **every `O_CREAT` open
  started from an empty buffer** and `>>` silently behaved as `>`. Since
  `close` persists the buffer as the file's entire contents, that is not a
  mis-positioned write, it destroys the rest of the file.
- `mkdir` (83) was not in the dispatch table while `mkdirat` (258) was
  implemented and working — busybox uses the legacy number. Added with the
  other three legacy spellings (`rename` 82, `rmdir` 84, `unlink` 87) as
  `AT_FDCWD` shims, the same shape `stat`/`lstat` already used.

Still open: an unbound 0/1/2 is not "free" for allocation, so `close(1);
open(f)` returns 3 rather than 1 (pinned divergence — every real shell uses
`dup2`), and `FD_CLOEXEC` is still accepted-and-ignored, which became a *live*
divergence when `execve` landed.

### Suggested order

Three of the original eight are done (§11.1 CoW, §11.2 threads+futex, §11.6
`PT_INTERP`), all on 2026-09-06. What is left, re-ranked by what a real
toolchain now hits first:

Rewritten 2026-09-06, after §11.3, §11.8 and the process ceiling landed.

```
  0. akuma-mmap adoption (§11.4)     — NOW THE TOP BLOCKER, and worse than
                                        this doc first said: `MAX_MAPPING` is
                                        64 MiB and refuses more, `EAGER_MAX_
                                        PAGES` is usize::MAX so there is *no
                                        lazy path* — a reservation costs its
                                        full size in physical RAM — and
                                        `NEXT_VA` is one bump shared by every
                                        process with no VA reuse. rustc and
                                        LLVM reserve far more than they touch.
                                        The gate is gone (REDUCING_PLATFORM_
                                        DEPENDENCY §1) and amd64 already
                                        depends on the crate.
  1. inode-backed fd I/O (§2/§11.5)  — `open` means "allocate the file's size
                                        and read all of it". rustc reads large
                                        .rlibs. The disk exists now.
  2. signals (§11.7)                 — `rt_sigaction` returns 0, a stub Rust
                                        *believes*; `wait4` is a yield spin
                                        burning a core per waiting cargo.
  3. akuma-slot-table for PROCS /
     SPAWN / THREADS                 — three hand-rolled `static mut` tables.
                                        Pairs naturally with the process-entry
                                        work above: both are about slot
                                        identity, and doing them apart touches
                                        the same code twice.
  4. boot-path merge (§1)            — no direct self-hosting payoff; its value
                                        is that it stops manufacturing "works
                                        under QEMU, broken on the HP box".
                                        `redirect_test` is registered on both
                                        paths; `spawn`/`busybox`/`execve`/
                                        `fork` still are not.
  5. dynamic linker done (§11.6)     — static vs dynamic rustc is a choice
```

Two items the work of 2026-09-06 added to this list rather than removed:

- **`PROCS`/`SPAWN`/`THREADS` are three hand-rolled `static mut` tables.**
  `crates/akuma-slot-table` is exactly this — FREE/ACTIVE/RETIRED slots with
  reuse generations — it is used by neither kernel directly, and it **does**
  build for `x86_64-unknown-none` (checked). Adopting it for all three at once
  is the right shape; one table converted and three not is not.
- ~~**`proc_entry_for` tops out at slot 15**, which is why `sys_fork` refuses a
  parent slot `>= 16`. Nine concurrent forked processes. `cargo` will find
  this.~~ **DONE 2026-09-06.** There is one `proc_entry` now, and the `PROCS`
  index it serves is seeded into `UserCtx::proc_slot` by
  `sched::seed_proc_slot` while the task is still unpublished — the shape
  `thread::thread_entry` had used all along, and which `UserCtx::thread_slot`'s
  own doc pointed at. Both `>= 16` guards became `>= PROC_SLOTS` (128), and
  `sched::spawn_in_space` — the publish-immediately variant — was deleted,
  because a task that is schedulable before it is seeded reaches `proc_entry`
  and finds `usize::MAX`. Verified on the metal: a **20-stage pipeline** runs,
  i.e. 20 concurrent processes where the ceiling was 9.

**The overlap is the argument.** §2 (`fd.rs` caching), §5
(`akuma-syscalls-abi`), the `akuma-mmap` adoption and the `loader.rs`
parse/place split are each *both* a streamlining item and a self-host
prerequisite. Doing them as cleanup work and doing them as port work is the same
work.

---

## Background

- `docs/archive/AKUMA_FIRECRACKER_AMD64.md` — the staged bring-up this target
  followed, and where most of the "deferred, and here is why" notes live.
- `docs/archive/AKUMA_SELF_HEALING_PORT.md` — the clock, DNS and resolver work
  that `clock.rs`/`dns.rs`/`net.rs` came out of.
- `docs/archive/AKUMA_AMD64_USB_XHCI.md` — the disk work this survey was taken
  against.
- `docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md` — the bare-metal reference
  machine, and why `serial::PRESENT` exists.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` — §1 (the PTE-flag encoding) is
  the standing prerequisite for §11.4; §7 is why the boot-path merge is glue and
  not a `trait Arch`.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why CoW and `mprotect` have
  to be told apart before either is trusted.
- `docs/archive/AKUMA_AMD64_RUST_STD.md` — §11.2's closure, and the measurement
  method: a real `std` binary, a `sha256`-identical A/B against Linux, and the
  note that on Apple Silicon `docker --platform linux/amd64` runs a binary
  correctly and traces it wrongly.
- `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` — the campaign this survey
  is the amd64 sequel to.
