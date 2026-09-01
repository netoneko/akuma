# Boot-registered hooks

Every extracted crate that needs to call *up* into the kernel does it through one
mechanism: a `static` cell written once during boot and read lock-free forever
after. There are **21** of them. This page is the inventory and the rule for
choosing between the two kinds; the audit that produced both is
[`../../archive/AKUMA_KERNEL_HOOKS.md`](../../archive/AKUMA_KERNEL_HOOKS.md).

> **Stability: A (stable).** The mechanism has one implementation
> (`crates/akuma-not-even-once`), one contract, and host tests. The inventory
> below moves when crates are extracted; the rule has not changed.

## Why hooks exist at all

Not because of `unsafe`, and not because of layering laziness. A hook exists when
a crate **below** another needs behaviour that lives **above** it, and a direct
dependency would be a cycle:

| crate | needs | which lives in |
|---|---|---|
| `akuma-bkl` | `yield_now` | the scheduler |
| `akuma-mmu` | "is any saved context on this L0?" | the thread table |
| `akuma-elf` | reading a file | the VFS |
| `akuma-pmm` | "who else maps this frame?" | the process table |
| `akuma-locks-rw` | the orphaned-lock sweep | `akuma-ext2`'s mount registry |
| `akuma-exceptions` | process/scheduler state | `akuma-exec` |

This is worth stating because it is regularly mistaken for a symptom of
`unsafe` placement. Localizing `unsafe` does **not** dissolve any of these — the
callee is scheduler/VFS *logic*, not an unsafe operation. What the crate-split
strategy does is avoid creating *new* hooks: when `console.rs` and `platform.rs`
moved **down** into `akuma-kernel-core`, `akuma-entry` could depend downward
instead of needing a print hook back up.

## The two kinds, and which to reach for

Both wrap the same cell (`OnceCopy`) and differ only in what absence means.

| | absent means | accessor | on absence |
|---|---|---|---|
| `Registered<T>` | a boot-order **bug** | `require()` | panics, naming the `init` you forgot |
| `OnceCopy<T>` | a legitimate **state** | `get()` | every read degrades (`is_some_and`, `map_or`, `if let`) |

**The rule: if the same `cfg` that declares the static also guarantees the
registration, it is a `Registered`.** There is no absent state to handle, and
spelling one anyway converts a boot-order bug into silence.

Both failure modes are silent in the wrong direction, which is why the choice is
not a style preference. A `Registered` that degraded would let a missing runtime
surface a thousand instructions later as a wrong answer. An `OnceCopy` that
panicked would take down early boot, where the console hook legitimately is not
installed yet and printing must stay a no-op.

Registration is **single-shot and idempotent** in both: a second `register`/`set`
is ignored, not last-writer-wins. That is what lets host tests inject a table
unconditionally from parallel threads.

## Inventory

`Registered<T>` — 14. Absence is a bug; read with `require()`.

| cell | crate | registered by |
|---|---|---|
| `RUNTIME` (`ExecRuntime`), `CONFIG` (`ExecConfig`) | `akuma-exec` | `akuma_exec::init` |
| `YIELD_HOOK` | `akuma-bkl` | `akuma_exec::init` |
| `SCHED` (`SchedHooks`) | `akuma-mmu` | `akuma_exec::init` |
| `VFS` (`VfsHooks`) | `akuma-elf` | `akuma_exec::init` |
| `SURVIVING_MAPPER` | `akuma-pmm` | `akuma_exec::init` |
| `PMM_CONFIG`, `PMM_HOOKS` | `akuma-pmm` | `kernel_main` |
| `HOOKS` (`ExceptionHooks`), config | `akuma-exceptions` | `kernel_main` |
| `INODE_FREED_HOOK` | `akuma-ext2` | boot |
| `BACKSTOP` | `akuma-locks-rw` | `akuma-vfs-glue` |
| `RUNTIME` (`NetRuntime`) | `akuma-primitives` | `akuma_net::init` |
| `BOOT_TEST_HOOKS`, `RUMP_TESTS_HOOK` | `akuma-kernel-glue` | `rust_start` |

`OnceCopy<T>` — 4. Absence is legitimate; every read degrades.

| cell | crate | why it can be absent |
|---|---|---|
| `PRINT_HOOK` | `akuma-primitives` | early boot prints before any console exists; must stay a no-op |
| `CLOCK_HOOK` | `akuma-primitives` | no timebase before the timer is up |
| `HOOKS` (`SyscallHooks`) | `akuma-syscalls-glue` | the optional rump/box interception tables |
| `HOOKS` (`VfsGlueHooks`) | `akuma-vfs-glue` | ditto |

## Known deviations

Two, both in `akuma-exec`, both recorded rather than fixed as of 2026-09-01:

- **Three callbacks are hand-rolled** as `AtomicUsize` + `core::mem::transmute`
  rather than using this mechanism: `CLEANUP_CALLBACK`, `SLOT_PURGE_CALLBACK`,
  `SLOT_REAP_CALLBACK` in `threading/mod.rs`. Six `unsafe` sites that the crate's
  own `OnceCopy` would remove outright — the setter already takes `fn(usize)` and
  the transmute produces `fn(usize)`, so the type was never erased and the
  `usize` is only an internal representation. See the archive doc §4.
- **`akuma-pmm`'s `SURVIVING_MAPPER`** is `Registered::new("unused")` — a
  `Registered` whose diagnostic says it is never meant to be `require()`d. It
  should be an `OnceCopy` or carry a real message.

## Adding one

1. Decide the kind by the rule above. Do not add a third mechanism.
2. `static X: Registered<Hooks> = Registered::new("akuma-foo: Hooks not registered — call akuma_foo::init() first");`
   The diagnostic names the crate **and the call the caller forgot**; it is the
   whole reason `Registered` exists over `OnceCopy`.
3. Register from the owning crate's `init`, once.
4. Read with `require()` (or `get()` for the degrading kind) — never cache the
   table in another `static`.

There is deliberately **no** `assert_all_hooks_registered()`. With the rule above
it has nothing to check: every `Registered` already panics with a named
diagnostic on first use, and every `OnceCopy` is *supposed* to be absent
sometimes. A checker over the union would either duplicate `require()` or fire on
the legitimately-absent half.

## Background

- [`../../archive/AKUMA_KERNEL_HOOKS.md`](../../archive/AKUMA_KERNEL_HOOKS.md) — the audit, and the two hooks it found on the wrong side of the rule.
- `crates/akuma-not-even-once/src/lib.rs` — the implementation and its host tests.
- [`../../archive/AKUMA_ENTRY_EXTRACTION.md`](../../archive/AKUMA_ENTRY_EXTRACTION.md) §11 — the conversions.
