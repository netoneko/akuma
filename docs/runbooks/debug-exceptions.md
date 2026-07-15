# Debug EL1 crashes and data aborts

Symptom-driven debugging for kernel-mode (EL1) faults, data/instruction
aborts, and "the kernel dumped registers and halted" failures — as opposed to
a hang with no output (see [`debug-boot-hang.md`](debug-boot-hang.md)) or an
OOM/allocation panic (see [`debug-memory-oom.md`](debug-memory-oom.md), which
owns the memory-corruption signatures that overlap with this doc).

> **Stability of this area: C (active risk).** `src/exceptions.rs` was
> touched every month from January through July 2026, including the June 2026
> memory+signal crisis. The recurring lesson across nearly every post-mortem
> below: **an EL1 handler must never `eret` back to the ELR it just killed a
> process for.** Redirecting to a landing pad instead of `ELR+4` exists
> because skipping forward re-executes the next instruction with the same
> poisoned register, cascading into a fault loop.

For vector table layout, trap frame structure, and ESR_EL1 dispatch, see
[`../reference/subsystems/exceptions.md`](../reference/subsystems/exceptions.md).
This runbook is action-first — symptom to cause to fix — not architecture.

## How to read an EL1 crash dump

A kernel-mode fault prints this exact format (`rust_sync_el1_handler`,
`src/exceptions.rs:1673`):

```
[Exception] Sync from EL1: EC=0x25, ISS=0x61
  ELR=0x40293f90, FAR=0x5, SPSR=0x80000345
  Thread=0, TTBR0=0x402e5000, TTBR1=0x402e5000
  SP=0x409ff580, SP_EL0=0x17fffeae9122b280
  Instruction at ELR: 0xf800852e
  Likely: Rn(base)=x9, Rt(dest)=x14
  WARNING: Kernel accessing user-space address!
```

An EL0 (userspace) fault instead prints `[Fault] Data abort from EL0 at
FAR=…` / `[Fault] Instruction abort from EL0 at FAR=…` (`exceptions.rs:3014`,
`:3389`) and is almost always benign — the process gets SIGSEGV, the kernel
survives. Only the `[Exception] Sync from EL1` form means the *kernel itself*
faulted.

Triage steps:

1. **Decode EC** (bits `[31:26]` of ESR_EL1, printed directly): `0x25`/`0x24`
   = data abort (current/lower EL), `0x20`/`0x21` = instruction abort,
   `0x0E` = illegal execution state, `0x22` = PC alignment fault, `0x15` =
   SVC, `0x18` = trapped MSR/MRS. See the EC table in
   [`../reference/subsystems/exceptions.md`](../reference/subsystems/exceptions.md#esr_el1-decoding).
2. **Decode DFSC/IFSC** (`ISS & 0x3F`): `0x04`-`0x07` = translation fault
   level 0-3 (unmapped page), `0x08`-`0x0B` = access-flag fault, `0x0C`-`0x0F`
   = permission fault, `0x21`-`0x23` = **synchronous external abort on
   translation table walk** — this specific class means the page-table
   *itself* is corrupt or the memory backing it aliases something else; treat
   it as a page-table-corruption bug, not a simple bad-pointer bug.
3. **Read FAR_EL1 against the address-space split.** `FAR < 0x4000_0000`
   while the kernel is executing means the kernel dereferenced what looks
   like a user address — the handler prints `WARNING: Kernel accessing
   user-space address!` for exactly this case. Compare `TTBR0` against
   `get_boot_ttbr0()`: if TTBR0 is still the **boot** page table (`0x402x_xxxx`
   range) but the code path assumed a **user** address space was active, that
   mismatch is the bug, not the dereferenced value.
4. **Read ELR_EL1.** Is it in kernel code (`0x4000_0000..0x8000_0000`)? The
   handler dumps the raw instruction word and guesses `Rn`/`Rt` from the
   LDR/STR encoding — cross-reference against `rust-objdump -d` on the release
   binary at that address to get the real instruction.
5. **Garbage ELR == garbage FAR** (a 64-bit value with high bits set, not any
   valid VA) is not a translation fault at all — it's `EC=0x22`/PC-alignment,
   meaning the **saved PC itself was clobbered** by something else (a freed
   page reused, a stack overwritten). Look at `SP` relative to the reporting
   thread's stack bounds (`Thread N kernel stack: base=… top=…`) — `SP` above
   `top` or noticeably below `base` means stack corruption, not a fault at the
   reported PC.
6. **GDB/lldb**: `HVF=0 GDB=1 cargo run --release`, then `lldb -o "gdb-remote
   1234"`. Use TCG (`HVF=0`) — HVF's gdbstub misreports PC as the exception
   vector entry, not the faulting instruction. `x/16gx $sp` and `x/8i $pc` at
   the point of the dump are the highest-signal commands; `watch` a specific
   address if you already suspect which pointer is getting clobbered.
7. Once you've matched EC/DFSC/FAR pattern, check the tables below — most of
   these signatures have already been root-caused once.

## Data abort / stale-pointer crashes (EC=0x25 / EC=0x24)

| Symptom (crash signature) | Cause | Status | Fix |
|---|---|---|---|
| `EC=0x25 ISS=0x61 FAR=0x5` "Kernel accessing user-space address" | `read_current_pid()` read `PROCESS_INFO_ADDR` (0x1000) while boot TTBR0 was active → device-memory garbage interpreted as PID | FIXED | TTBR0-range guard: boot TTBR0 (`0x4020_0000`-`0x4400_0000`) → `None`, don't read (`src/process.rs`) |
| `EC=0x25 ISS=0x46 FAR=0x300c2e80` on process exit, "stale TTBR0 or dereferencing user pointer from kernel" | `replace_image()`/`replace_image_from_path()` didn't reset `clear_child_tid`; exec inherited a stale musl TLS pointer; EL1 write to it never demand-paged (EL1 aborts don't trigger demand paging, only EL0 does) | FIXED | Reset `clear_child_tid=0` on exec (matches Linux `flush_old_exec`); defense-in-depth `is_current_user_page_mapped()` check before the CLEARTID write |
| `EC=0x25` writing into Bun/JSC's gigacage mmap (`FAR=0x50004000`, DFSC=0x07) | Kernel wrote to a user VA (`copy_to_user`-style raw pointer write) whose page wasn't mapped yet | FIXED | `el1_fault_recovery_pad` redirect (see below) is the correct defense; a proposed VA-range exclusion in `validate_user_ptr` was **reverted** — it also rejected valid JSC heap addresses since the kernel heap and JSC's 1 GB gigacage overlap in VA space |
| SSH drops / kernel spins after adding EL1 recovery | Handler `eret`'d back to the same `ELR_EL1` that just faulted → immediate re-fault → recursion → kernel stack overflow, corrupting SSH server state | FIXED | Redirect `ELR_EL1` to `el1_fault_recovery_pad()` (not the original ELR) before `eret` |
| `EC=0x25 FAR=0x1` repeating, cascading | `ELR+4` "skip the bad instruction" hack advanced into the *next* instruction, which reused the same poisoned register and faulted again — each retry drifted further into garbage | FIXED | Never `ELR+4`; always redirect to the landing pad, which calls `return_to_kernel_from_fault(-14)` (kills process cleanly) instead of retrying nearby code |
| `bun install` DNS hangs after a crash-and-retry cycle; only reproduces with `node_modules` present | `el1_fault_recovery_pad` looped `yield_now()` forever instead of exiting; the killed process's sockets were never closed, exhausting `MAX_SOCKETS=128` after a few crashes, so the next process couldn't allocate a DNS UDP socket | FIXED | Pad now calls `return_to_kernel(-14)`, which runs the full exit path (`cleanup_process_fds`, channel teardown, address-space deactivation) |
| Kernel copy silently truncates data mid-copy (e.g. rump DNS answers cut short) | `copy_to_user`/`copy_from_user` hit a translation fault on a not-yet-mapped lazy anonymous page mid-copy and bailed with a short copy instead of paging it in | FIXED | `try_resolve_el1_user_copy_lazy_fault` demand-pages the lazy page inline before falling back to the registered fault handler |
| **Generic EC=0x25 in kernel code, no special-case matched** | Any kernel dereference of a bad/unmapped user pointer not covered by the two fast paths (CoW, lazy-copy-fault) | BY DESIGN | Handler kills only the faulting process (`Zombie(-14)`, EFAULT) and lands in the recovery pad — this is now the default behavior, not a bug. If you see the kernel *halt* instead of killing one process, ELR was outside the recognized kernel-code range (`0x4020_0000..0x6000_0000`) — that's the actual bug to chase |

## PC / instruction-corruption crashes (EC=0x22 alignment fault, EC=0x21/0x20 instruction abort, EC=0x0E)

| Symptom (crash signature) | Cause | Status | Fix |
|---|---|---|---|
| `EC=0x22 ELR==FAR==<64-bit garbage>`, `SP` above the reporting thread's stack top, "Kernel SP outside thread's stack bounds" | Boot stack and kernel heap overlapped at low RAM (`code_and_stack` constant forgot the `KERNEL_BASE` offset) — heap allocations under load landed on top of Thread0's saved stack frames, and a function epilogue (`ldp x29,x30,[sp]; ret`) loaded garbage and jumped to it | FIXED | `code_and_stack` now also covers `BOOT_STACK_TOP + 1 MB guard`; see [`debug-memory-oom.md`](debug-memory-oom.md) — same root-cause class as the region-boundary bugs there |
| `EC=0x21 ISS=0x4` garbage-PC instruction abort on Thread0 under heavy mmap/munmap churn (apk, forktest), **not OOM** (plenty of free PMM/heap) | Same "kernel heap overlaps boot stack" class; confirmed via lldb+gdbstub — `x29`/`x30` both garbage, kernel stack filled with high-entropy data (not a call-frame pattern) | FIXED (root layout cause) | Same `code_and_stack` fix as above. If this recurs with headroom free, suspect a **different** overlapping-region bug of the same shape — check `heap_start` vs every other fixed-address region at the current `MEMORY=` size |
| `EC=0x0E` Illegal Execution State on the *next* trap after a normal-looking exception | SPSR_EL1's IL bit (bit 20) left set across `eret` | FIXED | Clear IL bit in SPSR before every `eret` — applied in `irq_handler`, `irq_el0_handler`, `default_exception_handler`, `sync_el1_handler`, and the syscall return path |
| `EC=0x0` (unknown/undefined), `ELR` in `ssh::init_host_key()`, `FAR=0`, appears only *after* all self-tests pass | Separate, never fully root-caused — noted explicitly as unrelated to the heap-corruption bug it was found alongside | OPEN (historical; unconfirmed if still reproducible) | If seen again, treat as a fresh bug — don't assume it's the heap-corruption class just because it co-occurred with it once |
| `(isv)` assertion in QEMU HVF (`hvf.c:1883`) occurring **after boot**, not during early init | Same underlying cause as the boot-time version (GICv2 MMIO under GICv3, or the compiler choosing a post/pre-indexed addressing mode for an MMIO `write_volatile`, which always reports ISV=0 to HVF) — can surface later if a *new* MMIO call site is added without going through `mmio_r32`/`mmio_w32` | FIXED (existing call sites); WATCH (new ones) | See [`debug-boot-hang.md`](debug-boot-hang.md) row on `(isv)`; for any *new* device MMIO code, never let the compiler pick load/store addressing — use explicit inline-asm helpers with base-register-only addressing (`src/gic_v3.rs` is the template) |

## Context-switch / stack / thread-state corruption

These don't always present as a clean EC/FAR pair — several show up as
garbled output or a *later*, unrelated-looking fault, because the actual
corruption happens well before the crash.

| Symptom (crash signature) | Cause | Status | Fix |
|---|---|---|---|
| `EC=0x25 FAR=0x3fffffa0` (a user stack address), `SPSR=0x4` (EL1t), after ~1M context switches | Kernel running in **EL1t** (using `SP_EL0`, a user stack address, as its own SP) instead of EL1h; new threads were spawned with `SPSR=0`, and any stray `eret` with that value dropped the kernel into user-SP mode | FIXED | Spawn functions set `context.spsr = 0x00000005` (EL1h); `irq_handler` checks SPSR bits `[3:0]` before `eret` and corrects EL1t/EL0-with-kernel-ELR to EL1h |
| `[SGI CORRUPT]`, system threads showing user-mode SPSR/ELR values | `switch_context` restored DAIF from the saved context; if IRQs were re-enabled mid-switch (after `CURRENT_THREAD` was already updated but before the switch finished), a nested IRQ saved state into the *new* thread's context | FIXED | Removed DAIF restoration from `switch_context`; IRQs stay masked for the whole switch |
| Hang during thread cleanup | `cleanup_terminated_internal` took `POOL.lock()` without disabling IRQs; a timer firing mid-cleanup tried to re-enter the same lock via the scheduler SGI handler → single-CPU deadlock | FIXED | `IrqGuard` around `POOL.lock()` in cleanup and similar paths |
| Hang tied to SGI debug prints | `alloc::format!()` in the IRQ handler acquired the allocator lock; if the interrupted code was mid-allocation, the handler deadlocked trying to allocate again | FIXED | Disabled `ENABLE_SGI_DEBUG_PRINTS`; general rule — no allocation inside IRQ handlers |
| `[return_to_kernel] PID=N ... Stack comparison (entry vs now): CHANGED!` showing another process's PID/path bytes (e.g. `"/bin/hel"`) written into `entry_sp` | Thread-slot cleanup (TERMINATED→FREE) raced with a new spawn (FREE→INITIALIZING) claiming the *same physical stack* while the old slot's `Process` struct still held an `entry_sp` pointing at it | FIXED | `DEFERRED_THREAD_CLEANUP`: only thread 0 runs cleanup, and a terminated slot must sit for a `THREAD_CLEANUP_COOLDOWN_US` (100 ms) before recycling |
| Data abort / permission fault right after a fork or address-space switch | `activate()`/`switch_context` changed `TTBR0_EL1` without flushing the TLB (or only flushed on one of the two paths); stale TLB entries from the previous address space resolved to the wrong physical page | FIXED | `activate()`/`deactivate()` flush TLB both before and after the TTBR0 write; `switch_context`'s inline asm does `dsb ish; msr ttbr0_el1; isb; tlbi vmalle1; dsb ish; isb` |
| Phantom-frame OOM: repeated `[MMU] WARN: va=... already mapped to pa=..., wanted pa=...` then a data/instruction abort from OOM on a later syscall | Multiple `CLONE_VM` threads raced `map_user_page` on the same lazy VA during readahead; the CAS loser's frame was still tracked (`track_user_frame`) even though its PTE install lost the race — a "phantom frame", leaked but accounted allocated | FIXED | `map_user_page` returns `(frames, installed: bool)`; callers only track/keep frames where `installed` is true, and free the loser's frame immediately; readahead loops pre-check `is_current_user_page_mapped()` to skip already-resolved pages entirely |
| `bun run ...` crash with "0 free pages" OOM + many `[MMU] WARN: already mapped` lines, reproduces even after the phantom-frame fix above | `IrqGuard` was reimplemented via function pointers (`runtime().disable_irqs`/`enable_irqs`) instead of inline `mrs`/`msr daif`, breaking **nesting** — dropping an inner guard unconditionally re-enabled IRQs even when the outer context (the demand-paging exception handler) needed them to stay off, letting timer preemption back into the middle of `map_user_page` | FIXED | `IrqGuard` reimplemented with save/restore via inline asm in `crates/akuma-exec/src/runtime.rs`, matching the original `src/irq.rs` semantics |

## Verify

After changing anything in the exception path, run the boot self-test suite
(these specifically exercise fault injection and recovery):

```
========== Memory Tests ==========
...
[TEST] test_execve_clears_child_tid ... PASS
[TEST] test_map_user_page_race_leaks_frame ... PASS
[TEST] test_readahead_race_phantom_frames ... PASS
[TEST] test_irqguard_preserves_disabled_state ... PASS
[TEST] test_irqguard_nesting_preserves_state ... PASS
[TEST] test_with_irqs_disabled_nesting ... PASS
[TEST] test_map_user_page_preserves_irq_state ... PASS
Overall: ALL TESTS PASSED
```

Then confirm the specific repro from the table no longer produces a
`[Exception] Sync from EL1` line — a `[Fault] Data abort from EL0` /
`SIGSEGV` for the *offending userspace process* is the expected, healthy
outcome (kernel survives, one process dies), not a bug:

```
[Fault] Process 44 (/bin/opencode) SIGSEGV after 0.63s
```

If you suspect a live but not-yet-triggered fault-loop regression, boot with
`HVF=0` (deterministic PC) and `GDB=1`, reproduce, and confirm the kernel
`eret`s to `el1_fault_recovery_pad` rather than back into the faulting
instruction — a repeating identical `EC`/`ELR` in the log is the fault-loop
signature to watch for.

## Background

- `archive/FAR_0x5_AND_HEAP_CORRUPTION_FIX.md` — the TTBR0-validation bug and
  the eleven missing-`with_irqs_disabled` fixes across the allocator/PMM/
  process tables; also the TLB-flush-after-TTBR0-switch bugs.
- `archive/EPOLL_EL1_CRASH_FIX.md` — the full EL1-recovery saga: landing pad,
  fault-loop regressions, socket-table exhaustion, epoll/fork interaction.
- `archive/PMM_DOUBLE_FREE_AND_EL1_CRASH.md` — the `EC=0x22`/`EC=0x21`
  garbage-PC investigation; distinguishes a genuine double-free from the
  unrelated heap-slurp and boot-stack-overlap bugs that produced an
  *identical-looking* crash signature.
- `archive/STACK_CORRUPTION_ANALYSIS.md` — the thread-slot cleanup race and
  `ProcessInfo`-on-the-stack corruption pattern.
- `archive/HEAP_CORRUPTION_ANALYSIS.md`, `archive/HEAP_CORRUPTION_INVESTIGATION.md`
  — userspace bump-allocator layout sensitivity (fixed via mmap-based
  allocation) and the FAR=0x5/garbled-console IRQ-race investigation.
- `archive/CONTEXT_SWITCH_BUGS.md` — SPSR/EL1t corruption, nested-IRQ context
  corruption, and IRQ-handler deadlocks.
- `archive/QEMU_HVF_ISV_BUG.md` — the `(isv)` assertion root causes (GICv2/v3
  mismatch, trapped physical timer, cache-maintenance on unmapped VA,
  compiler-chosen MMIO addressing mode).
- `archive/KERNEL_SPLIT_BUGS.md` — `clear_child_tid` exec-reset bug and the
  `CLONE_VM` readahead phantom-frame races (bugs 2, 3, 5, 13).
- `archive/BOOT_STACK_BUG.md` — boot-time version of the stack/heap overlap;
  see [`debug-boot-hang.md`](debug-boot-hang.md) for the boot-time symptom.
