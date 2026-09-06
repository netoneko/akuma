# amd64: streamlining candidates, and the road to self-hosting

**Date:** 2026-09-06
**Scope:** a read of all 18,458 lines of `amd64/src/` plus `amd64/Cargo.toml`,
taken at the point where the USB/xHCI disk work is nearly done
(`docs/archive/AKUMA_AMD64_USB_XHCI.md`, memory note *amd64 persistent disk =
USB/xHCI*).
**Status:** survey. Nothing here is fixed yet; §4 is the one piece started
(`crates/akuma-dmesg`).

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

## 4. The `dmesg` ring is a `static mut` with untested arithmetic — **started**

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

**Still to do:** `syslog(2)` itself. AArch64 has no `nr::SYSLOG` (asm-generic
116) and no dispatch arm, so `busybox dmesg` cannot yet reach the ring that is
now filling. And amd64 still has its own `static mut` — that is the adoption
pass, where `serial.rs`'s six `klog_*` helpers collapse onto this crate.

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

### 1. `fork` copies every page eagerly. There is no CoW.

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

### 2. Threads and futex are both entirely absent

```rust
56 => {
    const CLONE_VM: u64 = 0x0000_0100;
    if a1 & CLONE_VM != 0 {
        return errno::ENOSYS;   // usermode.rs:827
    }
    return sys_fork();
}
```

and there is **no futex arm at all** — syscall 202 does not appear in
`syscall_dispatch`.

`cargo`'s jobserver is threaded. `rustc` is threaded. Neither runs.

The good news is that the hard, subtle half is already extracted and host-tested:
`akuma-syscalls-sync` owns the futex op decode, the `(tgid, uaddr)` waiter table,
the deadline algebra, `WAKE_OP` and the wait-loop outcome — and per CLAUDE.md,
"every futex bug in `docs/archive/` is a property of one of those four things".
It builds for `x86_64-unknown-none`.

**Work:** the `clone` side — `CLONE_VM|CLONE_THREAD` sharing an address space in
`sched.rs`'s task table, per-task FS/GS base save/restore on switch (currently
*not* saved; `sys_arch_prctl`'s own comment says "two concurrent musl processes
would clobber each other"), then wire `akuma-syscalls-sync`.

### 3. One global 64-entry fd table

`amd64/src/fd.rs:127`, header:

> One table, not one per process. That is wrong in the way that matters as soon
> as there is a `fork`, and right for now.

`fork` now exists. `close_owned_by` (`fd.rs:933`) was added because `apk` already
ran the table to `EMFILE` in the middle of installing 14 packages. `rustc` alone
exceeds 64 descriptors on a real crate graph; `cargo` plus N concurrent `rustc`s
sharing one table is not survivable.

**Work:** move the table into `Process`, raise `MAX_OPEN`. The header says the
operations don't care where the array lives, so this should be a relocation.

### 4. `mmap` is a global monotonic bump with no VA reuse

`amd64/src/mm.rs:69`:

```rust
static NEXT_VA: AtomicU64 = AtomicU64::new(MMAP_BASE);   // MMAP_BASE = 0x1_0000_0000
```

One `AtomicU64` shared by **every process** — `release_anon_frames`'s own doc
says "another process's slice of the shared bump range". `munmap` frees frames
but never returns virtual address space. `MAP_FIXED` and file-backed mappings are
refused outright. A long `rustc` run walks off the end of the window.

**Work:** adopt `akuma-mmap`. Gated on `REDUCING_PLATFORM_DEPENDENCY.md` §1 —
`MmapRegion.flags` is a raw AArch64 PTE `u64` and the two encodings share no
field, which `mm.rs`'s header already names as the prerequisite.

### 5. Files are cached whole in kernel heap

§2. `rustc` reads large `.rlib`s and metadata; `open` currently means "allocate
the file's size and read all of it".

### 6. `PT_INTERP` is refused — static-PIE only

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

### Suggested order

```
  0. boot-path merge (§1)            — cheap, and everything below must be
                                        validated on QEMU *and* metal, which
                                        today are two different sequences
                                        with two different test lists
  1. CoW fork (§11.1)                — nothing works without it
  2. per-process fd tables (§11.3)   — small, and blocks every multi-process test
  3. clone(CLONE_VM) + futex (§11.2) — the hard half is already a tested crate
  4. akuma-mmap adoption (§11.4)     — needs REDUCING_PLATFORM_DEPENDENCY §1
  5. inode-backed fd I/O (§2/§11.5)  — needs the disk, which is in flight
  6. dynamic linker or static rustc (§11.6)
  7. signals (§11.7)
```

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
- `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` — the campaign this survey
  is the amd64 sequel to.
