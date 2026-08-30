# Splitting `akuma-exec`: survey, the three cycles, and an ordered proposal

**Date:** 2026-08-30. **Status:** **LANDED** — all five moves are in the tree.
§7 records what the plan got wrong, which was mostly in the direction of the work
being *cheaper* than budgeted. Read §7 before using §§1-5 as a guide to anything;
the survey there is still accurate, the cost estimates are not.

`akuma-exec` is 16,087 lines (12,706 production) across 33 files and **209
production `unsafe` sites** — more code than every `#![forbid(unsafe_code)]`
crate in the tree combined, and the single largest reason `crates/` sits at
37.9% enforced-safe rather than higher (`docs/reference/crate-safety.md`).

It is also the crate nobody depends on. Of the eight `crates/*/Cargo.toml` files
that *mention* `akuma-exec`, seven mention it in a comment explaining why they
deliberately do not depend on it. Only `akuma-syscalls-time` and the root bin
crate actually do. So there is essentially **one consumer** — `src/`, with ~2,500
`akuma_exec::*` call sites — and every extraction below can keep those call sites
unchanged by re-exporting from `akuma-exec`, the way `akuma-mmap` did.

## 1. What is in the crate

Measured with `python3 scripts/cloc_akuma.py` — `unsafe` is **lexed in code
context**, not grepped. "code" includes test code; "test %" is the fraction of it
that is test.

| module | code | test % | `unsafe` (prod) | what it is |
|---|---:|---:|---:|---|
| `process/` (15 files) | 6,246 | 17.5% | 56 | `Process`, the slot table + thread-identity map, fork/vfork/clone, fds, channels, signals, children, lazy-region map |
| `threading/` (3 files) | 3,749 | 18.8% | 56 | the scheduler: thread pool, context switch, SGI handler, wakers, preemption, sigframes |
| `mmu/` (4 files) | 2,107 | 7.9% | 86 | page tables, `UserAddressSpace`, ASIDs, TTBR gate, `copy_{to,from}_user` |
| `elf/` (6 files) | 1,133 | 18.4% | 6 | ELF parse + segment load, interp, initial stack |
| `sync.rs` | 933 | 50.5% | 3 | `RwSpinlock`, `KernelLock`, hold-tag profiling |
| `box_mod/` (3 files) | 496 | 55.0% | **0** | box registry, hierarchy, access checks |
| `bkl_model.rs` | 329 | `cfg(test)` | 0 | host model checker for the BKL protocol |
| `bkl.rs` | 267 | 15.0% | 1 | `enter_kernel`/`leave_kernel`, dropped-window ledger |
| `runtime.rs` | 150 | 24.7% | 1 | `ExecRuntime`/`ExecConfig` effect vtable |
| `alarms.rs` | 121 | 0% | 1 | alarm queue + hardware timer program |
| `memmath.rs` | 71 | 81.7% | 0 | two PTE-flag predicates |

Two numbers are worth stopping on. **`mmu/` holds 41% of the crate's entire
`unsafe` budget** (86 of 209) at the **lowest test coverage in the crate** (7.9%).
And `box_mod/` is 496 lines with zero `unsafe` and 55% test — it is already an
enforced-safe crate that happens to be a module.

External call sites, by namespace
(`grep -rhoE 'akuma_exec::[a-zA-Z_]+' src crates --include='*.rs'`):

```
1281 process     676 threading     388 mmu      75 bkl      46 sync
  35 runtime      13 memmath         9 alarms    1 elf_loader
```

`elf_loader`'s single external site is `akuma_exec::elf_loader::INTERP_BASE` in
`src/exceptions.rs:952`.

## 2. The internal graph, and the three cycles

Edge weights are distinct `crate::<module>::` references.

```
                    box_registry ──2──> process::Pid
                          │
                          1
                          v
  alarms ──3──> runtime <──6── elf ──10──> mmu ──4──> bkl
                  ^  ^                      │  ^       │ ^
                  20 3                    5 │  │ 9    6│ │10
                  │  │                      v  │       v │
               process <════ 118 ═══════ threading ═> sync
                  ^                          ^
                  ╚═══════════ 14 ═══════════╝
```

Three cycles, and they are not equally hard.

**(a) `sync`/`bkl` <-> `threading`** — 12 edges, all trivial. `sync` needs
`MAX_THREADS` (a const), `current_thread_id()`, and
`disable_preemption`/`enable_preemption`/`yield_now`. `bkl` needs `MAX_THREADS`
and `current_thread_id()`. That is a **five-item effect vtable**, which is exactly
what `runtime::ExecRuntime` already is.

**(b) `mmu` <-> `process`** — 10 edges, and **eight of them are in one file**:

```
mmu/user_access.rs:43   use crate::threading::set_user_copy_fault_handler;
mmu/user_access.rs:359  crate::process::address_space_owner_pid_for_fault()
mmu/user_access.rs:360  crate::process::lookup_process_shared
mmu/user_access.rs:365  crate::process::lazy_region_lookup(va)
mmu/user_access.rs:370  crate::process::LazySource::File { .. }
mmu/user_access.rs:379  crate::process::LazySource::File {
mmu/user_access.rs:419  crate::process::read_current_pid()
mmu/user_access.rs:432  crate::process::read_current_pid()
mmu/user_access.rs:433  crate::process::lookup_process_shared(owner_pid)
mmu/mod.rs:552          crate::process::lifecycle_trace_on()
```

**This is not a cycle, it is one misfiled file.** `user_access.rs` is
`copy_{to,from}_user` plus the lazy-fault resolve-and-retry path — a
syscall-boundary concern that has to know about processes and lazy regions. It
sits under `mmu/` because it touches PTEs. Move it *up* and `mmu -> process` drops
to a single call: `lifecycle_trace_on()` — and that one is not an edge either, it
is a `cfg!(feature = "debug-info")` check that `akuma-mmu` can declare for itself
(§3.5). So the cycle goes to **zero**, not to one.

The other `mmu -> threading` edges are three functions — `any_saved_ctx_on_l0`
(the TTBR free gate, `docs/archive/PAGE_TABLE_UAF_BKL_STORM.md`),
`note_current_expected_l0`, `set_user_copy_fault_handler` — all observe/register
shaped, all vtable-able.

**(c) `threading` <-> `process`** — 118 down, 14 back. The 14 back-edges:

| symbol | what it really is |
|---|---|
| `process::UserContext` (3 sites) | a `repr(C)` register frame. **Misplaced** — `threading` owns the switch that writes it |
| `find_pid_by_thread`, `table::pid_for_thread`, `lookup_process_shared` | the tid -> pid identity map |
| `is_current_interrupted` | signal delivery |
| `raise_sigchld_for_parent` | signal delivery |
| `reclaim::clear_draining` | slot teardown |
| `lifecycle_trace_on`, `dump_orphan_processes` | diagnostics |

`UserContext` moving down to a leaf kills 3 of 14 for free. The rest is the
thread-identity map (`process/table.rs`, 538 code lines, 19 `unsafe`), which is
arguably `threading`'s state living in `process`'s file — but untangling it
touches the `[KTG]`/`IDENTITY_*` accounting, load-bearing for two of the hardest
bugs on record (KTG stale-tid exit stamp; the ON_CPU scheduler race).
**This cycle should not be attempted in this pass.**

## 3. Proposal — three destinations, in order

The shape below routes each body of code to the crate that already owns its
concept, rather than minting a crate per module. Two of the three destinations
**already exist**, so two of the three steps add no manifest and no new
`cargo tree` edge.

### 3.1 `box_mod/` -> `akuma-isolation` (existing crate)

`akuma-isolation` is already `#![forbid(unsafe_code)]`, already 735 production
lines at **100% enforced-safe**, and its description is *"Process isolation
primitives: mount namespaces, network namespaces, SubdirFs"*. The box registry —
`BoxInfo { id, name, root_dir, creator_pid, primary_pid, parent_box_id }`, the
hierarchy walk, and the `box_access` permission checks — is the container
*identity* layer sitting directly on top of those namespaces. It is the same
subsystem (`docs/reference/subsystems/containers.md`) split across two crates for
no reason but history.

The decisive fact: **`akuma-exec` already depends on `akuma-isolation`.** This
step adds no edge to the graph; it moves 496 lines *into an existing dependency*.

`box_mod/`'s complete import set today is:

```
alloc::{string::String, vec::Vec}
spinning_top::Spinlock
crate::process::Pid                  # `pub type Pid = u32`
crate::runtime::with_irqs_disabled   # already a re-export of akuma_primitives::irq
```

Two small pieces of work: `Pid` must move to `akuma-primitives` (or be
redeclared), and `akuma-isolation` gains an `akuma-primitives` dependency it does
not have today — `with_irqs_disabled` is a safe `pub fn`, so `forbid` survives.
External surface is 6 call sites in `src/`, all reached through the
`akuma_exec::process::box_*` re-exports at `process/mod.rs:431`, which stay
exactly as they are.

**Why first:** cheapest possible proof of the pattern, no new manifest, and
container access control becomes independently testable at the moment
`proposals/jailed-boxes-by-default.md` and
`proposals/declarative-box-namespaces.md` are both about to change it.

### 3.2 `elf/` -> `akuma-elf` (new crate)

1,133 code lines, 18.4% test, 6 `unsafe` sites (segment writes through
`phys_to_virt` in `load.rs`/`stack.rs`).

It is already **one-way**: it references nothing in `threading` or `process`. Its
full upward surface is seven symbols —

```
mmu::phys_to_virt (6)   mmu::UserAddressSpace (1)   mmu::PAGE_SIZE (1)
runtime::runtime (4)    runtime::PhysFrame (2)
bkl::enter_kernel (1)   bkl::leave_kernel (1)
```

— and `runtime()` is used only for `read_file` / `read_at` /
`exec_bkl_drop_enabled`. Internally it has **three** consumers, all in
`process/image.rs:11,88,90`.

This one earns its own manifest rather than a merge, because there is no existing
crate whose concept is "parse an ELF and plan its segments". `phys_to_virt` and
`PAGE_SIZE` already live in `akuma-primitives` / `akuma-mmap` respectively, so the
only real friction is `mmu::UserAddressSpace`. Take it as a trait — the loader
wants *place this segment at this VA with these flags*, not a page-table type. If
that trait ever wants a second method that returns a PTE, the seam is in the wrong
place.

The 6 `unsafe` sites survive the move; this crate cannot `forbid`.

**What it buys:** the ELF loader is where the lazy-segment-boundary zeroing bug
and the RELR shared-page accumulation live (`instr_abort_relr_wedge`, still open).
Both are pure functions of a byte buffer and a VA plan, and neither is testable
today without booting.

### 3.3 `memmath.rs` -> `akuma-mmap` (existing crate)

71 lines, 81.7% test, two functions:

```rust
pub fn mapping_is_read_only_to_user(map_flags: u64) -> bool
pub fn is_shareable_mapping(map_flags: u64) -> bool
```

`akuma-mmap` already owns the region-level protection vocabulary — `user_flags`,
`is_write`, `prot_recorded` — and this is the raw-PTE-bit half of the same
question. `akuma-mmap` has an empty `[dependencies]` table and `forbid`; both
functions are pure `u64 -> bool` and preserve that.

Note this is **not** `akuma-pmm`. The PMM arithmetic that used to sit in
`memmath` — the user-page reserve, `next_reclaim_step`, the quarantine poison
codec — already migrated there for real in `PMM_EXTRACT.md` §7 Step 6, along with
its host tests. What is left is mapping predicates, which are a virtual-memory
concept, not a frame-allocator one.

### 3.4 `sync.rs` + `bkl.rs` + `bkl_model.rs` -> `akuma-bkl` (new crate)

~1,529 code lines, already the **best-tested part of the crate** (`sync.rs` is
50.5% test) and it already ships a host model checker for deadlock /
mutual-exclusion / starvation.

The cycle break is a four-item vtable registered at init, same shape as
`ExecRuntime`:

```rust
pub struct BklHost {
    pub current_thread_id: fn() -> usize,
    pub disable_preemption: fn(),
    pub enable_preemption: fn(),
    pub yield_now: fn(),
}
```

`MAX_THREADS` becomes a crate parameter rather than a dependency, and that half is
nearly free: `THREAD_TAG` and `DROPPED_WINDOWS` are *already* const-generic over
it (`ThreadTagTable<{ MAX_THREADS }>`, `DroppedWindowLedger<{ MAX_THREADS }>`).
`bkl` also uses `sync::irq_save_mask` / `irq_restore`, which are already
`akuma-primitives` re-exports — no work.

**Why this is the highest-value extraction in the list:** the BKL is where the
expensive bugs are. The dropped-window ledger, the tag=511 storms, the ON_CPU
scheduler race, the TTBR free gate — every one is a property of the protocol in
these two files, and every one cost a devbox boot (often SMP=4 under host
contention) to find. Same argument that justified `akuma-syscalls-sync`: *extract
the body of code whose bug history is a property of pure logic.* Cannot `forbid`
(3 `unsafe` sites in the raw spinlock), but the model checker becomes a
first-class gate instead of a `#[cfg(test)] mod` inside a crate that
cross-compiles.

### 3.5 `mmu/` -> `akuma-mmu` (new crate) — and **not** into `akuma-pmm`

Two moves, in this order:

1. **`mmu/user_access.rs` -> `process/user_access.rs`** (438 code lines, 15
   `unsafe`). Pure file move, no crate boundary. This is the step that makes the
   `mmu <-> process` cycle disappear; do it as its own commit so a bisect can land
   on it.
2. `mmu/{mod,types,asid}.rs` -> `crates/akuma-mmu` (~1,669 code lines, 71
   `unsafe`). Remaining upward edges after step 1 are `process::lifecycle_trace_on`
   and `threading::{any_saved_ctx_on_l0, note_current_expected_l0}`;
   `bkl::current_core_id` comes from `akuma-bkl` (§3.4) by then.

**The `lifecycle_trace_on` edge is a feature, not a callback.** Do not put it in
the vtable. Its whole definition is:

```rust
// process/mod.rs:113
pub(crate) fn lifecycle_trace_on() -> bool {
    cfg!(feature = "debug-info") && config().syscall_debug_info_enabled
}
```

There is nothing process-shaped in it — it is the `debug-info` feature ANDed with
a runtime config bit, living in `process/mod.rs` only because that is where the
first caller was. `akuma-mmu` declares its own `debug-info` feature, forwarded
from the bin crate exactly as `akuma-exec` already forwards it, and the `[AS-*]`
trace folds to a compile-time `false` without it. A `fn() -> bool` hook would be
strictly worse: it turns a constant-foldable gate into a runtime indirect call on
the address-space create/exec/free path, which is the thing the existing gate was
written to avoid (`akuma-exec`'s `debug-info` feature comment says so — the gate
was made `cfg!`-shaped precisely so the format strings leave `.rodata`).

**That leaves `akuma-mmu` with no `ExecRuntime` at all.** Its only other uses of
`crate::runtime` are re-exports of things that live lower down —
`PhysFrame`/`FrameSource`/`track_frame` (`akuma-mmap`, `akuma-pmm`),
`with_irqs_disabled`/`IrqGuard` (`akuma-primitives`) — plus one
`runtime().print_str` in `asid_exhausted_warn` (`mmu/mod.rs:527`), which is a
fixed `&'static str` and should be `safe_print!` regardless of this split
(`docs/reference/subsystems/console.md` § "Printing rules"). So the crate's
manifest is:

```toml
akuma-primitives  # irq, console, addr
akuma-mmap        # PhysFrame, PAGE_SIZE
akuma-pmm         # frame alloc/free, track_frame
spinning_top
[features] debug-info = []   # forwarded from the bin crate
```

and its only host hook is a **two-item** scheduler vtable for the TTBR free gate
(`any_saved_ctx_on_l0`, `note_current_expected_l0`) — the one genuinely upward
dependency, and the one worth the indirection because the gate must ask the
scheduler a question only the scheduler can answer.

**Why a new crate and not a merge into `akuma-pmm`.** The suggestion is natural —
both are "memory" — but it inverts the property the extraction programme exists to
create. `akuma-pmm` is *physical*: bitmap allocator, frame tracking, UAF
quarantine, CoW refcount ledger. 1,139 code lines, **5 `unsafe` sites**, 96.2%
safe production code, and a stated design invariant in its own header that it
takes **no dependency on `akuma-exec`**, achieved with `Registered` degrade-hooks.
`mmu/` is *virtual*: page tables, `UserAddressSpace`, ASID allocation, TTBR
tracking, TLB maintenance. 2,107 code lines and **86 `unsafe` sites**.

Merging them would take `akuma-pmm` from 5 `unsafe` sites to 91, drag its
dependents (`akuma-mmap`, `akuma-ext2`, `akuma-virtio`, `akuma-exec`) into a crate
that needs scheduler hooks to run its TTBR gate, and break the no-`akuma-exec`
invariant outright — `any_saved_ctx_on_l0` is scheduler state.

That is precisely the lesson `crate-safety.md` draws from the `akuma-net` split:
*"irreducible is a property of a body of code, not of a crate."* `akuma-net`
became `forbid`-able by moving its `unsafe` body **out** into `akuma-net-nic`.
Folding `mmu` into `akuma-pmm` does the same operation backwards — it merges the
unsafe body into the safe one.

The seam that already works is physical vs virtual, and it wants three crates,
mirroring the networking family:

```
akuma-pmm    physical frames        1,139 lines,  5 unsafe   (leaf)
akuma-mmap   region records           ~700 lines,  0 unsafe   (zero deps, forbid)
akuma-mmu    page tables + ASID     ~1,669 lines, 71 unsafe   (new — where it concentrates)
```

**What §3.5 buys:** `UserAddressSpace`'s `Drop`, the deferred-free path
(`free_or_defer_as_frames`, `drain_pending_ttbr_frees`), and the per-core
`ACTIVE_L0`/`PREV_L0` free gate are the mechanism behind the page-table UAF BKL
storm and the F8 saved-context TTBR0 gate — and `mmu/` is the least-tested
directory in the crate at 7.9%. The free gate in particular is a decision function
over `(l0_phys, per-core published L0s, saved contexts)`, exactly the shape that
hosts well.

## 4. What is explicitly NOT proposed

- **Splitting `threading` from `process`.** §2(c). The back-edges run through the
  thread-identity map, and that accounting is load-bearing for the KTG stale-tid
  and ON_CPU races. Move `UserContext` down to a leaf as a standalone cleanup;
  leave the rest.
- **`alarms.rs` (121 lines) as a crate.** Too small to earn a manifest, and it has
  9 external call sites all in `src/timer.rs` / `src/smp_shared.rs`. If it moves
  anywhere it is `akuma-timer`, which already owns CNTV/PL031 and the tick policy
  — but that is a separate question from this split.
- **A `forbid(unsafe_code)` target for `akuma-exec` itself.** Not reachable and not
  the point. After all five steps the crate is still ~11,200 code lines and **128**
  `unsafe` sites — trap frames, the thread-identity map, context switch,
  `user_access`. Those are irreducible in the `crate-safety.md` sense.

**Be honest about the headline.** Arithmetic, at 100% of the plan:

| | code lines | `unsafe` (prod) | per kloc |
|---|---:|---:|---:|
| `akuma-exec` today | 16,087 | 209 | 13.0 |
| moved out | 4,898 | 81 | |
| `akuma-exec` after | **11,189** | **128** | **11.4** |

Where the 4,898 goes: 1,669 to `akuma-mmu` (71 `unsafe`), 1,529 to `akuma-bkl`
(4), 1,133 to `akuma-elf` (6), 496 to `akuma-isolation` (0), 71 to `akuma-mmap`
(0). Three new manifests (`akuma-elf`, `akuma-bkl`, `akuma-mmu`), two existing
crates grown.

**It creates no new `forbid` crate — but it does make two existing ones
fatter, and that is the number `crate-safety.md` actually tracks.** Enforced-safe
code is the metric, not enforced-safe crate count:

| | enforced-safe code | of `crates/` |
|---|---:|---:|
| today | 16,052 | 42,317 = **37.9%** |
| after | 16,619 | 42,317 = **39.3%** |

`box_mod`'s 496 lines and `memmath`'s 71 land inside `forbid` boundaries, and the
denominator does not move — the other 4,331 lines are carved out of `akuma-exec`
into new crates, so they stay inside `crates/`. Tree-wide (`crates/` + `src/` =
86,591) that is 18.5% -> 19.2%.

Two honest caveats on that gain. It is **+1.4 points, not a step change** — the
big bodies being moved (`mmu`, `bkl`, `elf`) all carry `unsafe` and land in crates
that cannot `forbid`. And by *crate count* the ratio gets worse, 18-of-25 to
18-of-28, which is exactly why "code in those crates" is the figure that document
leads with.

The real structural win is **concentration**, the `akuma-net-nic` pattern:

| crate | `unsafe` per kloc after |
|---|---:|
| `akuma-mmu` (new) | **42.5** |
| `akuma-exec` (after) | 11.4 |
| `akuma-pmm` | 3.2 |
| `akuma-elf` (new) | 5.3 |
| `akuma-bkl` (new) | 2.6 |
| `akuma-isolation`, `akuma-mmap` | 0 (`forbid`) |

86 of the tree's 319 production `unsafe` sites are page-table and user-copy
mechanics. Today they are diluted across a 16-kloc crate at 7.9% test coverage.
After, 71 of them sit in a 1.7-kloc crate whose entire job is page tables — which
is the state in which `akuma-net` became reviewable, and then `forbid`-able.

`akuma-exec` is still the largest crate in the tree afterwards, by a wide margin.

## 5. Verification (per step)

Same gate for each:

```bash
# host
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo clippy --release

# the split must be a no-op for size and for the boot suite
cargo build --release && ls -l target/*/release/akuma
scripts/build_extreme_size.sh          # 4.0 MB floor must hold
cargo run --release                    # grep -ac PASSED, compare to baseline
SMP=4 cargo run --release              # BKL and mmu steps especially
python3 scripts/cloc_akuma.py src crates   # regenerate crate-safety.md numbers
```

Plus, per step:

- **`box_mod` -> `akuma-isolation`:** the 32 existing tests must run under that
  crate's `forbid(unsafe_code)`. `acceptance/` box playbooks.
- **`akuma-elf`:** self-host build (`scripts/run_selfhost_kernelbuild.py`) — the
  loader is on the path for every `rustc` exec.
- **`memmath` -> `akuma-mmap`:** its 4 host tests move with it; `akuma-mmap`'s
  empty `[dependencies]` table must stay empty.
- **`akuma-bkl`:** `bkl_model.rs` must run as a plain host test with no
  `cfg(target_os)` gymnastics. Then SMP=1/2/4 boot suite, and a real contention
  run — the tag=511 storm work says A/B against a HEAD worktree, not against
  memory.
- **`akuma-mmu`:** the `user_access.rs` move is verified by the self-host build
  (`[FILL-SHORT/prefault]` is the failure signature); the crate move by `[AS-*]`
  trace parity and `pending_ttbr_free_stats()` across a boot.

`cargo build --release` at each step must produce a kernel of **the same size**; a
size delta means something was dropped or duplicated, not moved.

## 6. Background

- `docs/archive/AKUMA_NET_SPLIT.md` — the model this follows, and the source of
  the "irreducible is a property of a body of code, not of a crate" argument that
  §3.5 leans on.
- `docs/archive/AKUMA_EXTRACT_MMAP.md` — `akuma-mmap`, the previous extraction
  *out of* `akuma-exec`, and the precedent for "sits below, re-exported, zero
  call-site churn".
- `docs/archive/PMM_EXTRACT.md` §5, §7 — why `memmath` is what is left of a bigger
  module, and where the PMM half of it went.
- `docs/reference/crate-safety.md` — where the 209 sites and the 37.9% come from.
- `docs/reference/subsystems/smp-shared.md` — the BKL semantics §3.4 extracts.
- `docs/reference/subsystems/containers.md` — the box registry §3.1 moves.

---

## 7. What actually landed (2026-08-30)

All five moves are in. The plan was directionally right and wrong about cost in
four places, three of them cheaper than budgeted.

### 7.1 The order in §3 is topologically impossible

`elf` is listed second because it looked like the cleanest one-way body — which
it is. But it needs `bkl::enter_kernel` and `mmu::UserAddressSpace`, so it cannot
move until **both** of those crates exist. Extracting it second would have meant
inventing two traits and then deleting them. The order that works is the reverse
of "cheapest first":

```
1. box_mod   -> akuma-isolation   (no new crate, no new edge)
2. memmath   -> akuma-mmap        (no new crate)
3. sync/bkl  -> akuma-bkl         (depends only on akuma-primitives)
4. user_access.rs -> process/     (pure file move, no crate boundary)
5. mmu       -> akuma-mmu         (needs akuma-bkl from step 3)
6. elf       -> akuma-elf         (needs akuma-bkl AND akuma-mmu)
```

### 7.2 `akuma-bkl` needed one hook, not four

§3.4 budgeted a four-item `BklHost` vtable. The real number is **one**.
`MAX_THREADS`, `current_tid`, `disable_preemption`, `enable_preemption`,
`irq_save_mask`/`irq_restore`, `PreemptGuard` and `safe_print!` had *already*
migrated to `akuma-primitives` in earlier rounds — `akuma-exec`'s
`threading::{disable_preemption, enable_preemption}` are literally
`pub use akuma_primitives::preempt::…`, and `current_thread_id()` is a one-line
wrapper over `preempt::current_tid()`. The only genuinely upward call left is
`yield_now`, the scheduler's own entry point.

The lesson generalises: **before designing a vtable to break a cycle, check
whether the leaf crate already exports the thing.** Four of the five items were
already there and the survey in §2 counted them as `threading::` because that is
the path the call site spelled.

### 7.3 `akuma-mmu` needed no `ExecRuntime` at all

Predicted in §3.5 and confirmed. After `user_access.rs` moved up and the trace
became a feature, the crate's only remaining `runtime()` reference was one
`print_str` in `asid_exhausted_warn` — a fixed `&'static str`, converted to
`safe_print!` per the console rule. Final manifest is
`{akuma-primitives, akuma-mmap, akuma-pmm, akuma-bkl, spinning_top, log}` plus a
**two**-item `SchedHooks`.

`akuma-elf` did need a table: four VFS callbacks (`read_file`, `read_at`,
`resolve_file_id`, `exec_bkl_drop_enabled`). Those `require()` rather than
degrade — a stub registration turns inode-backed reads into silent zeros, which
is the `[FILL-SHORT/prefault]` self-host ICE.

### 7.4 `memmath` is gone — the gate became a parameter

§3.3 said all 71 lines go to `akuma-mmap`. The first attempt moved only half and
left `is_shareable_mapping` behind in `akuma-exec`, on this reasoning: it is the
pure predicate ANDed with `config().shared_file_pages_enabled`, and `akuma-mmap`
has an empty `[dependencies]` table by design, so it cannot read an `ExecConfig`.

**That reasoning was wrong, and instructively so.** The config read was never part
of the decision — it was one `bool` the decision consumed. Taking it as an
argument moves the whole predicate down:

```rust
// akuma_mmap::user_flags
pub const fn is_shareable_mapping(flags: u64, shared_file_pages_enabled: bool) -> bool {
    shared_file_pages_enabled && is_read_only_to_user(flags)
}
```

The switch is now read at the single call site that owns it — a small wrapper in
`src/file_page_cache.rs`, which was *already* the import path both real callers
(`exceptions.rs`, `process_tests.rs`) went through. So no call site changed,
`crates/akuma-exec/src/memmath.rs` is **deleted**, and `akuma-exec` loses a module
rather than keeping a three-line one.

Both moved functions now sit in `akuma_mmap::user_flags` beside `is_write`,
`is_exec` and `from_prot` — the vocabulary they read. `mapping_is_read_only_to_user`
became `is_read_only_to_user` and dropped a fourth private copy of `AP_MASK` on
the way.

**The general shape is worth naming**, because it is the same one this document
describes for the PMM's arithmetic in `PMM_EXTRACT.md`: *a pure function that
reads its own configuration looks like it depends on the world.* It does not. It
depends on a value. Injectable config was originally introduced here to make the
function host-testable (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.11) — a real
improvement at the time — but injection is a heavier tool than a parameter, and
having reached for it once, the coupling it created is what stranded the function
two extractions later. **Before adding a hook or an injectable table to a crate
boundary, check whether the callee could just take the value.**

Two things fell out of the parameter form that the injected form could not have:

1. **The tests became pure.** They pass `true`/`false` instead of calling
   `register_config_for_test()`.
2. **The kill switch is tested for the first time.** Proving
   `SHARED_FILE_PAGES_ENABLED = false` disables sharing needed an injected config
   with the flag *off*, and `register_config_for_test` hardcodes it **on** — so
   the one behaviour that switch exists to provide had no test at all.
   `the_kill_switch_makes_every_mapping_unshareable` is now that test.

Writing the move also surfaced a real gap in the pair's contract. `is_write` and
`is_read_only_to_user` are **mutually exclusive but not exhaustive** — `AP` has
three EL0-reachable values, and the third (`AP_RO_EL1` = `user_flags::NONE`, the
`PROT_NONE` encoding) answers `false` to both. Code reaching for `!is_write(..)`
as a stand-in for the sharing predicate would wrongly treat a `PROT_NONE` page as
shareable. Neither function had a test tying them together while they lived in
different crates; `write_and_read_only_are_exclusive_but_not_exhaustive` is now
that test. (Its first version asserted complementarity and failed immediately on
`from_prot(0)` — which is how the gap was found.)

Two stale docs were corrected in the same pass, both pre-dating this change:
`akuma-pmm`'s header claimed `memmath` still held the mapping predicates and that
they were "always `akuma-exec`'s own"; and a comment in `process_tests.rs` pointed
the user-page-reserve arithmetic at `akuma_exec::memmath`, which had already moved
to `akuma-pmm` in `PMM_EXTRACT.md` §7 Step 6.

### 7.5 The `[AS-*]` trace lost its runtime half, deliberately

`akuma_exec::process::lifecycle_trace_on()` was
`cfg!(feature = "debug-info") && config().syscall_debug_info_enabled`.
`akuma-mmu`'s replacement is the `cfg!` alone. With the feature off — every
shipping profile — behaviour is identical and the whole thing constant-folds. With
it on, the trace no longer also consults `syscall_debug_info_enabled`. That is a
narrow, deliberate behaviour change on a debug-only path, taken because the
alternative was a `fn() -> bool` hook on the address-space create/exec/free path,
which is precisely the indirect call the `cfg!` shape exists to avoid.

`as_trace` also had to become `pub` **and** `#[inline]`. Inside one crate the
empty body folded for free; across a crate boundary the `#[inline]` is what lets
LLVM delete the caller's `format_args!` construction too.

### 7.6 Verification

| gate | result |
|---|---|
| host tests | **1,020 pass**, 28 test binaries, 0 failures |
| per-crate `clippy -- -D warnings` (all 28) | clean |
| `clippy --release`, `clippy --profile extreme-size` | clean |
| `cargo build --release` | builds |
| `scripts/build_extreme_size.sh` | builds, 707 KB ELF |
| boot suite, `MEMORY=2048` | 178 `[TEST]`, `[FS Tests] Complete: 6 passed, 0 failed`, 0 panics |

Test counts reconcile exactly, which is the check that nothing was silently
dropped: `akuma-exec` 219 -> 155, and 38 + 15 + 11 = 64 appear in
`akuma-bkl` / `akuma-elf` / `akuma-mmu`. `akuma-isolation` 43 -> 75 (box's 32),
`akuma-mmap` 32 -> 38 (the moved predicate + gate tests, plus three new ones).

**On booting under HVF:** the first run died with
`Assertion failed: (isv), function hvf_handle_exception, hvf.c:2437`. That is not
a kernel bug and not this change — `scripts/cargo_runner.sh` prints a banner
predicting it, because a **tests-carrying build under HVF with the default
`MEMORY=256M`** always hits it (`docs/archive/QEMU_HVF_ISV_BUG.md` "Root cause
5"). `MEMORY=2048` is the fix. Worth restating here because the assertion looks
exactly like a page-table regression, which is the most alarming thing this
particular change could have caused.

### 7.7 The image got **214 KB smaller**, and that needs explaining

`cargo build --release`, same features, same toolchain:

| | bytes | `.text` |
|---|---:|---:|
| HEAD (`ca31fb65`) | 4,327,864 | 2,851,392 |
| after the split | 4,113,400 | 2,648,604 |
| delta | **-214,464 (-4.96%)** | **-202,788** |

§5 says "a size delta means something was dropped or duplicated, not moved" — so
this had to be chased. Nothing was dropped: every key symbol is present, the boot
suite runs *more* `[TEST]` lines than the log that died early, and the host test
count reconciles. `.rodata` went *up* 3,360 bytes and `.data`/`.bss` are flat
(+112/+128); a real feature loss would show as `.rodata` and `.data` falling
together.

The cause is **ThinLTO inlining less across the new crate boundaries**. The
profile is `lto = "thin"` and its comment in `Cargo.toml` names exactly this:
cross-crate calls are inlined "only if the callee carries `#[inline]` — which
just ~10% of `crates/`' `pub fn` do, while the hot paths (mmu, the fault helpers)
all live there." Those hot paths just crossed a crate boundary. `.text` shrinking
by 200 KB is the *duplication from inlining* going away, not code.

**This is a latent performance regression, not a win, and it is not measured.**
The fault path (`map_user_page`, `resolve_user_leaf`, the `copy_*_user` helpers)
is the most-executed code in the kernel and it is now behind a call. Nothing in
the boot suite times it. Before treating the smaller image as good news, someone
should A/B a fault-heavy workload — `scripts/run_selfhost_kernelbuild.py` is the
obvious one, since `rustc` is ext2 + mmap bound — and if it regresses, the fix is
`#[inline]` on the handful of `akuma-mmu` entry points the fault path calls, not
undoing the split.

### 7.8 Still open

- **The `threading` <-> `process` cycle**, untouched as planned (§4). 14 back-edges
  through the thread-identity map.
- **`akuma-pmm` (5 sites) and `akuma-bkl` (4 sites)** are now small enough to audit
  for `forbid(unsafe_code)`. Deferred deliberately until the moves are confirmed
  regression-free — an `unsafe` audit and a code move in the same change would
  make a bisect useless.
- **The inlining measurement in §7.7.**
- Enforced-safe code is **16,592 of 42,634 (38.9%)**, from 16,052 of 42,317
  (37.9%). Note the crate *ratio* fell, 18-of-25 to 18-of-28, exactly as §4
  predicted — three new crates, none of them `forbid`-able.
