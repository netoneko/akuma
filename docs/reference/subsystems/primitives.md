# `akuma-primitives` — the dependency-free leaf crate

The bottom of the crate graph. Source: `crates/akuma-primitives/`.

> **Stability: A (stable, trust it).** Small, fully host-tested (31 unit tests),
> and every entry point either has no state or degrades to a documented no-op.
> The one thing to be careful about is **feature forwarding** — see
> "Features are load-bearing" below, where a mistake is silent rather than loud.

## The rule

**No dependencies. Ever.** `core` only — not `alloc`, not `spinning_top`, not
`log`. This is the leaf that every other crate may depend on, so anything added
here joins the whole tree's dependency closure.

A primitive that needs another crate does not belong here. A primitive that needs
something only the kernel can provide — a console, a clock — takes it as a
boot-registered [`OnceCopy`] hook and **degrades when unregistered** rather than
panicking. That degradation is the property that lets a leaf crate print at all,
and it is what several of the copies this crate replaced were each hand-rolling.

## Why it exists

Most duplicated primitives in this tree had one cause: *the canonical version
lived in a crate the duplicator could not depend on.* The bin crate owns the
console, so `akuma-exec` grew its own `StackWriter` + `safe_print!` rather than
depend on the bin crate (a cycle); `akuma-virtio` then grew a third copy as
`vprint!`. `OnceCopy` and `PreemptGuard` lived in `akuma-exec`, so `akuma-ext2`
and `akuma-net` compiled the 23.8k-line execution crate to reach ~40 lines of
RAII guard.

## Modules

| Module | Contents |
|---|---|
| `once` | [`OnceCopy<T>`] — single-shot lock-free cell for `Copy` types; [`Registered<T>`] — that cell plus the four operations a boot-registered callback table needs |
| `console` | `StackWriter<N>`, `FmtBuf<'a>`, the print hook, `safe_print!`, `print_args` |
| `irq` | all DAIF access: `IrqGuard`, `with_irqs_disabled`, `irq_save_mask`/`irq_restore`, `unmask_irqs`, `unmask_irqs_sync`, `mask_irqs_sync`, `read_daif`, `DAIF_I_MASKED` |
| `clock` | the uptime hook: `set_clock_hook`, `uptime_us`, `is_clock_registered` |
| `preempt` | `MAX_THREADS`, `current_tid`, the `PREEMPTION_DISABLED*` tables, `PreemptGuard`, `check_preemption_watchdog`, `scrub_slot` |
| `addr` | `virt_to_phys`/`phys_to_virt` (both identity) and the `DEV_*_VA` device window |

### `OnceCopy<T>`

Set once at boot, then read freely from any context including IRQ handlers. **No
spinlock by design**: a reader must never block on a writer, because reading a
boot-registered callback table from inside an IRQ that interrupted code holding
the same lock would self-deadlock on a single CPU. `set` is a release store,
`get` an acquire load; a second `set` is ignored.

This is the tree's *one* mechanism for "registered at boot, read from anywhere".
Reuse it rather than inventing a second one.

### `Registered<T>` — the callback-table form

`OnceCopy` is the cell. `Registered<T>` is the cell **plus the four operations
every kernel-callback table wants**, and the diagnostic to panic with when the
kernel forgot to register it:

| method | use |
|---|---|
| `register(v)` | publish, from the crate's `init`. Single-shot: a second call is ignored |
| `require()` | read, panicking with this cell's diagnostic. **The default** past `init` — an absent table there is a boot-order bug, not a condition to handle |
| `get()` | read as `Option`. For code that can legitimately run *before* registration and should degrade |
| `is_registered()` | non-panicking probe |

```rust
static RUNTIME: Registered<NetRuntime> =
    Registered::new("akuma-net: NetRuntime not registered — call akuma_net::init() first");
```

The message is stored whole rather than composed, so each crate keeps its own
wording and names its own `init`.

**Choosing `get` vs `require` is a real decision, so make it explicitly.**
`akuma-ext2`'s `InodeFreedHook` uses `get`, because the paths that free an inode
genuinely run before the kernel registers it (and in host tests, which have no
page cache), where "no invalidation needed" is the correct answer. Its *thread*
hooks used `get` for the same reason until 2026-08-31, when they were deleted
outright — the lock they served stopped asking liveness questions
(`archive/AKUMA_EXT2_CLEANUP.md` §4.4). `akuma-locks-rw`'s backstop kicker is
the surviving example of the same shape: unregistered, a waiter degrades to a
plain spin. `akuma-exec` and `akuma-net` use `require`, because nothing reads
their tables before `init`. Before this type the three crates each made that
call in a different house style, and only one of them wrote down why.

#### Three copies, and one of them was paying for it

| crate | before | read cost |
|---|---|---|
| `akuma-exec` | `OnceCopy` ×2 + four accessors | lock-free |
| `akuma-net` | **`Spinlock<Option<NetRuntime>>`** | **a lock on every read** |
| `akuma-ext2` | `OnceCopy` + hand-rolled `map_or` per read | lock-free |

`akuma-net` was the expensive one: **21 read sites**, dominated by `uptime_us`
(×10, called on every smoltcp `poll()`), `current_box_id` (×5, per socket op)
and `blocking_relax` (×3). Every network poll took a spinlock to read a function
pointer — in a crate whose own `NetRuntime` doc comment records moving two fields
*out* of the struct because the indirection "cost a spinlocked struct read on the
per-packet DMA path". All 21 are lock-free now.

Beyond the cost, a spinlock was the wrong mechanism there for the reason
[`OnceCopy`] states: a callback table read from an IRQ handler that interrupted
the lock holder self-deadlocks on a single core.

#### Single-shot registration is what makes host tests possible

Because `register` is idempotent, a host unit test can inject a table
unconditionally from parallel test threads with nothing to order and nothing to
race. That is how `akuma_exec::runtime::register_config_for_test` works, and it
is the supported alternative to giving production code an "is anything
registered?" branch it does not otherwise need — see
[Testing](#testing) below.

### Console: one writer, one macro

`safe_print!(N, "…", args)` formats into an `N`-byte **stack** buffer and writes
it to the registered sink. Truncates rather than allocating. CLAUDE.md
§ "Kernel conventions" forbids heap allocation on any path ending at the console
— the console is what survives when the allocator is what broke.

Two writer shapes, and only two:

- `StackWriter<const N: usize>` — owns its buffer. What `safe_print!` uses.
- `FmtBuf<'a>` — borrows the caller's `&mut [u8]` + `&mut usize`, so several
  writers can share one stack frame's space. `[PSTATS]`'s top-N line builds two
  side by side.

`print_args::<N>(args)` is the function-shaped entry point for helpers that
receive pre-built `core::fmt::Arguments` and so cannot be macros themselves
(`akuma-exec`'s `as_trace`).

**The sink is a hook, and an unregistered sink discards.** `print_str` is a no-op
before registration — never a panic — so it is safe from IRQ context and panic
handlers.

#### Registration order matters, and the failure is silent

Two callers install the sink; the earlier one wins (`OnceCopy` ignores the
second), and both point at `console::print`:

| caller | when |
|---|---|
| `src/main.rs` `rust_start` — **first statement of the kernel's Rust entry** | before any output at all |
| `akuma_exec::runtime::register` (from `akuma_exec::init`) | `src/main.rs` ~`:760` |

`rust_start`'s call is the load-bearing one. Everything between the Rust entry
and `akuma_exec::init` prints — DTB scan, memory detection, MMU and heap
bring-up, the layout assertions — and all of it would be **silently swallowed**
if the only registration were the later one. `console::print` needs no
initialisation (a const MMIO base and a volatile store), so there is nothing to
order it after.

If early boot ever goes quiet, check that call still exists and still runs first.

### IRQ: every DAIF access in the tree

Nothing outside this module touches `DAIF` in Rust. Pick by *operation*, not by
convenience:

| Entry point | Operation |
|---|---|
| `IrqGuard::new()` / `with_irqs_disabled(f)` | save + mask + `isb`, restore on drop. **Prefer these.** |
| `irq_save_mask()` / `irq_restore(d)` | save + mask, restore. **No `isb`.** Hot paths only |
| `unmask_irqs()` | `daifclr`, no barrier, no saved state |
| `unmask_irqs_sync()` | `daifclr` + `isb` — a pending IRQ is taken before the next instruction |
| `mask_irqs_sync()` | `daifset` + `isb`, no saved state |
| `read_daif()`, `DAIF_I_MASKED` | observe the mask without changing it |

All are `#[inline(always)]`, which is **load-bearing, not a hint**: they replaced
open-coded `asm!` at their call sites, several on the BKL-acquire and
per-packet-DMA paths, and the extraction is only behaviour-preserving if it emits
the same instructions with no call overhead.

> **Known divergence, deliberately preserved.** `IrqGuard` emits an `isb` after
> masking; `irq_save_mask` does not. That difference predates the merge (they were
> separate implementations) and is kept because resolving it either way is a
> behaviour change on a hot path that needs its own measurement — see the module
> header in `irq.rs` for the cost on each side. Do not "tidy" it in passing.

The one DAIF write not routed here is in `src/exceptions.rs`'s vector-install asm
block, where the surrounding `msr vbar_el1` / `isb` sequence has to stay one unit.

### Preemption

`PreemptGuard` disables scheduler preemption for a kernel spinlock critical
section, and under the BKL-drop features (`no-bkl-network`, `no-bkl-vfs`) also
masks local IRQs for the hold. That second half prevents the SMP=4 AB-BA wedge:
a BKL-free core inside an inner spinlock, a nested IRQ hard-spinning on the BKL,
and the BKL owner spinning on that same inner lock. See
[`locking.md`](locking.md).

Nesting is a **count**, not a boolean — an inner guard dropping must not
re-enable preemption while an outer one is still held. `drop` restores IRQs
*before* re-enabling preemption, the reverse of `new`'s order, so a timer IRQ
cannot land between the two.

`current_tid()` reads `TPIDRRO_EL0` and **halts the core** if the value is out of
range: every per-slot static in the kernel is indexed by it, so a corrupt value
must not be indexed with.

> **The read/write seam.** `current_tid()` (the read) is here. The *write*,
> `akuma_exec::threading::set_current_thread_register`, deliberately is **not** —
> it also re-points the per-core BKL attribution cache, which is scheduler state.
> So this crate can ask "which thread am I?" without owning "which thread is this
> core running?".

`scrub_slot(i)` clears all three per-slot records. `akuma-exec`'s
`threading::scrub_thread_slot` and `cleanup_terminated_internal` both call it —
a recycled slot inheriting a non-zero disable count is a thread the scheduler
silently never preempts.

### Addresses

`virt_to_phys` and `phys_to_virt` are **the identity** (`vaddr`,
`paddr as *mut u8`). They exist as named `#[inline(always)]` functions so the
assumption has a single home and call sites read correctly.

> **They must not become a runtime hook.** `akuma-net` once reached the kernel's
> translators through `NetRuntime` function pointers, and that indirection was
> deleted for costing a spinlocked struct read on the per-packet DMA path to
> reach two identity functions. If the kernel ever gains a non-identity kernel
> map, the honest options are a compile-time offset or a caller-passed
> translation — not a registered pointer.

`DEV_*_VA` is the fixed L0[1] device window, one 4 KB page per device, mapped
Device-nGnRnE at boot. `akuma_exec::mmu` re-exports the whole table.

## Features are load-bearing, and a mistake here is silent

`PreemptGuard`'s entire body is behind `#[cfg(kernel_smp_shared)]`, which this
crate's own `build.rs` emits from its own forwarded `smp-shared` feature. If the
forwarding chain breaks, **the guard compiles to a zero-sized no-op**: nothing
fails to build, nothing warns, and every inner-spinlock critical section in the
kernel quietly stops being protected from preemption. The symptom is a rare SMP
corruption or wedge somewhere else entirely.

| Feature | Effect here |
|---|---|
| `smp-shared` | `PreemptGuard` actually disables preemption |
| `no-bkl-network`, `no-bkl-vfs` | the guard additionally masks local IRQs for the hold |
| `extreme` | `MAX_THREADS` 256 → 64 (the per-slot statics are BSS whether used or not) |

Forwarded from the bin crate's `Cargo.toml` **directly** as well as through
`akuma-exec`, deliberately: relying on cargo's feature unification via a third
crate means a graph without that crate (`cargo test -p akuma-ext2`) silently gets
the no-op. `akuma-ext2` forwards `no-bkl-vfs` for the same reason.

**This is guarded by a boot self-test.** `test_preempt_guard_is_live`
(`src/process_tests.rs`) asserts the guard is non-zero-sized, that nesting is
counted, and that `MAX_THREADS` matches the profile. A healthy boot prints:

```
  live=true counts 0->1/2->0 held=true size=16 max_threads=256
[Test] preempt_guard_is_live PASSED
```

`size=16` is `bool` + saved `u64` DAIF. `size=0` means the forwarding broke.

## Crate graph

Nothing depends on `akuma-exec` except the kernel itself:

```
akuma-primitives  (core only)
├── akuma-virtio ──┐
├── akuma-ext2     │
├── akuma-net ─────┤ (also -> akuma-virtio)
├── akuma-vfs*     │
└── akuma-exec ────┴── akuma-isolation -> akuma-vfs
                        ^
                        └── akuma (bin)
```

\* `akuma-vfs` does not depend on `akuma-primitives` today; it has no need to.

`akuma-ext2` (`Registered` + `PreemptGuard`), `akuma-net` (`Registered` +
`PreemptGuard`) and `akuma-virtio` (`PreemptGuard` + the translators +
`DEV_VIRTIO_VA` + `console::print_str`) each reach only into this crate. Verify with `cargo tree -p <crate> --edges normal` — an
import-list grep is not enough, because a transitive edge through another crate
does not appear in one.

## Re-export map

Every symbol that moved here is re-exported from its old path, so no call site
changed. When reading old code or docs, these all resolve to this crate:

| Old path | Now |
|---|---|
| `akuma_exec::runtime::OnceCopy` | `once::OnceCopy` (still re-exported from `akuma_exec::runtime`) |
| `akuma_net::runtime`'s `Spinlock<Option<NetRuntime>>`, `akuma_exec`'s two `OnceCopy` statics, `akuma_ext2`'s `THREAD_HOOKS` | `once::Registered<T>` — **not** a re-export; the three statics were each rewritten in place, and their public accessors (`runtime()`, `config()`, `try_runtime()`, `is_registered()`) kept their names and signatures, so no call site changed |
| `akuma_exec::runtime::{IrqGuard, with_irqs_disabled}` | `irq::` |
| `akuma_exec::sync::{irq_save_mask, irq_restore}` | `irq::` |
| `akuma_exec::sync::PreemptGuard`, `akuma_net::runtime::PreemptGuard` | `preempt::PreemptGuard` |
| `akuma_exec::process::FmtBuf` | `console::FmtBuf` |
| `akuma_exec::threading::{MAX_THREADS, disable_preemption, enable_preemption, is_preemption_disabled, preemption_disabled_count, preemption_disabled_at, check_preemption_watchdog}` | `preempt::` |
| `akuma_exec::mmu::{virt_to_phys, phys_to_virt, DEV_*_VA}` | `addr::` |
| `crate::console::StackWriter` (bin), `crate::irq::{IrqGuard, with_irqs_disabled, disable_irqs, enable_irqs}` (bin) | `console::` / `irq::` |

`tprint!` stays in the bin crate (`src/console.rs`) — its `[T<secs>.<cs>]` stamp
comes from `crate::timer::uptime_us()`, and it is the bin crate's to own.

## Testing

```bash
cargo test -p akuma-primitives --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

31 tests. The non-obvious ones are worth keeping: mid-codepoint UTF-8 truncation
(a naive `as_str` panics), the unregistered-hook no-op, preemption nesting
counted rather than boolean, `preemption_disabled_at` naming the 0→1 call site
and not a nested one, the device-window page-alignment/collision check, and
`Registered`'s single-shot rule — a second `register` must **not** win. That
last one is load-bearing twice over: it is the property host tests rely on, and
it is the semantic `akuma-net` moved to when it stopped being last-writer-wins.

### Injecting a table in a host test

Prefer this over adding an "is anything registered?" branch to production code
for the sake of a test. Such a branch reads like a design principle and usually
is not: if the code only runs after `init`, the panic it guards against is
unreachable, and the guard makes every host test **skip** the very path it was
added to enable.

```rust
#[cfg(test)]
mod tests {
    // Idempotent, so call it unconditionally from every test — `cargo test`
    // runs them in parallel threads of one process and there is no ordering.
    fn setup() { crate::runtime::register_config_for_test(); }
}
```

`akuma_exec::runtime::register_config_for_test` is the worked example. It
registers only the `CONFIG` half — the 27-function-pointer `ExecRuntime` has no
meaningful stub and pure logic never needs it — from a full-literal
`ExecConfig::for_test()` kept next to the struct, so adding a field breaks it and
someone has to choose a test value rather than silently getting a zero. Its
`syscall_debug_info_enabled` is `true` so tracing paths are *executed* by tests
rather than skipped.

**The bar for host-testing kernel code is lower than it looks.**
`with_irqs_disabled` is a no-op off `target_os = "none"`, `current_tid` is in
this crate, and `akuma-exec`'s wake path is atomic-array bookkeeping — so
registering and signalling a waiter are host-testable; only a thread actually
stopping and resuming is not. Full reasoning:
`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §6.1.

## Background

- [`archive/AKUMA_PRIMITIVES_EXTRACTION.md`](../../archive/AKUMA_PRIMITIVES_EXTRACTION.md)
  — the six-rung extraction, what was duplicated, what the measurements were, and
  the two places the plan was wrong.
- [`archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.55 / §5.555 — the survey that diagnosed the missing crate, and Phase 4's
  running record. **§5.8** is `Registered<T>`: the three-way divergence, the 21
  lock acquisitions it removed, and why the line count is a wash. **§6.1** is
  the host-testability argument the injection pattern above came out of.
- [`../../runbooks/verify-trim-fat-change.md`](../../runbooks/verify-trim-fat-change.md)
  — the no-regression gate to run after adding to this crate: four clippy
  configs, the host-test count, the boot baseline.
- [`console.md`](console.md) § "Printing rules" — the no-alloc console rule and
  its exemptions.
- [`locking.md`](locking.md) — the BKL carve-outs `PreemptGuard` exists for.
