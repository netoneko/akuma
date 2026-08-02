# `CURRENT_TRAP_FRAME[tid]` survives thread death — 2026-08-02

A per-thread pointer to a live EL0 trap frame is published on every syscall and
cleared on exactly one path. Process exit does not take that path, and thread
teardown does not clear it either — so a recycled thread slot inherits a pointer
into a kernel stack that has already been freed to the PMM.

Found while investigating
[`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1 (the `big.rs`
"EL0 return with a kernel register context" failure). **It is not established that
this causes §5.1** — see §4. It is reported here as a defect in its own right.

## 1. The mechanism

`CURRENT_TRAP_FRAME` (`crates/akuma-exec/src/threading/mod.rs:307`) is a lock-free
`[AtomicU64; MAX_THREADS]` (`MAX_THREADS = 64`,
`crates/akuma-exec/src/threading/types.rs:11`) holding, per thread slot, the address
of that thread's live `UserTrapFrame`. The frame itself lives on the thread's own
kernel stack at `SP_EL1 - 832`, pushed by the EL0 sync vector
(`src/exceptions.rs:104-171`).

There is exactly one writer of each kind:

| | function | only call site |
|---|---|---|
| set | `set_current_trap_frame` (`threading/mod.rs:3284`) | `src/exceptions.rs:2726` |
| clear | `clear_current_trap_frame` (`threading/mod.rs:3292`) | `src/exceptions.rs:2781` |

Both sit in the SVC branch of `rust_sync_el0_handler_inner`, with the syscall
dispatch between them:

```
2726   set_current_trap_frame(frame)
2737   let ret = crate::syscall::handle_syscall(syscall_num, &args)
2748   if let Some(proc) = current_process_shared() {
2749       if proc.exited {
2772           akuma_exec::process::return_to_kernel(exit_code);   // ← does not return
2773       }
2779   }
2781   clear_current_trap_frame()
```

`return_to_kernel` at `:2772` never comes back — it unwinds into the scheduler. So
**every process that exits through a syscall leaves `CURRENT_TRAP_FRAME[tid]`
pointing at its own kernel stack.** That is the normal exit path for `exit` and
`exit_group`, i.e. essentially every process.

Teardown does not repair it. `cleanup_terminated_internal`
(`threading/mod.rs:1067-1200`) resets a specific list of per-slot state —
`TERMINATION_TIME`, `THREAD_CONTEXTS` (zeroed), `PENDING_SIGNALS`, `PENDING_KILL`,
`PREEMPTION_DISABLED` / `_SINCE`, `THREAD_SIGALTSTACK_{SP,SIZE,FLAGS}` — and then
calls `pool.free_stack_for_slot(i)` (`threading/mod.rs:1889`), returning the kernel
stack to the allocator. `CURRENT_TRAP_FRAME[i]` is not in that list.

Net effect after a slot is recycled: the new thread starts life with a non-zero
`CURRENT_TRAP_FRAME` entry aimed at memory that has been freed and may already be
handed out to something else.

## 2. Who reads it

Three readers, all of which treat a non-zero entry as a valid frame:

- `get_saved_user_context` (`threading/mod.rs:3384`, deref at `:3393`) — the
  fork/vfork/clone register capture. Called from `clone_thread`
  (`crates/akuma-exec/src/process/mod.rs:2686`), `fork` (`:2385`), `vfork` (`:2560`).
- `current_trap_frame_elr` (`threading/mod.rs:3304`, deref at `:3313`) — syscall
  errno diagnostics.
- `dump_thread_resume_points` (`threading/mod.rs:3357`) — the heartbeat hang dump.

None of them validate the pointer.

## 3. Why the blast radius is smaller than it looks

`get_saved_user_context` — the dangerous reader, since its result becomes a child's
initial `UserContext` and is ERETed to — only consults `CURRENT_TRAP_FRAME` when
`thread_id == current_thread_id()` (`threading/mod.rs:3390`):

```rust
if thread_id == current_thread_id() {
    let frame_ptr = CURRENT_TRAP_FRAME[thread_id].load(Ordering::Acquire);
    if frame_ptr != 0 { /* deref */ }
}
```

A thread calling `clone`/`fork` is by definition inside the SVC branch, so
`set_current_trap_frame` at `exceptions.rs:2726` has just overwritten the stale
entry with its own valid frame. **On the fork/clone path the staleness is masked.**

The two diagnostic readers are not masked: `current_trap_frame_elr` and
`dump_thread_resume_points` can be reached outside an SVC window and will
dereference a freed stack. `dump_thread_resume_points` in particular iterates every
non-`FREE` slot, so it can touch a stale entry belonging to a *different* thread —
and it is called precisely when the system is already wedged.

## 4. Relationship to `BKL_RUSTC_SCALING_BASELINE.md` §5.1 — unproven

§5.1 reports a `clone_thread`-heavy workload (rustc `opt cgu.N` workers) faulting
with `fault_pc=0x4016f138` (kernel text), `user_sp=0xd4`, `x1=0x12` (the thread's
tid), `x3=0xd4` (the process's pid). The shape — a child ERETing with a kernel
register context — is what a garbage `get_saved_user_context` result would produce,
which is why this was the first thing checked.

But §3 shows the fork/clone path masks the stale pointer, so **this defect does not
explain §5.1 as written.** Either there is a path to `get_saved_user_context` that
does not go through `exceptions.rs:2726`, or §5.1's corruption has a different
source. Both remain open.

Also unresolved from §5.1: the addresses could not be symbolized. None of the
checked-in binaries reproduce the log's build — the freshest
(`target/aarch64-unknown-none/release-smp-shared/akuma`) resolves `0x4016f138` to
`rump_proxy::proxy_bind+0x250`, which is not plausible for that trace. Treat any
symbolization of §5.1's addresses as unverified.

## 5. Adjacent observations

Recorded while tracing this; not verified as bugs.

**5.1 The two 832-byte frame layouts are not interchangeable.** The EL0 sync frame
(`UserTrapFrame`, `crates/akuma-exec/src/threading/types.rs:69-83`) and the IRQ frame
(`src/exceptions.rs:280-333`, `setup_fake_irq_frame` at `threading/mod.rs:1535`) are
both 832 bytes but differ in layout — notably `sp_el0` at +248 / `elr_el1` at +256 in
the sync frame versus SPSR at +248 / `SP_EL0` at +256 in the IRQ frame, and the NEON
block starting at +304 versus +288. Handing one to a consumer of the other yields a
swapped PC/SP and a garbage SPSR.

The in-tree comments are already wrong about this: `src/exceptions.rs:271-272` claims
`[sp+288]: x10,x11` and `[sp+304]: Q0-Q31`, and `:426-428` claims
`[sp+832..847]: x10, x11`, but the actual push order in `irq_el0_handler` puts NEON at
+288 and x10/x11 at +816. **The comments should be corrected regardless of whether
any real mixing path exists.**

**5.2 `Process::thread_id` clearing is inconsistent, but the trampoline hazard looks
covered.** `entry_point_trampoline` (`process/mod.rs:2781-2787`) locates its `Process`
by scanning for `p.thread_id == Some(tid)` and takes the first ACTIVE match, so a
zombie retaining a recycled tid would hand the new thread the wrong `UserContext`.
The kill paths clear it — `process/mod.rs:1246`, and `signal.rs:89` with the explicit
comment *"prevent entry_point_trampoline from matching this zombie"*, plus
`signal.rs:136`. Whether every normal-exit path also clears it was not fully traced.

## 6. Suggested fix

Clear the entry at both ends rather than relying on the single happy-path site:

1. Add `CURRENT_TRAP_FRAME[i].store(0, Ordering::Release)` to
   `cleanup_terminated_internal` alongside the other per-slot resets — this is the
   authoritative fix, since it covers every way a slot can be recycled.
2. Clear it in `return_to_kernel` (or immediately before `exceptions.rs:2772`) so the
   window between exit and cleanup is not exposed to the diagnostic readers.
3. Optionally have `spawn_user_closure_initializing` zero it when it claims a slot
   (`FREE → INITIALIZING`, `threading/mod.rs:803`), as a belt-and-braces measure.

Per `docs/reference/subsystems/` convention a kernel change here needs a boot-suite
self-test in `src/process_tests.rs`. A test that spawns a thread, exits it through
`exit_group`, forces slot recycling, and asserts `CURRENT_TRAP_FRAME` for that slot
is zero at the new thread's first syscall would cover items 1 and 3.

## Background

- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §5.1 — the failure
  that prompted this trace.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.1.1 — the
  process-table-freed-under-running-threads hazard ("the documented safety argument
  covers self-free, not peer-free"), the same lifetime family as this.
- `docs/reference/subsystems/exceptions.md` — trap frame and vector reference.
