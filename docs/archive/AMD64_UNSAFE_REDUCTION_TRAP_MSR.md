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
  Do not `ls /root` there; it is a `getdents` problem on that directory, not in this
  branch.
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

## Verification

| check | result |
|---|---|
| host `cargo test` for `akuma-{trap,idt}-amd64`, `-cpu`, `-primitives`, `-pagetable` | green (trap 9, idt 4, cpu 7 incl. canonical-address tests, primitives incl. `fill32`) |
| clippy `-D warnings` (pre-commit hook, every crate) | green |
| `scripts/check_unsafe_budget.py` | ok, ratcheted 708 → 600 |
| QEMU microvm `SMP=4` boot suite | 824 passed / 0 failed on every quiet run of every commit; the thread-probe flake above is the only failure seen |
| QEMU q35 `run-xhci.sh` | 495 passed / 5 failed — the 5 documented `root=/dev/sda1` FS failures |
| **trashcan (HP, bare metal, multiboot2, 4 cores)** | see below |

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
