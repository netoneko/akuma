# `src/allocator.rs` → `crates/akuma-alloc` (2026-08-31)

The kernel heap moved out of the bin crate wholesale. **This was a quarantine,
not a cleanup** — the tree's total `unsafe` did not go down, and was never
supposed to. The point was to get 20 trusted-but-difficult sites out of `src/`
and behind a crate boundary with a stated contract, so the rest of the kernel can
keep trending safe.

The interesting part is not the move. It is that the first cut of the crate was
**wrong in a way that looked right**, and §3 is the record of that.

## 1. Numbers

Regenerated with `python3 scripts/cloc_akuma.py src crates` — never incremented
by hand.

| scope | before | after |
|---|---:|---:|
| `src/` production `unsafe` | 137 | **116** |
| `crates/` production `unsafe` | 300 | **320** |
| tree production | 437 | **436** |
| enforced unsafe-free | 22 of 32 | 22 of **33** |

A move, not a removal, and the asymmetry is the whole intent. `akuma-alloc`
itself reports **20 sites in 605 production lines, 57.0% safe** — the lowest
score of any crate in the tree, and the successful outcome. It will never carry
`#![forbid(unsafe_code)]`: the `GlobalAlloc` impl, raw span claiming into Talc,
and the canary reads/writes either side of every user pointer are the crate's
reason to exist.

Boot: **306 PASSED / 0 FAILED**, unchanged. `extreme-size` 707K, unchanged.
Host suites 64 → **66** (see §4).

## 2. Final shape

```
crates/akuma-alloc/
  Cargo.toml        talc, spinning_top, akuma-primitives, akuma-pmm  — and nothing else
  src/lib.rs        605 production lines, 20 unsafe sites, no build.rs, no unstable features
```

`src/main.rs` keeps `pub use akuma_alloc as allocator;`, so `crate::allocator::`
still resolves at ~40 call sites including the boot tests — the move touched no
caller.

Rewired on the way out:

| was | now | why |
|---|---|---|
| `crate::pmm::alloc_pages_contiguous_zeroed` → `Option<PhysFrame>` | `akuma_pmm::alloc_pages_contiguous_zeroed` → `Option<usize>` | the shim existed only to wrap a raw PA in a `PhysFrame` the allocator immediately unwrapped. Working in raw PAs drops the `akuma-exec` dependency `PhysFrame` dragged in |
| `akuma_exec::mmu::phys_to_virt` | `akuma_primitives::addr::phys_to_virt` | where it actually lives; `akuma_exec` only re-exports it |
| `crate::irq::with_irqs_disabled` | `akuma_primitives::irq::with_irqs_disabled` | ditto — `src/irq.rs:17` is a `pub use` of exactly this |
| `crate::console::print` / `crate::safe_print!` | `akuma_primitives::console::print_str` / `safe_print!` | ditto |

## 3. The hooks were the wrong answer, and this is why

**The first cut preserved five call sites into `akuma-exec` and `crate::syscall`
as `OnceCopy` hooks the bin registered at boot.** 62 lines of `hooks.rs`, a
registration step in `main.rs`, and careful documentation of the
degrade-to-no-op contract. It compiled, booted 306/0, and the `[HEAP]` line came
back byte-identical.

It was still backwards. The user's read — *"do we actually need these
diagnostics? this all seems very backwards"* — was correct, and the reasoning is
worth keeping:

> A hook is the right tool when a lower layer genuinely must call back into a
> higher one. `PmmHooks`' reclaim ladder is the real thing: the PMM *must* ask
> the heap and the page cache to release memory, and only the bin knows how to
> reach them. A hook is the **wrong** tool when the code simply belongs
> somewhere else. Routing a misplaced call through a function pointer does not
> fix the layering — it preserves the inversion and adds a registration step.

None of the five was part of allocating. Four belonged elsewhere and the fifth
belonged nowhere:

| was in the allocator | now | reasoning |
|---|---|---|
| `#[global_allocator]` | `src/main.rs` | a **binary-level declaration**. A library that makes it silently decides the allocator for everything linking it |
| `#[alloc_error_handler]` + `current_process_shared` + `return_to_kernel(-12)` | `src/main.rs` | also binary-level, and its body is OOM *policy* — "kill the process, not the kernel" — which needs the process table the heap has no business knowing about |
| `syscall_counters::dump()` on allocation failure | that same handler | returning null from `alloc` reaches `handle_alloc_error` immediately, so the dump lost nothing by moving to where whole-kernel diagnostics belong |
| `current_syscall_nr()` + `current_thread_id()` on the `[HEAP]` line | **deleted** | attribution on a 5 MB-boundary progress print did not justify an allocator knowing about syscalls or threads. The line still reports the size that drove the growth, which is what actually found the talc-span-metadata runaway |

Result: **zero hooks**, `hooks.rs` deleted, and the crate needs no unstable
feature.

The generalisable test, since this will come up in the next extraction: *if the
lower layer would still work correctly with the hook permanently unregistered,
the call did not belong there.* All four surviving items pass that test trivially
— the heap allocates fine without ever printing a syscall number. `PmmHooks`
fails it, which is why it is a hook.

## 4. Getting the layering right made it host-buildable

The hooked version could not be in `default-members`: `cargo test --target $HOST`
at the repo root builds every default member, and the crate failed with

```
error: the `#[alloc_error_handler]` in this crate conflicts with allocation
       error handler in: std
```

A `default-members` exclusion was written for it, with a comment citing the
`sched-sim` CLI precedent. Both were **deleted** once the handler moved to the
bin — a plain `no_std` library with no binary-level items builds for the host
fine. Host suites went 64 → 66.

Worth noting the direction of causation: host-buildability was a *consequence* of
fixing the layering, not the motivation. The exclusion had felt like a legitimate
constraint of the domain ("it's the kernel heap, of course it can't build for the
host"). It was actually a symptom of two misplaced declarations.

## 5. No `build.rs`, deliberately

The pre-move file had exactly one cfg: `#[cfg(kernel_tests)]` on
`allocated_bytes()`. A crate only sees a cfg its **own** build script emits, and
`akuma-exec` once shipped a whole family of dormant `kernel_profile_extreme`
gates for want of one (`akuma_exec_missing_buildrs_cfg`). Rather than add a build
script plus a forwarded `no-tests` feature for a three-line atomic read, the gate
was dropped: both callers (`src/tests.rs`, `src/process_tests.rs`) are themselves
inside `kernel_tests`-gated modules, so it guarded nothing, and LTO drops the
function when nothing calls it.

**Adding a cfg to this crate later means adding a `build.rs` first.**

## 6. What the move broke, and how it surfaced

One thing, caught by a build target rather than by review:

`src/pmm.rs`'s `alloc_pages_contiguous_zeroed` lost its only production caller
when the heap stopped using `PhysFrame`. `extreme-size` builds with `-D
dead-code` and `no-tests`, so it failed with *"function
`alloc_pages_contiguous_zeroed` is never used"* — the remaining callers are all
in `src/tests.rs`. It is now `#[cfg(kernel_tests)]`, matching them.

Default `cargo build --release` never saw this: the test modules are compiled
there, so the function had callers. **A whole-file extraction needs every build
target built, not just the default one** — the profiles differ in which modules
exist, so they differ in what is dead.

## 7. Clippy

The crate inherits `[lints] workspace = true` (pedantic + nursery), which the bin
had been suppressing with crate-level `allow`s. 21 warnings appeared on code that
had not changed:

- **10 × `cast_ptr_alignment`** — the canary `u64` reads/writes at 8-byte-aligned
  offsets either side of the user pointer. `main.rs` carried
  `#![allow(clippy::cast_ptr_alignment)]` with the justification "kernel-specific:
  MMIO and error-code paths require these casts intentionally"; the same allow,
  with the same reasoning, moved with the code.
- **8 × `too_long_first_doc_paragraph`**, **3 × `missing_must_use`** — fixed
  properly rather than allowed.

## Background

- [`crate-safety.md`](../reference/crate-safety.md) § "The allocator is a
  quarantine, not a cleanup" — the current-state version of §1 and §3.
- [`memory.md`](../reference/subsystems/memory.md) § "Kernel heap allocator" —
  the subsystem doc.
- `AKUMA_EXTRACT_MMAP.md`, `AKUMA_NET_SPLIT.md`, `PMM_EXTRACT.md` — earlier
  extractions, all of which *were* about driving `unsafe` down. This one is not,
  and that difference is the point of §1.
- `SYSCALL_UNSAFE_CLEANUP.md` — the opposite technique: operations moved *into*
  the crate that owns the thing being poked, so the obligation is stated once.
  Here the whole file moved and the misplaced calls moved out.
