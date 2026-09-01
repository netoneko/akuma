# Boot-registered hooks: the audit

**Date:** 2026-09-01
**Scope:** every `static` in the tree that holds a kernel-supplied callback table.
**Outcome:** 21 hooks inventoried, one rule written down, two hooks converted,
two deviations recorded. Current-state reference:
[`../reference/subsystems/kernel-hooks.md`](../reference/subsystems/kernel-hooks.md).

Nothing here was on fire. The system is stable; this is hygiene on a mechanism
that had quietly grown from three users to twenty-one.

## 1. Why it was audited

`proposals/POST_REFACTORING_CLEANUP.md` item 1, written while splitting `src/`
into `akuma-kernel-core` + `akuma-kernel-glue`. The extraction added two more
`OnceCopy`-backed hooks on top of several that already existed, and the proposal
asked three questions:

1. Is each hook's failure mode (**panic** vs **degrade quietly**) deliberate, or
   copy-paste?
2. Should there be an `assert_all_hooks_registered()`?
3. Now that there are six-plus of these, could any collapse onto one mechanism?

## 2. The inventory

21 cells, in three mechanisms:

| mechanism | count | where |
|---|---|---|
| `Registered<T>` | 14 | 9 crates |
| `OnceCopy<T>` | 4 | `akuma-primitives` ×2, `akuma-syscalls-glue`, `akuma-vfs-glue` |
| `AtomicUsize` + `transmute` | 3 | `akuma-exec/threading/mod.rs` |

## 3. Answer to Q1: already consistent, except twice

The tree had **already** converged on two coherent classes without writing the
rule down:

| | absent means | accessor |
|---|---|---|
| `Registered<T>` | a boot-order **bug** | `require()`, panics naming the `init` you forgot |
| `OnceCopy<T>` | a legitimate **state** | `get()`, every read degrades |

Both failure modes are silent in the wrong direction, which is what makes the
choice load-bearing rather than stylistic. A `Registered` that degraded would let
a missing runtime surface a thousand instructions later as a wrong answer; an
`OnceCopy` that panicked would take down early boot, where the console hook
legitimately is not installed yet and printing must stay a no-op.

The rule that separates them, now in `akuma-not-even-once`'s header: **if the
same `cfg` that declares the static also guarantees the registration, it is a
`Registered`.**

Two hooks were on the wrong side of it, both in `akuma-kernel-glue`, both
`OnceCopy` statics whose `cfg` is identical to `rust_start`'s registration — so
neither had an absent state to handle:

- **`BOOT_TEST_HOOKS`** — a hand-rolled
  `.expect("boot test hooks not registered before kernel_main")`. The right
  policy spelled the long way, with a diagnostic naming no `init` call. Converted
  to `Registered::require()`.
- **`RUMP_TESTS_HOOK`** — the one that mattered. Its call site read:

  ```rust
  if !config::DISABLE_ALL_TESTS
      && let Some(f) = RUMP_TESTS_HOOK.get() { f(); }
  ```

  A drift between the static's `cfg` and the registration's would have skipped
  the **entire rump regression suite** without printing a word. Converted to
  `require()`.

Because that second change turns a silent skip into a panic on a live path
(`rump` is a default feature), it was verified by boot rather than inspection:
`SMP=2`, 165 `Result: PASS`, no panic, and `[PASS] test_rump_fd_ref_survives_fork`
present — the suite runs.

## 4. Answer to Q3: three hooks predate the mechanism

`akuma-exec/src/threading/mod.rs` stores three callbacks as `AtomicUsize` and
`transmute`s them back to `fn(usize)`:

```rust
static SLOT_PURGE_CALLBACK: AtomicUsize = AtomicUsize::new(0);
pub fn set_slot_purge_callback(cb: fn(usize)) {
    SLOT_PURGE_CALLBACK.store(cb as usize, Ordering::SeqCst);
}
// …
let purge: fn(usize) = unsafe { core::mem::transmute(purge_addr) };
```

Dating this answers "why would anyone store a callback as a `usize`":

| | date |
|---|---|
| `CLEANUP_CALLBACK` written | **2026-03-19** |
| `OnceCopy` first exists (in `akuma_exec::runtime`) | 2026-05-28 |
| `SLOT_PURGE_CALLBACK` written | 2026-08-04 |
| `Registered` (adds the diagnostic) | 2026-08-30 |

The first had no alternative when it was written. The later two copied a local
pattern that was **already obsolete inside their own crate** — `OnceCopy` had
lived one module over for two months.

There is an irony worth recording: `OnceCopy` was *born* in
`akuma_exec::runtime`, made `pub` there so `akuma-ext2` could reuse it, then
extracted to `akuma-primitives` and finally to `akuma-not-even-once`. The crate
that invented the mechanism uses it in `runtime.rs` (`ExecRuntime`, `ExecConfig`)
and **nowhere else in its own 9,300 lines**. It was extracted to help others and
never swept at home.

The conversion is free of behaviour change:

- The type was never erased — `set_slot_purge_callback` takes `fn(usize)` and the
  transmute produces `fn(usize)`. The `usize` is purely an internal representation.
- The only semantic difference is `.store()` being last-writer-wins vs
  `OnceCopy::set` first-writer-wins, and each hook has exactly **one**
  registration site, all from boot:
  `akuma-vfs-glue/src/fs.rs:72`, `akuma-kernel-glue/src/lib.rs:1073`,
  `akuma-exec/src/process/mod.rs:420`.
- The call sites check `!= 0` and skip, so absence is a legitimate state:
  `OnceCopy` + `get()` is the correct kind, not `Registered`.
- It would remove **6** `unsafe` sites, and `transmute` is strictly more
  dangerous than the API needs — it will convert any address to any signature,
  with a `!= 0` check as the sole guard.

Not done in this pass; recorded here and in the reference page's "Known
deviations".

## 5. Answer to Q2: deliberately not built

`assert_all_hooks_registered()` was proposed and **rejected**. With the §3 rule
it has nothing left to check: every `Registered` already panics with a named
diagnostic on first use, and every `OnceCopy` is *supposed* to be absent
sometimes. A checker over the union would either duplicate `require()` or fire on
the legitimately-absent half.

## 6. The finding that isn't a failure mode

`akuma-pmm`'s `SURVIVING_MAPPER` is declared
`Registered::new("unused")` — a `Registered` whose diagnostic announces that it
is never meant to be `require()`d. It is either an `OnceCopy` wearing the wrong
type or a message nobody wrote. Left as-is, recorded.

## 7. What this audit does *not* support

Hooks are regularly mistaken for a symptom of `unsafe` placement — the intuition
being that localizing `unsafe` into small crates would let callers depend
directly and retire the hooks. Checked against the actual set, and it does not
hold: `akuma-bkl` needs `yield_now`, `akuma-mmu` needs the thread table,
`akuma-elf` needs file reads, `akuma-exceptions` needs process state. In every
case the callee is scheduler/VFS **logic**, not an unsafe operation, and the
dependency direction is a property of the functionality.

What the crate-split strategy *does* do is prevent **new** hooks. When
`console.rs` and `platform.rs` moved down into `akuma-kernel-core` during the
`akuma-entry` split, `akuma-entry` could depend downward for the console the
secondary trampoline prints to. Had they stayed beside the boot assembly, that
would have been hook number 22.

## Background

- [`../reference/subsystems/kernel-hooks.md`](../reference/subsystems/kernel-hooks.md) — the current-state inventory and the rule.
- [`AKUMA_ENTRY_EXTRACTION.md`](AKUMA_ENTRY_EXTRACTION.md) §11 — the two conversions, in the extraction that prompted the audit.
- `crates/akuma-not-even-once/src/lib.rs` — the implementation; its header carries the rule, and its `registered_tests` pin single-shot idempotence and the diagnostic.
