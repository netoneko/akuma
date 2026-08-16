# `threading::init` trampled the slots secondary cores were already running on

**Status: FIXED, 2026-08-17.** Found while diagnosing the false
`[STACK-OVERFLOW]` reports in
[`SMP_SECONDARY_IDLE_STACK_CANARY.md`](SMP_SECONDARY_IDLE_STACK_CANARY.md); this
is the second, separate bug that investigation turned up — and the reason the
boot suite never caught the first one.

## The ordering that sets it up

There are two `bringup_secondaries()` call sites in `src/main.rs`, chosen by image:

| Image | Call site | Order |
|---|---|---|
| self-test (`not(feature="no-tests")`) | `src/main.rs:848` | **BEFORE** `threading::init` |
| runtime (`no-tests`, e.g. devbox-smoltcp) | `src/main.rs:964` | after `threading::init` |

On the self-test image, then, each secondary core has already called
`adopt_current_as_core_idle` by the time init runs: it claimed a thread-pool slot
(1, 2, 3 for cores 1, 2, 3), stored `RUNNING`, latched its `ON_CPU` gate,
registered its 64 KiB `.bss` boot stack in `stacks[slot]`, and **is executing on
that slot right now**.

Init then walked every slot as if the pool were fresh.

## What it clobbered

**1. The RUNNING state.** `threading::init()` ended with:

```rust
// "Thread 0 is RUNNING (boot thread), all others are FREE"
for i in 1..MAX_THREADS {
    THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
}
```

"All others are FREE" is false the moment a core has adopted a slot. Storing
`FREE` hands a live slot back to the allocator, so the next `claim_free_slot` —
the async-main thread, in practice — is handed a slot another core is running on.
Two threads, one slot.

The code comment at `src/main.rs:840` describes the async-main thread "colliding
with a secondary's adopted idle slot and stalling the boot" under this order, and
the runtime image works around it by moving bringup later. That collision was
never diagnosed past the word "collides", and this store is the obvious mechanism
for it — but see the scope note under "Regression test": the link is inferred
from the code, not reproduced. The store is wrong on its own terms regardless.

**2. The stack, and the exception stack.** `ThreadPool::init`'s pre-allocation
loop covers `1..RESERVED_THREADS` (= 8), which spans every slot a secondary can
claim (`MAX_CORES = 8`):

```rust
for i in 1..config().reserved_threads {
    assert!(self.allocate_stack_for_slot(i, config().system_thread_stack_size), ...);
}
```

`allocate_stack_for_slot` overwrites `stacks[i]` with a fresh 512 KB PMM stack and
re-points `slots[i].exception_stack_top` at the top of it. The core keeps running
on its `.bss` stack, so the pool now describes memory nothing is executing on,
for a live core. Everything that consults `stacks[i]` — `validate_current_sp`,
the canary check, the `STACK_USAGE_PROBE` high-water scan — reads the wrong
region, silently.

It is also pure waste: three 512 KB PMM stacks allocated for slots that will
never use them.

## Why the boot suite was blind to the canary bug

`test_stack_canary_overrun_is_reported` asserts `spurious == 0`, which is exactly
the right assertion for
[`SMP_SECONDARY_IDLE_STACK_CANARY.md`](SMP_SECONDARY_IDLE_STACK_CANARY.md). It
passed anyway: by the time it ran, clobber (2) had replaced the unpainted `.bss`
stacks with PMM stacks whose canaries `allocate_stack_for_slot` *had* painted and
which nothing was running on. The suite was checking pristine memory belonging to
no one.

So the runtime image reported an overflow that had not happened, while the
self-test image checked the wrong stack and reported nothing. Neither image saw
the truth.

## Fix

One predicate, applied at the three places init touches a slot:

```rust
fn is_adopted_core_idle(slot: usize) -> bool {
    slot < MAX_THREADS && IS_IDLE_THREAD[slot].load(Ordering::Acquire)
}
```

`IS_IDLE_THREAD` is what `register_core_idle` sets, so it is already true for
exactly the adopted slots by the time init runs, and reading it needs no lock —
init calls the predicate while holding the `POOL` lock in one place and not in
the other. Both pre-allocation loops (and the `kernel_profile_extreme` warm-floor
pair) `continue` past such slots; the state-reset loop leaves their `RUNNING`
alone.

Neither `bringup_secondaries()` call site moved. The fix makes the pre-init order
correct rather than papering over it; the runtime image is unaffected either way.

One incidental link fix came with it: `secondary_stack_base` became `pub` so the
self-test can compute the expected base, which pulled its inlined `adrp` into
another codegen unit and broke the link —

```
rust-lld: error: undefined symbol: secondary_boot_stacks_shared
```

The `global_asm!` block declared `.global secondary_entry_shared` but not the
stack array, so the symbol had only ever resolved within its own CGU. Added.

## Regression test

`test_core_idle_slots_survive_init` in `src/process_tests.rs`
(`#[cfg(kernel_smp_shared)]`, runs right after `smp_shared_cores_online`, skips on
a single-CPU boot). For every registered secondary idle slot it asserts the two
things init used to break: the slot is still `RUNNING`, and the pool's recorded
stack base is the core's actual `.bss` stack (`smp_shared::secondary_stack_base(core)`).

Verified in both directions on `SMP=4`:

```
[Test] smp_shared_cores_online PASSED (3/3 secondaries on shared kernel)
  checked=3 bad=0
[Test] core_idle_slots_survive_init PASSED          # with the fix
```

Negative control — `is_adopted_core_idle` stubbed to `return false`, reproducing
the pre-fix behaviour exactly:

```
  core 1 idle slot 1: running=true recorded_base=0x4000e000 expected_base=0x403fd3c0
  core 2 idle slot 3: running=true recorded_base=0x60080000 expected_base=0x4040d3c0
  core 3 idle slot 2: running=true recorded_base=0x60000000 expected_base=0x4041d3c0
  checked=3 bad=3
[Test] core_idle_slots_survive_init FAILED
```

What catches it is the **stack** assertion: the recorded bases are PMM stacks
(`0x4000e000`, `0x600*`) where the cores' `.bss` stacks (`0x403*`) should be.

Note `running=true` in all three rows — the state check does **not** trip. By the
time the suite runs, those slots read `RUNNING` again; init's `FREE` store is a
momentary window, and whatever occupies the slot afterwards (the secondary itself
via a scheduler transition, or a new claimant) leaves it `RUNNING`. The check is
kept because it is cheap and asserts the invariant directly, but the stack-base
comparison is what actually has teeth. (The `core N → slot M` pairing also varies
run to run — cores race to `claim_free_slot` — which is why the test resolves the
slot through `core_idle_slot(core)` rather than assuming `slot == core`.)

Scope note: clobber (1) is established by reading the code — the store is
unconditional over slots a core has adopted — and clobber (2) is directly
observed above. The link from (1) to the boot stall in the `src/main.rs:840`
comment is **inferred, not reproduced**: the self-test image boots fine today with
the `FREE` store in place (286/286), so whether the window is hit depends on what
claims a slot next and when.

## Verify

```bash
INSTANCE=6 SMP=4 MEMORY=4096 cargo run --release
```

Expect `286 PASSED / 0 FAILED`, including `core_idle_slots_survive_init PASSED`.
Exactly one `[STACK-OVERFLOW]` line is expected in this suite — `tid=4`, 512 KB,
the canary `test_stack_canary_overrun_is_reported` deliberately breaks and
restores. Use `grep -a`; QEMU emits a control byte that makes plain `grep` treat
the log as binary.

## Background

- [`SMP_SECONDARY_IDLE_STACK_CANARY.md`](SMP_SECONDARY_IDLE_STACK_CANARY.md) —
  the false-`[STACK-OVERFLOW]` bug this was found underneath.
- `SMP_SHARED.md` M4 "open item" — the two-call-site bringup order and the
  original "collides with the async-main thread" note.
- `docs/reference/subsystems/smp-shared.md` — shared-kernel SMP architecture.
