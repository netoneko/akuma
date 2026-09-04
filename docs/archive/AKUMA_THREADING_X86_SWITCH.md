# A real x86_64 cooperative context switch for `akuma-threading` — 2026-09-05

The second half of `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md`, after
`docs/archive/AKUMA_THREADING_GATE_FIX.md`'s gate fix. Where that pass made
the crate *compile* for `x86_64-unknown-none` (with the real switch handler
left as a deliberate `unimplemented!()`), this pass gives it a real, working,
boot-verified x86_64 switch — proven by two threads actually running
concurrently under it, not just by a clean build.

## What was built

A **second, independent, cooperative-only switch path**, reachable only
through `x86_64` arms of `spawn_fn`/`spawn_system_thread_fn`/`yield_now`.
It does not touch, replace, or attempt to match the AArch64
SGI-interrupt/fake-IRQ-frame mechanism those functions already had — see the
gate-fix doc and the proposal for why that mechanism has no smaller version,
only a materially different and simpler one.

**Ported, not invented**: `amd64/src/sched.rs`'s `switch_context`
(`push rbp/rbx/r12/r13/r14/r15; mov [rdi],rsp; mov rsp,[rsi]; pop …; ret`) is
already proven — its own `smoke_test` runs real cooperative+preemptive
round-robin scheduling in every `amd64/run.sh` boot. `akuma-threading` gets
the same asm, renamed to `akuma_threading_x86_switch_context` to avoid
confusion with the original, plus one new piece `amd64/sched.rs` didn't need:

**`akuma_threading_x86_thread_entry_trampoline`** — a second asm stub, needed
because `amd64::sched::Context::for_task` only ever launches
`extern "C" fn() -> !` with no per-task data (its three `smoke_test` workers
are distinct top-level functions), while `spawn_fn` must launch an arbitrary
boxed `FnOnce() -> !`. `switch_context`'s plain `ret` can't pass an argument
on its own, so `x86_build_closure_context` (the x86_64 analogue of
`Context::for_task`) preloads the closure pointer into `r12` and the
boxed-closure trampoline function pointer into `rbx` — two of the six
"callee-saved" stack slots the switch already treats as opaque — and points
the initial `ret` at this new stub, which does `mov rdi, r12; jmp rbx` to
hand the argument off in the System V first-argument register before tail-
jumping to the real trampoline.

**`akuma-exec-core::thread::Context`** is now `target_arch`-gated: the
AArch64 21-field `#[repr(C)]` struct is unchanged; the `x86_64` arm is
`{ magic: u64, rsp: u64 }` — one real field, matching
`amd64::sched::Context` exactly (`magic` kept only for interface parity with
`is_valid()`, unused by anything today).

**Scheduling decision**: not `ThreadPool::schedule_indices` (the AArch64
multi-core scheduler's SMP/priority/network-thread-boost policy — real
policy this pass does not attempt to match), but `x86_pick_next`, a plain
round-robin scan over `READY`/`RUNNING` slots — the same shape
`amd64::sched::yield_now`'s own scan already uses. "Current thread" is
tracked in a new plain `X86_CURRENT_THREAD: AtomicUsize`, not
`current_thread_id()`/`TPIDRRO_EL0`: that read is correctly stubbed to always
answer `0` off `aarch64` (`akuma-primitives::preempt::current_tid`, the
sibling fix to `akuma-mmu`'s own gate bug), so every thread would misread as
"thread 0" if the switch used it. x86_64 has no free per-thread register for
the same trick without also claiming `IA32_FS_BASE`, which
`amd64/src/usermode.rs` already needs for real userspace TLS — out of scope
here, same as no-SMP.

## What compiles now vs. what still doesn't

`spawn_fn`, `spawn_system_thread_fn`, and `yield_now` have real `x86_64`
arms. Everything downstream of the AArch64 fake-IRQ-frame layout that this
pass's scope doesn't cover — `pthread_create`-style spawning
(`ThreadPool::spawn_user_closure_initializing`), execve/fork context rewrite
(`update_thread_context`), fork's saved-user-context read
(`get_saved_user_context`), real shared-kernel SMP
(`adopt_current_as_core_idle`), and several TTBR0-keyed diagnostics
(`any_saved_ctx_on_l0`, `get_saved_kernel_resume`,
`dump_thread_resume_points`) — got the same treatment as `akuma-mmu`'s
excluded methods: gated to `aarch64`, with an `x86_64` stub that is honest
about doing nothing (`None`, a no-op, or `unimplemented!()` where no
"nothing happened" answer exists) rather than silently degrading.

**`akuma-exec` still does not build for `x86_64`.** Checked directly
(`cargo build -p akuma-exec --target x86_64-unknown-none`): it now fails at
**`akuma-user-access`** — a crate not touched by this session at all, with
its own unconditional AArch64 `asm!` (`cbz`/`b.lo`/`b.hs`/`b.ne` — invalid
x86_64 mnemonics, the identical bug class fixed twice already this session)
— and at `akuma-elf`, which calls `UserAddressSpace::alloc_and_map`/
`write_page_bytes`, methods outside the 6-method subset
`docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md` gave the `x86_64` arm. Neither
is new evidence of a regression; both are exactly the boundary "prove the
primitive, not the whole dependency chain" always implied — now measured
and named rather than assumed.

## Verification

```
cargo build --release                                     # aarch64 kernel — unchanged
cargo clippy --release                                     # clean
cargo build -p akuma-threading --target x86_64-unknown-none --release
cargo clippy -p akuma-threading -p akuma-exec-core --target x86_64-unknown-none --release  # clean
cargo clippy -p akuma-threading -p akuma-exec-core --target aarch64-unknown-none --release  # clean
cargo test --target aarch64-apple-darwin                   # full workspace, 115 suites, 0 failed
```

**The real proof — two threads actually running, not a link check.**
Temporarily wired `akuma-threading` into `akuma-amd64` (dependency +
`ThreadRuntime`/`ThreadConfig`/`ProcessHooks` registration + a probe module)
and spawned two closures via `spawn_fn`, each accumulating a **local**
variable across 5 calls to `yield_now()` — the same property
`amd64::sched::worker_body`'s own doc comment names as what distinguishes a
real context switch from a call that happens to return: if the switch failed
to preserve a thread's stack or its callee-saved registers, the final
checksum comes out wrong. Drove the scheduler from `kmain` with a plain
`yield_now()` loop until both finished. Booted under `amd64/run.sh` (`-M
microvm`, TCG). All four checks passed:

```
akuma-threading x86_64: both workers spawned   [OK]
akuma-threading x86_64: both workers ran to completion   [OK]
akuma-threading x86_64: worker 0's locals survived every switch   [OK]
akuma-threading x86_64: worker 1's locals survived every switch   [OK]
akuma-threading x86_64: switches 6
```

Both workers' checksums matched their independently-computed expected
values exactly. Full boot: **177 self-tests passed, 0 failed** (the same 177
`docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md` measured, since that probe was
already reverted by this point — confirmed by the count: 173 baseline + these
4). No regression anywhere else in the amd64 self-test suite, including
`amd64::sched`'s own independent scheduler smoke test, which shares no code
with this new path and kept passing throughout.

One real, if minor, snag on the way: the probe's first `ThreadConfig`
guessed stack sizes too close to `EXCEPTION_STACK_SIZE` (32 KiB) and tripped
`verify_stack_memory`'s "usable kernel stack too small" panic — a real,
arch-neutral safety check doing exactly its job against a bad config, not a
bug. Fixed by sizing the probe's stacks larger, not by touching the check.

Probe reverted after — `git diff --stat amd64/` is empty.

## Next

`akuma-exec`'s remaining `x86_64` blockers are now named, not just implied:
`akuma-user-access`'s own gate-fix pass (unstarted, not scoped by any
existing proposal), and extending `UserAddressSpace`'s `x86_64` method set to
cover what `akuma-elf` needs (`alloc_and_map`, `write_page_bytes`). Neither
was in scope for this pass.
