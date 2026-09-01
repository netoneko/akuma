# Exceptions (vector table & trap entry)

The AArch64 exception vector table, GPR/FPSIMD save-restore, and ESR_EL1
dispatch every syscall, page fault, IRQ, and signal delivery flows through.
Source: `crates/akuma-exceptions/src/lib.rs` (3,120 code lines) — it was
`src/exceptions.rs` until 2026-09-01
([`AKUMA_EXCEPTIONS_EXTRACTION.md`](../../archive/AKUMA_EXCEPTIONS_EXTRACTION.md)),
and the move took `src/` production `unsafe` from 91 sites to 11. The crate
reaches kernel-core state (the syscall dispatcher, the IRQ dispatcher, the
`src/config.rs` tunables) only through the `ExceptionHooks`/`ExceptionsConfig`
registrations `src/main.rs` installs at boot; it names no `crate::` path.
For what each destination does
once routed here, see [`syscalls.md`](syscalls.md) "Dispatch" (SVC →
`handle_syscall`), [`memory.md`](memory.md) "CoW fork" (data-abort → CoW
resolution), [`syscalls/signal.md`](syscalls/signal.md) (signal delivery
frame build/unwind), [`drivers/gic.md`](drivers/gic.md) "IRQ dispatch to the
scheduler" (IRQ routing), and [`scheduler.md`](scheduler.md) "Context switch"
(what a context switch saves/restores).

> **Stability: C (active risk).** Touched every month from January through
> July 2026, including inside the Jun 2026 memory+signal crisis
> (`docs/README.md`) and again 2026-07-01. The recurring lesson: **an EL1
> handler must never `eret` back to the ELR it just killed a process for** —
> redirecting to a landing pad (`el1_fault_recovery_pad`) instead of `ELR+4`
> exists because skipping forward re-executes the next instruction with the
> same poisoned register, cascading into a fault loop that overran the kernel
> stack and took down the SSH server (`archive/EPOLL_EL1_CRASH_FIX.md`
> Regressions 2 and 4).

## Vector table

`global_asm!` at the top of the file (`exceptions.rs:14-568`) defines
`exception_vector_table`, 0x800-aligned per AArch64 spec, with 16 entries (4
exception types × 4 exception sources), each 0x80-aligned:

| Source | Synchronous | IRQ | FIQ | SError |
|---|---|---|---|---|
| Current EL, SP0 | `sync_el1_handler` | `irq_handler` | stub | stub |
| Current EL, SPx | `sync_el1_handler` | `irq_handler` | stub | stub |
| Lower EL, AArch64 | `sync_el0_handler` | `irq_el0_handler` | stub | stub |
| Lower EL, AArch32 | stub | `irq_handler` | stub | stub |

Only 4 of the 16 vectors are real handlers — every FIQ/SError entry and the
AArch32-lower-EL synchronous entry point at `default_exception_handler`,
which is a diagnostic dump (ESR/ELR/SPSR/TTBR0/SP) that halts in a `wfe` loop
if `eret` would be dangerous (`rust_default_exception_handler`,
`exceptions.rs:1406`). Akuma never runs AArch32 code and never enables
FIQ/SError routing, so these are pure stubs. `init()` (`exceptions.rs:1381`)
installs the table into `VBAR_EL1` and clears the IRQ mask in `DAIF`.

## Trap frame layout

Two distinct frame layouts exist, both 832 bytes (GPR block + NEON block),
built and torn down entirely in the assembly stubs — there is no separate
`.S` file:

- **EL0 sync (`sync_el0_handler`, `exceptions.rs:104-252`):** `[sp+0..287]`
  GPRs x0-x30 + SP_EL0 + ELR_EL1 + SPSR_EL1 + TPIDR_EL0 (this region is
  `UserTrapFrame`, re-exported from `akuma_exec::threading` at
  `exceptions.rs:639`); `[sp+288..303]` saved kernel SP + a scratch slot for
  the syscall return value; `[sp+304..831]` Q0-Q31 + FPCR + FPSR. The pointer
  to this frame (`x0`) is passed straight into `rust_sync_el0_handler`.
- **IRQ (`irq_el0_handler` / `irq_handler`, `exceptions.rs:263-566`):** same
  832-byte budget, built with individual `stp`/pre-index pushes rather than a
  single fixed-offset block, unified between the EL0 and EL1 IRQ paths so
  `rust_irq_handler_with_sp` doesn't need to know which one fired.

The two NEON blocks are at **different offsets** — sync at `+304` (FPCR `+816`,
FPSR `+824`), IRQ at `+288` (FPCR `+800`, FPSR `+808`) — so the frames are not
interchangeable despite sharing a size. Rust code that reads the sync frame's FP
state (the signal paths, which save it into the sigframe and restore it back) goes
through `akuma_exec::threading::sigframe::SyncFrameNeon` and its
`sync_frame_neon(frame)` accessor rather than open-coding those offsets; the
accessor's safety contract is "sync frame only", for exactly this reason.

Both stubs **always** save/restore the full NEON/FPSIMD register file on
every trap, including plain syscalls — there is no lazy FPSIMD save. Both
clear SPSR_EL1's IL bit (bit 20) before `eret`; leaving it set produces a
spurious EC=0xe (illegal execution state) on the *next* trap. The EL0 IRQ and
kernel IRQ paths both hard-check `ELR_EL1 != 0` immediately before `eret` and
spin on a `0xDEADBEEF` marker instead of returning if it is — a defensive
trip-wire against corrupting ELR to zero (`exceptions.rs:368-372, 524-529`).

**Where the frame lives:** each thread's kernel stack reserves a private 1
KB exception area at its top (`exceptions.rs:574-600`); `TPIDR_EL1` holds the
current thread's exception-stack pointer and is repointed by
`set_current_exception_stack` on every context switch (see
[`scheduler.md`](scheduler.md) "Context switch"). Separately,
`akuma_exec::threading::CURRENT_TRAP_FRAME[tid]` (`set_current_trap_frame` /
`clear_current_trap_frame`, `crates/akuma-exec/src/threading/mod.rs:3088`) is
only populated for the duration of native syscall dispatch
(`exceptions.rs:2342` through `:2397`) — not the whole exception window — so
that `fork`/`clone` can read the parent's full register state while
`handle_syscall` is on the stack.

## ESR_EL1 decoding

`rust_sync_el0_handler` (`exceptions.rs:2033`) and `rust_sync_el1_handler`
(`exceptions.rs:1621`) both read `ESR_EL1`, extract the Exception Class
(`(esr >> 26) & 0x3F`) and ISS, and dispatch on a `match ec` over the
`mod esr` constants (`exceptions.rs:642-648`):

| EC | Meaning | EL0 handler arm |
|---|---|---|
| `0b010101` | SVC64 (syscall) | `exceptions.rs:2044` |
| `0b100100` | Data abort, lower EL | `exceptions.rs:2428` |
| `0b100000` | Instruction abort, lower EL | `exceptions.rs:3044` |
| `0b011000` | Trapped MSR/MRS/system instr | `exceptions.rs:3409` |
| `0b111100` | BRK | `exceptions.rs:3462` |
| anything else | unknown | `exceptions.rs:3470` — logs full state, kills the process (`return_to_kernel(-1)`), never halts the kernel |

The **EL0 SVC arm** extracts the syscall number from `x8` and args from
`x0`-`x5`, then calls `crate::syscall::handle_syscall` — see
[`syscalls.md`](syscalls.md) "Dispatch" for what happens next; not
re-explained here. `rt_sigreturn` (NR 139) is special-cased *before* that
call and handled inline via `do_rt_sigreturn` — see
[`syscalls/signal.md`](syscalls/signal.md). After a syscall returns (or a
signal is pending), the same arm checks `take_pending_signal` and diverts
into `try_deliver_signal` — again, frame construction is
[`syscalls/signal.md`](syscalls/signal.md)'s territory.

The **data-abort and instruction-abort arms** decode `DFSC`/`IFSC`
(`iss & 0x3C`/`0x3F`) into translation-fault vs. permission-fault, and for a
write-permission fault on a CoW page, resolve it inline (alloc copy, remap,
`track_user_frame`/`remove_user_frame`/`cow_ref_dec`) — that resolution
sequence is [`memory.md`](memory.md)'s "CoW fork" section, not repeated
here. This doc's contribution is only that EC=0x24/0x20 is *how* the CPU gets
here: `FAR_EL1` gives the faulting VA, `ELR_EL1` the faulting PC, and the
handler resolves lazy regions, PROT_NONE auto-commit, and CoW faults before
falling back to `try_deliver_signal(frame, 11, far, true, esr)` (SIGSEGV) and
`return_to_kernel(-11)`.

The **MSR/MRS trap arm** emulates a handful of cache-maintenance and
system-register instructions the kernel traps from EL0 (`DC CVAU`/`IC
IVAU`/`DC ZVA`, `CTR_EL0` reads) rather than allowing direct EL0 access, then
advances `ELR_EL1` by 4 and returns. The **BRK arm** treats any `brk`
instruction as SIGTRAP (`return_to_kernel(-5)`).

## EL1 (kernel-mode) synchronous exceptions

`rust_sync_el1_handler` runs when the *kernel itself* faults — almost always
a data abort (EC=0x25) from kernel code dereferencing a user pointer
directly (`phys_to_virt` writes, `copy_to_user`/`copy_from_user`, the rump
sysproxy `copyout` path). Two fast paths run before any diagnostic dump, to
keep normal operation quiet:

1. `try_resolve_el1_cow_fault` (`exceptions.rs:1498`) — same CoW resolution
   as the EL0 data-abort arm, for when kernel code is the one touching a
   shared RO page.
2. `try_resolve_el1_user_copy_lazy_fault` (`exceptions.rs:1584`) — demand-pages
   a not-yet-mapped lazy anonymous page touched by a kernel→user copy, so a
   translation fault mid-copy doesn't EFAULT-truncate the copy at a page
   boundary (fixed after it silently dropped the tail of DNS answers over
   the rump stack).

If neither resolves it and a **registered user-copy fault handler** exists
(set by `copy_from_user_safe`/`copy_to_user_safe` around a bounded access),
`ELR_EL1` is redirected there so the copy returns `EFAULT` instead of
crashing the kernel. Otherwise, for EC=0x25 with `ELR` in kernel code range,
the handler kills only the faulting process — sets it `Zombie(-14)`,
`kill_thread_group`s it, notifies waiters — **and redirects `ELR_EL1` to
`el1_fault_recovery_pad`** (not `ELR+4`) before returning, so `eret` lands in
a function that calls `return_to_kernel_from_fault(-14)` (closes fds, frees
the address space) instead of re-faulting on the next instruction. Any other
EL1 exception class, or a translation-table-walk external abort, falls
through to a full register/page-table dump and halts the kernel in a `wfe`
loop — a kernel-mode fault outside the two fast paths and the EC=0x25
recovery is treated as an unrecoverable kernel bug, not a killable-process
condition.

## Nesting and re-entrancy

IRQs are explicitly re-enabled (`msr daifclr, #2`) partway through
`sync_el0_handler`, once GPRs/FPSIMD are safely on the stack, so a syscall
can be preempted — this is what lets the 10 ms scheduler tick fire during
long syscalls. IRQ handlers themselves do not re-enable interrupts, so IRQ
entries do not nest. An exception firing while already inside `sync_el1_handler`
(e.g. the debug dump itself faulting) is not specially guarded against; the
per-thread exception stack has headroom (1 KB) but a fault-while-handling-a-
fault would recurse until it overflows that area — the historical
`STACK_CORRUPTION_ANALYSIS.md`-class bugs motivated keeping the EL1 path
free of allocation (`StaticWriter`, a fixed 256-byte stack buffer, is used
instead of `format!`/heap `String` for exactly this reason).

## QEMU TCG instruction-misrouting workarounds

The EL0 SVC arm also carries several QEMU-specific recovery blocks that
exist only because QEMU's TCG binary translator occasionally emits EC=0x15
(SVC) for instructions that are not `svc` at all: a stale-I-cache guard that
distinguishes a real syscall from a spurious one by checking `ELR-4` is
actually an `svc` encoding (`is_aarch64_svc`, `exceptions.rs:2017`), a JIT
cache-coherency retry for syscall numbers > 500, and two instruction
emulators (`emulate_dc_zva`, `emulate_stp_xzr_xzr`) for `DC ZVA` and `stp
xzr, xzr` sequences QEMU misroutes when the target lands in a `PROT_NONE`
lazy region (`exceptions.rs:2110-2304`). These are QEMU-emulation quirks, not
Akuma bugs; see `archive/GO_FORKTEST_DEBUG.md` for the full misrouting
taxonomy (Patterns 1-4) if debugging a similar `[WILD-DA]` symptom.

## IRQ entry

`irq_handler`/`irq_el0_handler` save the same 832-byte frame, then call
`rust_irq_handler_with_sp` (`exceptions.rs:1455`), which acknowledges the IRQ
via the GIC, special-cases the scheduler SGI, and otherwise dispatches
through the generic IRQ table. The scheduler/GIC-specific behavior of that
function is [`drivers/gic.md`](drivers/gic.md)'s "IRQ dispatch to the
scheduler" section — not re-explained here.

## EL0 vs EL1 transitions

The lower-EL vectors are only entered on a genuine EL0→EL1 trap (SVC, user
fault, user IRQ); the kernel never routes through them for its own code.
Kernel-mode faults always land in the current-EL vectors
(`sync_el1_handler`/`irq_handler`), which is why the EL1 sync path has to
special-case "is this actually a CoW/lazy-page fault against a user pointer
touched from kernel code" rather than assuming every EL1 fault is a genuine
kernel bug. There is no separate kernel exception stack switch: the EL1
handlers save minimal context on whatever `SP` was active (SPx, per the
vector table's "Current EL, SPx" row) rather than switching to `SP_EL0`.

## Background

- `archive/EPOLL_EL1_CRASH_FIX.md` — the EL1 fault-loop regression that
  produced `el1_fault_recovery_pad`, and the later fix making the pad call
  `return_to_kernel(-14)` instead of yielding forever (socket-table leak).
- `archive/FAR_0x5_AND_HEAP_CORRUPTION_FIX.md` — the original TTBR0-validation
  and IRQ-protection bugs behind `FAR=0x5` kernel panics; eleven separate
  missing-`with_irqs_disabled` fixes across `pmm.rs`/`akuma-alloc`/`process.rs`,
  plus the TLB-flush-after-TTBR0-switch bugs in `mmu.rs`/`threading.rs`.
- `archive/GO_FORKTEST_DEBUG.md` — the QEMU EC=0x15 misrouting taxonomy (DC
  ZVA, `stp xzr, xzr`) behind the workarounds in the EL0 SVC arm.
- `archive/STACK_CORRUPTION_ANALYSIS.md` — per-thread exception-stack layout
  and the ProcessInfo-corruption investigation that motivated allocation-free
  crash-path formatting.
