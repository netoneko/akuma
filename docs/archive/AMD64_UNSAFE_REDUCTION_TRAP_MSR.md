# amd64 unsafe reduction, round 2: the trap entry, the MSRs, and the tables that claimed "under the BKL"

**Date:** 2026-10-18. **Status:** landed on `more-amd64-fixes` (`1e25768c`, `f9dbd044`,
`f1997b33`, `5e263f32`). Verification is in the table at the end — read it before
trusting a number here.

Continues the audit in `docs/reference/unsafe-budget.json` (gate:
`scripts/check_unsafe_budget.py`). Policy is unchanged: `unsafe` goes in small leaf
crates with one stated contract, drivers and policy shrink to safe Rust, and
macro-generated `unsafe` counts.

## Numbers

| | before (`c1fe3655`) | after (`5e263f32`) |
|---|---|---|
| tree, production `unsafe` sites | 708 | **600** |
| `amd64/src` | 240 | **118** |
| `amd64/src/idt.rs` | 38 | **4** |
| `usermode.rs` / `smp.rs` / `multiboot2.rs` | 31 / 17 / 19 | 6 / 10 / 6 |
| `futex.rs` / `thread.rs` / `sched.rs` / `lapic.rs` | 12 / 7 / 13 / 13 | 0 / 0 / 6 / 4 |
| new crates | — | `akuma-trap-amd64` 2, `akuma-idt-amd64` 1 |
| `akuma-cpu` | 54 | 64 (one place for what the tree open-coded) |

## What moved where

**`akuma-trap-amd64`** — the CPU's exception frame (`InterruptStackFrame`,
`CodedFrame`) and the saved register file (`TrapRegs`) as types; the twelve entry stubs
as `#[unsafe(naked)]` functions (two shapes, `plain!` and `coded!`, replacing five
hand-copied `global_asm!` blocks); and `Trap { kind, error_code, frame: &mut, regs:
&mut }`, built **once**, in one private function per stub shape, and handed to a
`fn(&mut Trap)` registered per `Kind` through `akuma_not_even_once::Registered`
(lock-free read). Stubs reach their thunk and `SMAP_ACTIVE` by `sym`, so there is no
`unsafe extern "C"` block and no `#[unsafe(no_mangle)]` on a dispatcher. The contract is
the crate header's: the pointers a stub hands its thunk address the frame the CPU just
pushed and the block the stub just saved, disjoint, and exclusively the handler's until
`iretq`. The dereference is a private function, which is what discharges it.

Adding a trap is a `Kind` arm (vector, error-code shape, DPL) and a `register` line;
`idt::init` walks `Kind::ALL`. Host tests pin the stub offsets the assembly hard-codes
(`lea rdi, [rsp + 120]` / `[rsp + 128]`), the alignment arithmetic, the vector table,
and that an edit through `&mut Trap` lands in the stack words `iretq` restores.

**`akuma-idt-amd64`** — 256 gates as `AtomicU64` pairs (the word with the present bit
is stored last), the gate encoding as a pure host-tested function, `lidt` the only
`unsafe`. `static mut IDT` and four `&raw mut` blocks are gone.

**`akuma-cpu`** gained what the tree open-coded: `tsc::rdtsc` (20 call sites),
`debugreg::{dr6, clear_dr6}`, `park::{wfi_mwait, irq_window}` (the idle
`sti;mwait;cli` and `sti;nop;cli` brackets), and **`msr`** — a closed `Msr` enum
(`ApicBase`, `Pat`, `Efer`, `Star`, `Lstar`, `Fmask`, `FsBase`, `GsBase`, `KernelGsBase`),
safe `read`, `unsafe write`, and safe `set_fs_base` / `set_user_gs_base` behind a
canonical-address check. Three copies of `wrmsr` and two of `rdmsr` collapsed.

**Tables that said "under the BKL" and were checked by nothing:**

| table | was | now |
|---|---|---|
| `SPAWN` (usermode) | `static mut`, 11 raw-pointer sites | two arrays of atomics, free row = `pid 0` |
| `WAITERS` (futex) | `static mut`, 12 sites | `Spinlock<WaiterTable>`; `purge_task` runs at thread teardown, which is not obviously under the BKL |
| `THREADS` (thread) | `static mut`, 7 sites | `Spinlock<[Option<Thread>; 448]>`; scans copy a row out and drop the guard before calling the process table or the scheduler |
| per-slot kernel/trap stacks | `vec![0u8; N].leak()` + raw canary writes | `Vec<AtomicU64>` leaked once into a `OnceCopy<Stacks>`; the canary is plain atomic stores |
| `UserCtx` through `smp::current_uctx()` | nine `unsafe { (*ptr).x }` blocks | `sched::with_current_uctx` / `read_current_uctx` over the slot's `BorrowCell` (the pointer was always `MACHINE[slot].uctx.as_ptr()`) |
| LAPIC register page | `LAPIC_BASE` + `reg_ptr` | `OnceCopy<MmioWindow>` |
| framebuffer, boot page directories, WC split tables | raw pointers, `unsafe impl Send for FbConsole`, `PtPage` | `MmioWindow` (+ new `fill32` = `rep stosd`), `pagetable::Table` (+ new `Table::zeroed`) |

## A real bug the cleanup found

`sys_arch_prctl(ARCH_SET_FS | ARCH_SET_GS, addr)` wrote the MSR with whatever the
program passed. `wrmsr` of a **non-canonical** address to `IA32_FS_BASE` raises `#GP`
*in ring 0*, which `general_protection_trap` treats as fatal for a non-user-copy `rip`:
one `arch_prctl(0x1002, 0x8000_0000_0000_0000)` halted the machine. The old comment
said "a bad value can only fault the program's own accesses", which is true of a
canonical bad value only. It is `EPERM` now, as in Linux, and the context-switch
restore skips a non-canonical stored value instead of faulting the switch.

## Things found on the way that are not fixed

* **The thread probe self-test flakes under host CPU load.** `thread: probe reported
  every check [FAIL] got 0x0 want 0x1ff` plus its nine children, at SMP=4 under TCG:
  the probe makes **no** failing syscall (`strace_err` is silent), `[threads] new
  high-water` stops one short of a passing run, and teardown still reports no leak.
  It predates this work: the session-start baseline `c1fe3655` reproduces it (1 of 5
  with the host loaded by `ncpu` busy loops, 0 of 6 quiet). On this branch it ranged
  from 0 of 5 to 4 of 4 across batches of runs, tracking how busy the host was. Adding a ring-buffer log of every `EXIT_STATUS` store
  made it vanish (0 of 5), so the next attempt needs an instrument with no code-layout
  or timing footprint. Not root-caused.
* **`ls /root` spins forever on the trashcan's running kernel** (`a1022dcd`): the
  process burns a core at 100% (14–16 CPU-minutes observed, two of them at once) and
  while they ran a `git worktree add` on the same box made no progress until they
  were killed. Reproduced twice with a bare `ls /root`; other directories list fine.
  Do not `ls /root` there. **ROOT-CAUSED AND FIXED 2026-10-19** (branch
  `fix/ls-root-getdents-spin`, commits `a6fd5eb9`, `11639f61`, `7c98d407`; kernel
  `7c98d40-release-smp-shared` verified on the metal). Three stacked defects, in the
  order they were found, each masked by the next:
  1. `sys_getdents64` (`akuma-syscalls-glue::fs`) cloned the fd's whole
     directory snapshot **per call**; `/root`'s inode (1589249) holds ~145 k
     entries in 5.5 MiB of dirent data (months of on-box build churn — ext2
     directories never shrink), so the ~1,300-call stream re-allocated ~100 k
     `String`s per call under the VFS BKL. Fix: pure `getdents_step` fills by
     slice; the snapshot is moved into the fd once.
  2. Even with the arm clone-free, `proc.get_fd` deep-clones the `FileDescriptor`
     — including that snapshot — so every call still paid one full clone
     (probe on the metal: 70 ms/call). Fix: `SharedFdTable::with_fd` borrows the
     live entry in place; the fill runs inside `update_fd`. Measured after:
     4,038-call stream with a 2 KiB buffer in **446 ms total** (110 µs/call;
     call 1, the snapshot build, is 421 ms).
  3. Every `stat`/`open`/`exec` path resolution read the **whole directory** per
     component (`lookup_path` → `read_inode_data` + `parse_directory`): 238 ms
     per `stat` on the metal, so `ls` — which stats entries — could not finish.
     Fix: `Ext2Filesystem::find_in_dir` scans block by block and stops at the
     match. 50 stats: 11.9 s → 0.58 s.
  After all three, `ls /root` completes (~5 min for 145 k entries) and the box
  stays fully responsive throughout — ssh answered in 0.18 s mid-listing; the
  original "one ls wedges every other syscall" is gone. **Why /root had 145 k
  entries at all**: `/root/hda-capture.sh` — a meow-written HDA debug capture,
  run by herd's `hda-capture` service with `restart = true` — wrote a
  `dmesg-boot-<ts>.txt` into `/root` **every ~3 s** (the service exits
  immediately, herd restarts it): 146,113 files since Oct 9, ~9 GB, and the
  5.5 MiB directory that turned every lookup into a linear scan. Fixed
  2026-10-19: the script now writes `/var/log/dmesg/hda-boot-*.txt` and prunes
  to the newest 20, and the service was removed from `enabled/` (parked at
  `disabled-hda-capture.conf.removed-2026-10-19`; the 146,113 legacy files were
  deleted from Ubuntu — `find … -delete`, 1 m 48 s; a plain `rm` glob died of
  ARG_MAX, and a background `rm` loop from the Akuma side would have taken
  hours). Ryzen's Akuma partition was checked and is clean — its klog is
  `userspace/klogd`, which writes per-boot rotated logs to `/var/log/ryzen`
  with a 256 MiB cap and never touches `/root`. Post-cleanup end state on the
  metal (`7c98d40-release-smp-shared`): `/root` = 426 entries, `ls /root`
  0.01 s warm (5.9 s first call — the cold USB read of the 5.5 MiB of stale
  dirent blocks ext2 keeps allocated; recreate `/root` from Ubuntu some day to
  compact them). **Still open**: lookups are a linear dirent scan, so a
  *populated* large directory still costs ~2 ms per stat — real Linux has the
  same O(n) scan on
  non-htree directories; the proper followup is **htree (`dir_index`) lookup
  support** — the feature flag is already set on this image (tune2fs) and
  `/root` carries `EXT4_INDEX_FL`, but the driver parses linearly, so it also
  silently ignores index blocks when *listing* (those directories list
  incompletely-but-terminating rather than spinning). Also found on the way,
  still open: directories grown by Akuma's own `add_dir_entry` can end with an
  **uninitialised second block** (`/root/bigdir`, a fresh 60-file test dir: 
  `i_size` 8192, block 2 full of a freed file's indirect-block garbage) and
  consecutive duplicate name/inode pairs; `debugfs`/`e2fsck -fn` confirm it on
  the raw partition. Harmless to listing (the parser stops at the garbage
  record) but a real write-path bug. The getdents arm's per-call contract is
  pinned by host tests: `cargo test -p akuma-syscalls-glue getdents` and
  `cargo test -p akuma-ext2 lookup`. Diagnostic of record: a raw-`getdents64`
  probe binary (`/tmp/probe2`, `/tmp/probe3` on the box — inline-asm syscall,
  static musl, prints per-call records/EOF/timing); the kernel's own
  `/proc/<pid>/syscalls` counters proved the pre-fix burn, and
  `scripts/install_kernel_amd64.sh` + ryzen builds carried the loop.
* **Swapping `multiboot2.rs`'s `Surface` impl changed the kernel image by −408 KiB**
  (`_kernel_end` `0x1212dd0` → `0x11aced0`, back to the pre-audit size). Nothing in
  the change should shrink the image by that much; it is almost certainly the `fbcon`
  `Console<Framebuffer>` monomorphisation inlining differently. Recorded because a
  size swing of that order moves every later static and the PMM's first free frame,
  and layout-sensitive failures are the open question of
  [`AMD64_MAC_BUILT_KERNEL_METAL_HANG.md`](AMD64_MAC_BUILT_KERNEL_METAL_HANG.md).
* A kernel built twice from one commit on different hosts is still two programs
  (floating `nightly`). Pinning the toolchain remains recommended and undone.

## Not done, and why

* **`FaultInfo` / shared fault policy with `akuma-exceptions`.** The `#PF` chain is
  armed window → user demand paging → CoW → user-copy fixup → ring-3 signal → fatal,
  and each arm's guard is *its effect succeeding* (`fault_in` returned `true`,
  `cow_write_fault` returned `true`), so there is no pure decision to hoist. The one
  pure decision in it, the CoW write fault, is already `akuma-cow`, shared. A decoded
  `FaultInfo` would be a rename.
* **`akuma-mmu`'s AArch64 half (80 sites).** Needs the `Table` adoption done through
  `pub unsafe fn …(l0_ptr: *mut u64, …)` signatures other crates call
  (`demote_range_to_ro`, `map_user_page`, `shatter_block_to_pages`), i.e. an API
  change across crates, verified on `lima`'s aarch64 KVM boot. Not started.
* **Drivers on raw pointers** (`i2c.rs`, `rtw89.rs`, `net.rs`, `nvme.rs`,
  `watchdog.rs`): mechanical `MmioWindow` conversions, but the hardware that exercises
  them (ryzen's NVMe/i2c/wifi) was offline, and the trashcan's NIC is the only one it
  covers. Left rather than converted blind.
* `usermode.rs`'s remaining six: the `enter_user_mode` extern block, the fault-exit
  `asm!`, the `enter_user` call, the `leave` read that still takes the raw `UserCtx`
  pointer (it is handed on to `signal::deliver_pending`), the `Sysinfo`-as-bytes
  view, and `init_syscall`'s MSR writes. `mm.rs`'s nine are physmap page views (`write_bytes`/`from_raw_parts` on a frame
  address) that want a *sound* owned-frame type, not a safe wrapper over `pa: u64` —
  a `fn zero_page(pa: u64)` would be unsound (any caller could pass any frame).

## Round 3 (2026-10-19): items from "Not done" that are now done

446 -> 434 production `unsafe` sites (`amd64/src` 81 -> 71).

* **Raw-pointer drivers.** The list was stale: `i2c.rs`, `nvme.rs`, `watchdog.rs` and
  `rtw89.rs` were already on `MmioWindow`; what was left was `net.rs`
  (`init_rtl8169(*mut u8)` -> `Rtl8169Device::probe`), now safe and window-taking, and
  four copies of "map pages Device, then vouch" (`i2c`, `watchdog`, `pci`, `lapic`),
  now one `paging::map_device_window`.
* **`usermode.rs`.** The `Sysinfo`-as-bytes view is `Sysinfo::to_bytes()` in the
  `forbid(unsafe_code)` ABI crate, written at `offset_of!`s and tested against
  hand-written offsets; the `leave` read goes through `sched::read_current_uctx`. What
  remains there is inherently unsafe (the `enter_user_mode` externs and call, the
  stack-switching `asm!`, `init_syscall`'s MSR writes).
* **Physmap page views.** `mm.rs` has none now; they were in `idt.rs`, `loader.rs` and
  `usermode.rs`. `phys::FreshFrame` is the sound owned-frame type: an exclusively owned,
  not-yet-published frame, freed on `Drop`, handed on with `publish()`. It replaces the
  demand-zero, CoW-destination, loaded-segment and test-program-staging views, and fixed
  two latent leaks on failure paths (a segment page whose map failed was never freed;
  the test program's failure arm freed a frame the address space already tracked).
  Frames that belong to *someone else's* address space (`loader::write_user`/`read_user`,
  the fault-report memory dump) are not fresh and stay as they were.
* Verified: host tests for the touched crates, clippy, `amd64` check, and the ryzen
  OVMF -> GRUB -> multiboot2 rehearsal, framebuffer and `nofb`: 804 passed, 0 failed.
  **Not exercised off the metal:** `init_rtl8169` (no emulated RTL8168).

## Verification

| check | result |
|---|---|
| host `cargo test` for `akuma-{trap,idt}-amd64`, `-cpu`, `-primitives`, `-pagetable` | green (trap 9, idt 4, cpu 7 incl. canonical-address tests, primitives incl. `fill32`) |
| clippy `-D warnings` (pre-commit hook, every crate) | green |
| `scripts/check_unsafe_budget.py` | ok, ratcheted 708 → 600 |
| QEMU microvm `SMP=4` boot suite | 824 passed / 0 failed on every quiet run of every commit; the thread-probe flake above is the only failure seen |
| QEMU q35 `run-xhci.sh` | 495 passed / 5 failed — the 5 documented `root=/dev/sda1` FS failures |
| **trashcan (HP, bare metal, multiboot2, 4 cores)**, Mac-built `d6ed6691` (md5 `2aa83fa8…`), pushed with `hpbox.akuma_push`, installed with `kinstall`, `reboot -f` | **boots, ssh up, 4 cores.** `uname -v` = `d6ed6691`; `[fb] 3840x2160 … wc on clear 10.9ms = 3024MB/s` (the `MmioWindow`/`fill32`/`Table`-based `map_wc`/PAT path, same as the 3026 MB/s measured before); 0 of `PANIC`/`CANARY`/`[Fault]`/`FREED-CR3`/`SWITCH NO-BKL`/`TLB stuck` in `dmesg`. **Self-test: 777 passed, 1 FAILED - `physmap: reaches PHYSMAP_LIMIT` (`translate` of the last page of the 64 GiB physmap is `None`; the 4 GiB probe passes).** Not yet classified: no control run exists. The session-start baseline `c1fe3655`, Mac-built, is at `/root/akuma-amd64.base` on the box (md5 `9b9ca29e…`), **not installed**; booting it answers whether this failure predates the audit. Not run on metal: spawn stress, an incremental `kbuild`. |

The multiboot2/framebuffer code (`multiboot2.rs`) is **not** executed by the PVH QEMU
runs. The trashcan boots through it (GRUB multiboot2, framebuffer console, WC mapping),
so that run is the only coverage for the `MmioWindow` framebuffer, `fill32`, the
`Table`-based `map_wc`, and the PAT MSR path.

## Background

* Previous round: `AMD64_MAC_BUILT_KERNEL_METAL_HANG.md` (the `akuma-dma` / `akuma-pagetable`
  / `BorrowCell` work and the unexplained Mac-built-kernel hang).
* `docs/reference/unsafe-budget.json`, `scripts/check_unsafe_budget.py`,
  `docs/reference/crate-safety.md`.
* `docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md` — why every stub `cld`s.
