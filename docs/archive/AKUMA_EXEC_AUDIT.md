# `akuma-exec`: unsafe audit

**Date:** 2026-09-01
**Crate:** `crates/akuma-exec` — 9,311 production lines, the largest in the tree.
**Status of the crate:** stable and in production on every profile. Nothing in
this document is an outage or a known-live corruption. It is an audit of a crate
that, by its owner's own description, "was never really audited" — the isolation
half was cut out into `akuma-isolation` and some other pieces moved, but the
remaining `unsafe` had never been enumerated as a set.

**Result:** 123 `unsafe` sites classified into 8 kinds. **All six `&self` ->
`&mut` casts are gone** (§4/§5a/§5c-bis/§5c-ter) — none legalised with
`UnsafeCell`; each became an atomic or moved inside the lock that already
guarded it.

**Then §6 was worked in earnest (2026-09-02):** `akuma-user-access` extracted
(§6.A), `process/mod.rs` pushed down 29 → 12 (§6.B), and **`akuma-threading`
extracted** (§6.C) — the scheduler, its 57 `unsafe` blocks, into a crate that
can't forbid so `akuma-exec` can head toward one. The crate went from ~126
`unsafe` blocks to **37**; §6.D (`table.rs`) and the EL0-entry cluster are what
stand between it and `#![forbid(unsafe_code)]`.

**§6.E landed (2026-09-02), and the "unsafe-free after A–C" claim below was
wrong.** `akuma-el0-entry` extracted (`enter_user_mode` + its `asm!` + the
`enter_user_mode_checked` SPSR gate); `#[unsafe(no_mangle)]` dropped from
`return_to_kernel` (nothing links the C symbol); and five raw-pointer sites
became safe `akuma-mmu` helpers — `write_phys` for `prepare_for_execution`'s
`ProcessInfo` write, new `read_current_user_val` for `read_current_pid`'s
fallback, new `with_phys_bytes_mut` for the prefault file-fill, the safe
`map_user_page_tracked` for the prefault common path, and new
`write_current_user_val` (+ `is_current_user_range_writable`) for the two CLONE
`set_tid` EL1 stores. **37 → 25 `unsafe` sites.** What remains is **three
groups, and the crate cannot `forbid` until all three clear** (group 1 landed
2026-09-02 — `akuma-slot-table` extracted, **25 → 7** sites):

1. ~~**`table.rs` (18 + the `with_process_exclusive` `unsafe fn`)**~~ — §6.D,
   the generic `akuma-slot-table` primitive. **Done 2026-09-02.** `table.rs`
   holds one `unsafe` block now (`with_process_exclusive`'s body, forwarding to
   `SlotTable::active_exclusive`) and the `unsafe fn` signature; `children.rs`'s
   `lookup_process_shared` deref is gone (safe `table::active_process_ref` →
   `SlotTable::active_ref`). Both survive only because Group 2 (below) has not
   removed `with_process_exclusive`'s callers yet.
2. **The execve / first-run *exclusive* window (3)** — Phase 7f
   (`BKL_PHASE7_AUDIT.md` §5), converting the destructive lifecycle window to
   real interior mutability. Two shapes:
   - **2a — first-run (`run_registered_process`, `entry_point_trampoline`).**
     **Done 2026-09-02.** The only `&mut` need was `Process::run` /
     `prepare_for_execution` setting `self.state` (+ `reset_io` clearing
     `exited`/`exit_code`). Those three fields became atomics —
     `state: AtomicProcessState` (an `AtomicU64` packing `Zombie(i32)`, in
     `akuma-exec-core`), `exited: AtomicBool`, `exit_code: AtomicI32` — the same
     call `brk` made in §5a, ~50 mechanical call-site edits across 5 crates.
     `run`/`prepare_for_execution`/`reset_io` now take `&self`; both sites reach
     their process through `table::active_process_ref` (safe). **7 → 5** sites.
   - **2b — the execve destructive window (`with_own_process_exclusive` →
     `replace_image`).** **Done 2026-09-02. `akuma-exec` 5 → 2 sites** — only
     Group 3 remains. `replace_image` / `replace_image_from_path` take `&self`;
     the `do_execve` closure
     (`akuma-syscalls-glue/src/proc.rs`) resolves its process through
     `lookup_process_shared` and `with_own_process_exclusive` /
     `with_process_exclusive` / `table::get_process_ptr` all go away. Following
     the `ProcAddressSpace` precedent (scalars → lock-free atomic, aggregates →
     one lock) rather than the single coarse `Spinlock<ProcessImage>` the plan
     sketched — `§9.2` rejected a coarse `PROCESS_TABLE_LOCK` and the same
     reasoning applies field-by-field here:
       - **scalars → atomics** (the §5a call): `Process::{entry_point,
         initial_brk, process_info_phys}` → `AtomicUsize`; `clear_child_tid` →
         `AtomicU64`; `sigaltstack_{sp,size}` → `AtomicU64`, `sigaltstack_flags`
         → `AtomicI32`. Signal delivery reads `sigaltstack_*` and must stay
         lock-free.
       - **`ProcessMemory` scalars → atomics** (`code_end`, `brk`,
         `stack_bottom`, `stack_top`, `mmap_limit` → `AtomicUsize`, joining
         `next_mmap`), plus a `reset(&self, …)` so `replace_image` never
         replaces the struct. Contradicts §5-bis's "never assigned again" —
         which was true field-by-field but not across the wholesale
         `self.memory = ProcessMemory::new(…)` execve does.
       - **aggregates → `Spinlock<ProcessImage>`**: `{ name: String, args:
         Vec<String>, context: UserContext }` — the three `execve` swaps
         wholesale. `context` readers (3 of them) copy the `UserContext` out
         under the lock (`Copy`) so the guard never crosses
         `enter_user_mode_checked`'s `!`. `name`'s hot-path readers become
         `proc.image.lock().name` (guard temp lives to statement end); the cold
         `/proc/<pid>/*` renderers take a clone via new `Process::image_name()`
         / `image_args()`. `dynamic_page_tables` turned out to be **dead** —
         nothing pushes to it — so `replace_image`'s `.clear()` just went away
         and the field stayed a bare `Vec` (drained on `Drop`, which keeps
         `&mut self`).
     Removed: `with_process_exclusive` (+ its `table.rs` forwarding block),
     `table::get_process_ptr` (callerless), `SlotTable::active_exclusive`
     (callerless — `akuma-slot-table` 11 → 9 sites). `with_own_process_exclusive`
     kept but now **safe** (`&Process` via `lookup_process_shared`, BKL check
     retained as the peer-exclusion guard until Phase 7f lands). `entry_point`
     and the `Process` `sigaltstack_*` copies have no non-test reader — atomic
     only because `replace_image` writes them.
     Gates: boot self-tests (`signal_reset_on_exec`, `execve_no_heap_leak`,
     `replace_image_preserves_pid`, `execve_kills_thread_group_siblings`,
     `execve_preserves_cwd`, `failed_exec_preserves_cloexec_fds`, the exec-reset
     `clear_child_tid` test — all PASS); `/proc/self/status` name rendering over
     SSH; SMP=4 forktest.
3. **Two decoupled-pointer sites (2)** — `fork`'s `demote_range_to_ro` (its
   `parent_l0` and the serializing `parent_as` lock are *deliberately
   independent* parameters — the self-test passes a lock stand-in whose own L0
   is empty, so this cannot become a `&mut UserAddressSpace` method without a
   phys-validated L0 wrapper), and the vfork-child-prefault `map_user_page` edge
   (edit under the leader's `as_lock`, record frames in the child's distinct
   address space — `map_user_page_tracked` cannot express "lock A, record in B").

Two incidental findings — a triplicated enum (§5b) and the fact that nobody has
swept for either pattern tree-wide (§5c) — came out of the same reading.

## 1. Why this crate is not another `akuma-entry`

The obvious question after `akuma-kernel-glue` took `#![forbid(unsafe_code)]` is
whether `akuma-exec` can do the same. It is by far the biggest prize left:
9,311 lines at 95.2% safe.

It cannot, and the reason is the shape of the distribution rather than the count.
`akuma-kernel-glue`'s `unsafe` was in **2 self-contained files** that were a
hardware surface (boot assembly, the secondary trampoline) — lift those out and
the remaining 2,416 lines forbid. `akuma-exec`'s is woven through its two central
files:

| file | sites | prod lines |
|---|---:|---:|
| `threading/mod.rs` | 49 | 2,633 |
| `process/mod.rs` | 33 | 1,819 |
| `process/table.rs` | 19 | 538 |
| `process/user_access.rs` | 16 | 395 |
| `process/children.rs` | 2 | 797 |
| `threading/sigframe.rs` | 2 | 188 |
| `process/spawn.rs` | 1 | 282 |
| `process/image.rs` | 1 | 237 |

Extracting the four heavy files would leave ~3,900 lines able to forbid, at the
cost of cutting the crate's core in half — the process table and the threading
core on one side, `children`/`spawn`/`fd`/`signal`/`channel`/`stats`/`exec` on the
other. That is a real option, but it is a design change, not a lift-and-shift,
and it should not be attempted on the strength of a line count.

## 2. The eight kinds

Counts are line-level and heuristic (135 `unsafe`-bearing lines against cloc's 123
*sites*, since one site can span lines); the proportions are what matter.

| kind | ~n | what it is |
|---|---:|---|
| raw deref of a per-slot table entry | 36 | `*mut Process` from the process table; `get_context(tid)` into the thread-context array |
| raw read/write at a computed address | 27 | trap-frame fields by byte offset (`sp + 240`, `sp + 248`), the robust-futex list walk in user memory, stack canaries |
| user↔kernel copy / raw slice | 14 | `user_access.rs` and its `__arch_copy_user_memory` fault trampoline |
| asm / extern / `no_mangle` / `unsafe impl` | 12 | `msr tpidr_el1`, the `thread_start` trampolines, `unsafe impl Sync for SyncContext` |
| context switch / enter EL0 / map user page | 10 | |
| **stored-callback `transmute`** | 6 | **fixed in this pass — §4** |
| **`&self` → `&mut` field cast** | 6 | **§5, the real finding** |
| RawWaker vtable | 4 | |

The first five are the crate's subject matter. A crate that owns the process
table, the thread contexts and the EL0 boundary is going to dereference raw
pointers and read trap frames; that is not debt, and the audit's conclusion on
those 99 sites is "correctly placed, leave them".

## 3. What is genuinely reducible

Three of the eight kinds are not subject matter:

- **stored-callback `transmute` (6)** — fixed, §4.
- **`&self` → `&mut` field cast (6)** — unsound, §5.
- **RawWaker vtable (4)** — `akuma-exec` has `alloc`, so `alloc::task::Wake`
  would express the thread waker safely without a hand-written vtable. Not
  attempted here; the `RawWakerVTable` is small and its four functions are
  correct. Worth doing when someone is already in that file.

## 4. Fixed: three callbacks stored as `AtomicUsize` + `transmute`

`threading/mod.rs` held three kernel-registered hooks as an `AtomicUsize`
containing `cb as usize`, transmuted back to `fn(usize)` at each read:

```rust
static SLOT_PURGE_CALLBACK: AtomicUsize = AtomicUsize::new(0);
pub fn set_slot_purge_callback(cb: fn(usize)) {
    SLOT_PURGE_CALLBACK.store(cb as usize, Ordering::SeqCst);
}
// …at the read site:
let purge: fn(usize) = unsafe { core::mem::transmute(purge_addr) };
```

Why it was written that way is dating, not design:

| | date |
|---|---|
| `CLEANUP_CALLBACK` written | **2026-03-19** |
| `OnceCopy` first exists, in `akuma_exec::runtime` | 2026-05-28 |
| `SLOT_PURGE_CALLBACK` written | 2026-08-04 |
| `Registered` (adds the diagnostic) | 2026-08-30 |

The first predates the alternative by five months. The later two copied a local
pattern already obsolete **inside their own crate** — and the irony is that
`OnceCopy` was *born* in `akuma_exec::runtime`, made `pub` there so `akuma-ext2`
could reuse it, then extracted twice. The crate that invented the mechanism used
it in `runtime.rs` and nowhere else in its own 9,300 lines. It was extracted to
help others and never swept at home.

The conversion is behaviour-preserving:

- **The type was never erased.** The setter takes `fn(usize)` and the transmute
  produced `fn(usize)`; the `usize` was only an internal representation. And
  `transmute` is strictly more dangerous than the API needs — it will convert any
  address to any signature, with `!= 0` as the sole guard, and an `AtomicUsize`
  lets future code store a non-function value there at all.
- **`OnceCopy`, not `Registered`.** The read sites check for absence and skip, and
  the recycler genuinely runs before the kernel registers anything, so absence is
  a legitimate state — the rule in `docs/reference/subsystems/kernel-hooks.md`.
- **Single-shot is safe here.** `.store` is last-writer-wins and `OnceCopy::set`
  is first-writer-wins, so this is the one real semantic change. Each hook has
  exactly one registration site, all from a boot-time `init`, and no test callers:
  `akuma-vfs-glue/src/fs.rs:72`, `akuma-kernel-glue/src/lib.rs:1073`,
  `akuma-exec/src/process/mod.rs:420`.

123 → **119** sites, 452 → 448 unsafe lines. Verified by boot as well as by
build, since the recycler is a live path.

**One `transmute` deliberately remains**, at `spawn_user_thread_initializing`:

```rust
core::mem::transmute::<extern "C" fn() -> !, fn(*mut ()) -> !>(trampoline_fn)
```

That one *does* change the signature — it is a different and more dangerous
operation than the callback round-trip, and it is not addressable by swapping the
container. Left alone, flagged here.

## 5. Not fixed: six `&self` → `&mut` field casts

`process/mod.rs` mutates fields through a shared reference:

```rust
pub fn vm_alloc_mmap(&self, size: usize) -> Option<usize> {
    with_irqs_disabled(|| {
        let _g = self.vm_lock.lock();
        // SAFETY: `vm_lock` (held here) serializes every caller, so this is the
        // unique live reference to `memory` for the closure's duration
        let memory = unsafe { &mut *(core::ptr::addr_of!(self.memory) as *mut ProcessMemory) };
        memory.alloc_mmap(size)
    })
}
```

Six sites, all `&self` methods, and they are not obscure corners — they are the
VM accessors `CLAUDE.md` instructs everyone to use:

| line | method | field |
|---|---|---|
| 901 | `vm_with_regions` | `mmap_regions` |
| 928 | `vm_alloc_mmap` | `memory` |
| 940 | `vm_free_mmap` | `memory` |
| 1008 | `with_address_space` | `address_space` |
| 1017 | `with_address_space` | `address_space` |
| 1177 | `set_brk` | `brk` |

**What the SAFETY comments get right:** mutual exclusion. `vm_lock` and `as_lock`
genuinely serialize writers; there is no data race, and the reasoning about
`CLONE_VM` siblings is sound.

**What they do not address:** aliasing. `memory`, `address_space`, `mmap_regions`
and `brk` are plain fields — no `UnsafeCell` anywhere in `Process`'s declaration
of them — so a `&mut` derived from `&self` is UB under Stacked/Tree Borrows
regardless of how well-locked it is. Miri would flag the write. A lock provides
exclusion; it does not provide provenance.

**Calibration — why this is not urgent.** The obvious escalation is "rustc emits
`noalias readonly` for `&T`, so this is actively miscompilable". That is *not*
true for this type: those attributes require `T: Freeze`, and `Process` contains
`Spinlock<LazyRegionMap>` and `Arc<Spinlock<StdioBuffer>>`, so `Process` is
`!Freeze` and `&Process` gets neither attribute. The field-level UB is real and
the whole-type accident that defuses it is not something to rely on, but it does
mean this is a correctness-hygiene item and not a fire. Consistent with the
crate's observed stability.

**The fix, and its cost.** Wrap the mutated fields in `UnsafeCell` — which keeps
the runtime behaviour identical and makes the existing `unsafe` sound rather than
merely locked. The cost is that the fields are `pub`:

| field | refs inside `akuma-exec` | refs outside |
|---|---:|---:|
| `address_space` | 54 | 148 |
| `brk` | 28 | 4 |
| `memory` | 13 | 12 |
| `mmap_regions` | 9 | 8 |

`brk` is the cheap one and does not need `UnsafeCell` at all: it is a plain
`usize` mutated through `&self`, which is the exact shape of an `AtomicUsize` —
that removes the site rather than legalising it. `address_space` at 202
references is a mechanical but wide change and should be its own commit, most
likely behind a private field plus an accessor rather than a bare type swap.

## 5-bis. Correction: these can be made **safe**, not merely sound (2026-09-01)

§5 and §6 below said the fix "legalises the `unsafe` rather than removing it",
and proposed `UnsafeCell` plus accessors. **That was wrong**, and it was wrong by
anchoring on the first mechanism that came to mind instead of auditing the
fields. Corrected here; §6's order is updated to match.

### The anti-pattern is a lock that guards nothing

```rust
pub vm_lock: Spinlock<()>,          // a lock guarding nothing
pub mmap_regions: Vec<MmapRegion>,  // ...and the data it "guards", beside it
```

There is no way to obtain `&mut` from a `Spinlock<()>`, so the code reaches
around it with `&mut *(addr_of!(self.field) as *mut T)`. Putting the data
**inside** the lock — `Spinlock<Vec<MmapRegion>>` — hands out `&mut` safely and
makes the lock/data pairing compiler-checked instead of comment-checked. Zero
`unsafe`, not sound-but-unsafe.

`Process` already does this correctly **one field over**:
`lazy_regions: Spinlock<LazyRegionMap>`. The idiom was in the same struct the
whole time.

### What the audit of the fields actually found

`ProcessMemory` has seven fields, and only **one** of them is a mutable
aggregate:

| field | mutated after `new()`? |
|---|---|
| `code_end`, `brk`, `stack_bottom`, `stack_top`, `mmap_limit` | **no** — set at construction, never assigned again |
| `next_mmap` | yes, and it is **already an `AtomicUsize`** (CAS'd for `CLONE_VM` siblings) |
| `free_regions: Vec<(usize, usize)>` | **yes — the only one** |

So the whole of `vm_alloc_mmap`/`vm_free_mmap`'s `unsafe` exists to reach one
`Vec`. `free_regions: Spinlock<Vec<(usize, usize)>>` removes it outright and lets
both methods keep their `&self`.

### Splitting `vm_lock` is safe, and here is the evidence

`vm_lock` nominally guards `mmap_regions` *and* `ProcessMemory::free_regions`.
There are exactly **four** `vm_lock.lock()` sites in the tree:

| site | touches |
|---|---|
| `mod.rs:904` `vm_with_regions` | `mmap_regions` only |
| `mod.rs:934` `vm_alloc_mmap` | `free_regions` only |
| `mod.rs:949` `vm_free_mmap` | `free_regions` only |
| `mod.rs:1189` `set_brk` | nothing, since §5a — the store is atomic |

**No site holds it across both fields.** Two callers that want an allocate-then-
record sequence already take and drop the lock twice, so a competing thread can
already interleave between them; per-field locks remove no exclusion that exists
today.

### `address_space` needs a different shape, and it is not "just wrap it"

Naively wrapping `UserAddressSpace` in a `Spinlock` makes every reader acquire
it, and most readers are deliberately lock-free scalar getters on the fault path:

| accessor | n | kind |
|---|---:|---|
| `l0_phys()` (`self.l0_frame.addr`) | 42 | scalar |
| `track_user_frame` | 24 | mutation |
| `map_page` | 14 | mutation |
| `track_page_table_frame` | 12 | mutation |
| `ttbr` | 11 | scalar |
| `is_shared()` | 6 | scalar |

~59 of them are scalars. And `as_lock` already has documented deadlock
discipline — `mod.rs:193` describes chunking holds to avoid "this core holding
`as_lock` while a nested IRQ hard-spins for the BKL, against a peer holding the
BKL and waiting on `as_lock` in `munmap`". Making every reader acquire it widens
that surface considerably.

The shape that works is a **split**, not a wrapper: the scalars
(`l0_phys`, `asid`, `is_shared`, `ttbr`) become atomics on `Process`, read
lock-free as they are today; the mutating operations move inside
`Spinlock<UserAddressSpace>`, reached through `with_address_space`, which is
already exactly that accessor — so those call sites do not change shape at all.

**That is a change to the locking model, not a wrapper swap**, and it needs its
own plan: `l0_phys` changing under a lock-free reader is precisely the
page-table-UAF class (`PAGE_TABLE_UAF_TTBR_GATE_FIX`). Do `memory`/
`mmap_regions` first as a proof of the pattern.

## 5a. Landed: `Process::brk` -> `AtomicUsize` (2026-09-01)

The first of the six, and the only one that could be *removed* rather than
legalised — the other five guard aggregates (`ProcessMemory`, `UserAddressSpace`,
`Vec<MmapRegion>`) that have no atomic equivalent.

`set_brk(&self)` wrote through `&mut *(addr_of!(self.brk) as *mut usize)` under
`vm_lock`. The lock was real and is kept; what it could not provide was
provenance. The give-away was already in the function's own doc comment:

> *"Concurrent **readers** of `brk` race the store exactly as they did against
> the old `&mut self` write."*

That is a description of an atomic, written out longhand. `AtomicUsize` with
`Ordering::Relaxed` expresses precisely the semantics that were already
intended, and the compiler agrees with it. The `vm_lock` acquire stays: it no
longer protects this store (an atomic needs none) but still orders it against
the other `vm_*` bookkeeping under the same lock, and removing it would be a
change to the locking discipline rather than to soundness.

Cost: 12 sites in `akuma-exec`, plus two readers in `akuma-exceptions` and one
boot-suite constructor. **6 -> 5 UB sites.**

The disambiguation is the trap: there are **three** `brk` fields — `Process::brk`
(`process/mod.rs:590`), `ProcessMemory::brk` (`process/types.rs:590`, the same
line number by coincidence) and `LoadedElf::brk` — so `.brk` is not sed-able.

## 5b. Landed: `FrameSource` was defined three times (2026-09-01)

Found while answering "why does `pmm.rs` exist". The same five-variant enum was
declared in **three** crates —

| | |
|---|---|
| `akuma-pmm/src/lib.rs:343` | the real one; owns the tracker |
| `akuma-exec/src/runtime.rs:24` | byte-identical copy |
| `akuma-mmu/src/lib.rs:110` | byte-identical copy |

— with **three** byte-identical five-arm converters between them
(`runtime.rs`, `akuma-mmu`, and `akuma-exec/src/pmm.rs`), each translating a
copy back into the original before every `akuma_pmm::track_frame` call.

Both copies justified themselves in comments, and both justifications were
backwards:

- `runtime.rs`: *"this crate's enum and `akuma_pmm::FrameSource` are separate
  types (the crate sits below `akuma-exec` and cannot name this one)."* True and
  irrelevant — `akuma-pmm` cannot name *ours*, but `akuma-exec` has always
  depended on `akuma-pmm` (`Cargo.toml:82`) and could always have named *theirs*.
- `akuma-mmu`: *"the `akuma-exec` enum, mirrored here so this crate can attribute
  frames without depending on the execution crate."* It was mirroring a copy;
  the original sits **below** both, in a crate `akuma-mmu` already depends on.

Now one definition and zero converters; the two copies are `pub use
akuma_pmm::FrameSource`. Nothing was gained by the duplication except three
places to forget a variant.

`akuma-exec/src/pmm.rs` also carried its module-doc block **twice** (two `//!`
headers with different bodies), and asserted that `akuma-pmm` "works in raw
`usize` physical addresses **on purpose** (see its module doc)" — that doc argues
leaf-ness, never usize-ness. Both corrected; the second matters because it is
exactly what a future "should `akuma-pmm` speak `PhysFrame`?" decision turns on.

## 5c. Deferred: sweep for both patterns tree-wide

**Neither of the above was looked for — both were tripped over.** That is the
finding. `akuma-exec` was audited because it was the biggest crate; the
`&self` -> `&mut` cast and the duplicated-enum-plus-converter both turned up
inside it, and nothing has checked whether they exist elsewhere.

Two sweeps are owed, and neither has been run:

1. **`&self` -> `&mut` casts.** Grep shape: `addr_of!(self.` and
   `as *mut` on a place derived from `&self`. In `akuma-exec` every instance
   carried a SAFETY comment arguing *mutual exclusion*, which is the right
   argument for the wrong question — so the comment is not a filter. The check
   that matters is whether the field sits in an `UnsafeCell`.
2. **Duplicated types with hand-written converters.** Grep shape: the same
   variant list declared in more than one crate, plus a `match` whose arms are
   `X::A => Y::A`. An identity `match` between two enums is the tell, and it is
   cheap to search for.

Both are mechanical to look for and neither is urgent — the system is stable and
these are hygiene. But "we found two of these by accident in one crate" is not
evidence that there are only two.

## 5c-bis. Landed: `free_regions` and `mmap_regions` moved inside their locks (2026-09-01)

The §5-bis design, applied. **5 -> 2 UB sites**, and the three that went are
*gone*, not legalised — there is no `unsafe` in the replacement.

| before | after |
|---|---|
| `pub vm_lock: Spinlock<()>` + `pub mmap_regions: Vec<MmapRegion>` | `pub mmap_regions: Spinlock<Vec<MmapRegion>>` |
| `pub free_regions: Vec<(usize, usize)>` | `pub free_regions: Spinlock<Vec<(usize, usize)>>` |
| `vm_with_regions`: `&mut *(addr_of!(self.mmap_regions) as *mut _)` | `let mut regions = self.mmap_regions.lock(); f(&mut regions)` |
| `vm_alloc_mmap`/`vm_free_mmap`: cast to `&mut ProcessMemory` | `self.memory.alloc_mmap(size)` — the method takes `&self` and locks the free list itself |

`ProcessMemory::alloc_mmap` and `free_mmap` changed receiver from `&mut self` to
`&self`, which is what let the two `Process` wrappers drop their casts. That is
sound because of the field audit in §5-bis: the five `usize` fields are immutable
after `new()` and `next_mmap` is already atomic, so `&self` + a locked
`free_regions` covers everything either method touches.

### `vm_lock` is deleted

It guarded nothing once both `Vec`s held their own locks — and `set_brk`, its
last holder, had already stopped needing it in §5a. A `Spinlock<()>` beside the
data it nominally protects is the anti-pattern that created all six casts, so
leaving a vestigial one behind would have re-planted it. `set_brk` is now a bare
relaxed atomic store with no lock and no IRQ masking.

Doc references to `vm_lock` in `akuma-mmap`, `akuma-syscalls-mem`,
`akuma-exec-core` and `children.rs` describe the *discipline* (short holds, no
yield/alloc inside) which still stands; they name a field that no longer exists
and are worth a pass.

### One hold, not two, in `alloc_mmap`

The free-list scan reads an index it then mutates, so the lock is taken once
across the whole scan-and-splice rather than per-operation — releasing between
them would let a peer invalidate `i`. It is dropped before the `next_mmap` CAS
loop, which needs no lock.

### What the readers cost

Every previously lock-free `.mmap_regions` read now takes the lock — `len()`,
`iter()`, `clone()` on the fork path, plus four boot-suite sites. That is the
right trade here and not the one §5-bis warns about for `address_space`: these
are fork/exec-path reads, not the per-fault `l0_phys()` reads, and they were
racing the writers before.

## 5c-ter. Landed: `address_space` + `as_lock` merged into `ProcAddressSpace` (2026-09-02)

Full writeup: [`AKUMA_EXEC_ADDRESS_SPACE_MERGE.md`](AKUMA_EXEC_ADDRESS_SPACE_MERGE.md).

The §5-bis "split, don't wrap" design for the last two casts, applied. **2 -> 0
UB sites** — the crate's `&self -> &mut` cast count is now **zero**, and the two
that went are *gone*, not legalised.

`Process` carried the address space as two fields — `address_space:
UserAddressSpace` and `as_lock: Spinlock<()>` — and `with_address_space` bridged
them with `&mut *(addr_of!(self.address_space) as *mut UserAddressSpace)`. They
are now one field:

```rust
pub struct ProcAddressSpace {
    inner: Spinlock<UserAddressSpace>,   // was `as_lock` + the data it guarded
    ttbr0: AtomicU64,                    // lock-free mirror: (asid << 48) | l0_phys
    shared: AtomicBool,
}
```

### The field audit that made the wrapper unnecessary to fear

§5-bis worried that `Spinlock<UserAddressSpace>` would put a lock acquire on
~59 fault-path scalar reads. It doesn't, because **`UserAddressSpace`'s
`l0_frame`/`asid`/`shared` are immutable after `new()`/`new_shared()`** (grep:
no `self.l0_frame =` anywhere) and `page_table_frames`/`user_frames` are already
`Spinlock`-wrapped. So the four scalar getters (`l0_phys` 42, `ttbr0` 11,
`is_shared` 7, `asid` 3) read an atomic mirror on `Process`; **every other
`UserAddressSpace` operation already ran under `as_lock`** (the fault path in
`src/exceptions.rs` takes it before every `track_*`) or held an exclusive
`&mut Process` (fork child, ELF loader). The mirror is refreshed only by
`execve` — `ProcAddressSpace::replace` swaps the inner value and stores the new
`ttbr0`/`shared` in one step.

### `with_address_space` is now zero `unsafe`

```rust
pub fn with_address_space<R>(&self, f: impl FnOnce(&mut UserAddressSpace) -> R) -> R {
    let mut g = self.address_space.lock();   // as_lock, IRQs masked on smp-shared
    f(&mut g)
}
```

Both cfg arms collapse into this one — the `not(kernel_smp_shared)` arm's "single
core / BKL-serialized" cast is gone; on that config the spinlock is simply always
uncontended (the BKL serialises every caller first).

### `AsLockHold` deleted; `CowRemap::CallerHoldsAsLock` fixed

`AsLockHold::new(&owner.as_lock)` at ~10 `src/exceptions.rs` sites became
`owner.address_space.lock()`, using the returned guard for the `track_*`
sequence that used to reach the field separately. `CowRemap::CallerHoldsAsLock`
carried `&'a Process` and its arm called `owner.address_space.track_user_frame`
— which, once `track_*` is behind the lock, would have **self-deadlocked**
against the caller's own hold. It now carries `&'a mut UserAddressSpace` (the
caller's guard) directly.

### The `&self` frame-tracking passthroughs are deliberately absent

`ProcAddressSpace` has one-shot passthroughs for the read-only `&self` methods
(`translate`, `resident_pages`, `user_frame_count`, …) but **not** for
`track_user_frame`/`track_page_table_frame`/`adopt_user_frame`/`remove_user_frame`
— those are called from fault/CoW paths that already hold `lock()` across a
`map_user_page` + track sequence, and a self-locking passthrough there is the
deadlock above. Those sites take `lock()` and go through the guard.

## 6. Recommended order

1. ~~The three callbacks~~ — done, §4.
2. ~~`brk` → `AtomicUsize`~~ — done, §5a.
3. ~~`memory` and `mmap_regions` → inside their locks~~ — done, §5c-bis (not
   `UnsafeCell`: the data moved *inside* `Spinlock`, zero `unsafe`).
4. ~~`address_space` + `as_lock` → `ProcAddressSpace`~~ — done, §5c-ter. The
   scalars became an atomic mirror on `Process` and the rest moved inside
   `Spinlock<UserAddressSpace>`; again zero `unsafe`, not `UnsafeCell`.
   **All six `&self -> &mut` casts are now gone.**
5. RawWaker → `alloc::task::Wake`, opportunistically. Still open — and weaker
   than it looks: `Wake` needs `Arc<Self>`, so it trades 6 trivially-correct
   `unsafe` lines for a heap allocation on every `current_thread_waker()`. The
   thread waker packs its handle into the data pointer precisely to avoid that.

6. Splitting the crate for `forbid`. This is not one step but four, and the
   first is nearly free:

   - **A. Extract `process/user_access.rs` → `akuma-user-access`.** ~~open~~
     **done 2026-09-02** ([`AKUMA_EXEC_USER_ACCESS_EXTRACTION.md`](AKUMA_EXEC_USER_ACCESS_EXTRACTION.md)):
     the EL0 copy boundary — asm loop, fault trampoline, `copy_from/to_user` —
     is its own crate, one stated contract, cannot forbid. `akuma-exec` **126 →
     112** `unsafe` blocks. The demand-paging half (`prefault_user_range`) needs
     a `Process` so it stayed as `process::lazy_prefault`, reached through a
     `set_prefault_hook` registration. Prereq done on the way: the per-thread
     user-copy-fault-handler slot moved to `akuma_primitives::preempt` (both the
     setter and `akuma-exceptions`' reader now need neither `akuma-exec`).
   - **B. Push `process/mod.rs`'s 29 down.** **Done 2026-09-02, 29 → 12:**
     fork's 4 phys-page copies → `akuma_mmu::copy_phys_page`; the two
     `ProcessInfo` frame writes (here + `image.rs`) → `akuma_mmu::write_phys`
     (`ProcessInfo` gained `#[derive(Copy)]`); the robust-futex list walk + the
     `clear_child_tid` exit write (~9 raw `core::ptr::read/write` on user VAs) →
     `akuma_user_access::{read_user_into_with, write_user_val_with}` with
     `Prefault::No`; the `(*ptr).tgid` deref in `kill_thread_group` →
     `table::with_process`; and `execute_boxed` + `check_process_exit` — both
     dead since an older launch design, **zero references tree-wide** — deleted
     (−3).

     The 12 that remain are irreducible here: the EL0-entry surface
     (`enter_user_mode` ×2 + its `asm!`, `entry_point_trampoline`,
     `enter_user_mode_checked`, `Process::run` — ~8) does **not** move to
     `akuma-entry` — that crate depends on `akuma-exec` (a cycle) and its stated
     contract is "nothing here dereferences a caller-supplied pointer", which
     `enter_user_mode(&UserContext)` and the trampoline's `&mut *proc_ptr` both
     do. It is `Process`'s own subject matter. Plus `demote_range_to_ro`'s raw
     `*mut u64` L0, `table::with_process_exclusive`, and the two CLONE `set_tid`
     writes that are **deliberately** a plain EL1 store (`copy_to_user_safe` was
     tried and broke the Go runtime — the comment at the site).
   - **C. Extract `threading/mod.rs` + `sigframe.rs` → `akuma-threading`.**
     **Done 2026-09-02.** ~5,800 lines (5,200 prod + 6 test modules), **57
     `unsafe` blocks** — the thread pool, the lock-free per-slot state arrays,
     the context switch, park/wake, per-thread stacks + overflow detection, the
     `thread_start`/SGI trampolines. `akuma-exec` **94 → 37** `unsafe` blocks.

     It does not name `Process`; `UserContext`/`UserTrapFrame`/`MAX_THREADS` were
     already in `akuma-exec-core`. Its upward surface is three registered hook
     structs built in `akuma_exec::init`: `ThreadRuntime` (6 platform fns — a
     subset of `ExecRuntime`), `ThreadConfig` (15 tuning knobs — a subset of
     `ExecConfig`), `ProcessHooks` (7 fns: the trace gate, two tid→pid lookups,
     the reclaim drain flag, the interrupt check, and two diagnostics —
     `dump_orphan_processes` + a `proc_dump_info` tuple for `[THR-DUMP]`). Deps:
     `akuma-{primitives,exec-core,cpu,mmu,pmm,bkl,mmap}` — **none reach back to
     `akuma-exec`**. Prereq on the way: the per-thread user-copy-fault-handler
     slot had already gone to `akuma_primitives::preempt` in step A.
     `akuma-exec` re-exports `pub use akuma_threading as threading`, so all 33
     downstream `akuma_exec::threading::…` consumers resolve unchanged.
   - **D. `process/table.rs` (19)** — the `*mut Process` slot store.
     **Done 2026-09-02.** Genericized into `akuma-slot-table`: a
     `SlotTable<T, N>` owning the FREE/ACTIVE/RETIRED state array, the
     `[AtomicPtr<T>; N]`, the per-slot reuse generation, the retire timestamp,
     and every deref — the move `akuma-locks-rw-cell`/`akuma-gic` made, never
     naming `Process`. `table.rs` keeps all domain logic (the identity cache,
     `THREAD_PID_MAP`, the reclaim hooks, `mark_thread_terminated`) and calls
     the generic crate for each deref. The orderings and the RETIRED-window
     generation bump are preserved verbatim. **`akuma-exec` 25 → 7** `unsafe`
     sites; `akuma-slot-table` carries 11 behind one stated contract.

     On the way, the identity cache lost its `own_ptr`/`tgid_ptr`
     `AtomicPtr<Process>` fields: `identity_get` now derives the pointer with
     `SlotTable::ref_if_current(slot, stamped_generation)`, which within one
     generation is immutable and equal to what the cached pointer held (the
     comment at the old deref site already argued this). The `*_slot` stores
     became the `Release` publication point that `*_ptr` used to be.
     `collect_process_info`'s two `MaybeUninit` array sites went too —
     `[T::default(); N]` with the existing `T: Copy` bound. Gates: full boot
     self-test suite green (`identity_lazy_restamp`, `identity_recycled_slot_rejected`,
     `epilogue_identity_revalidated`, `fork-bkl-drop`,
     `pmm_conserved_across_spawn_exit_reap`, `retired_reclaim_*`, `slot_recycling`);
     `akuma-slot-table` host tests (racing claimers/reclaimers, generation
     tracking, cooldown).

   **Correction (§6.E, 2026-09-02):** A–D do **not** leave the crate
   `unsafe`-free. The execve / first-run exclusive window (`with_process_exclusive`
   and its three callers) survives every extraction — it is a Phase 7f problem,
   not a layering one — and two decoupled-pointer sites (`demote_range_to_ro`,
   the vfork prefault edge) need their own phys-validated wrappers. `forbid`
   needs D **and** those. Do **not** do C on the strength of a line count; A
   stands on its own regardless.

## Background

- [`AKUMA_ENTRY_EXTRACTION.md`](AKUMA_ENTRY_EXTRACTION.md) — the split that made `akuma-kernel-glue` forbid, and why this crate is not the same shape.
- [`AKUMA_KERNEL_HOOKS.md`](AKUMA_KERNEL_HOOKS.md) — the hook audit; §4 here is its `akuma-exec` deviation, now closed.
- [`../reference/subsystems/kernel-hooks.md`](../reference/subsystems/kernel-hooks.md) — `Registered` vs `OnceCopy`.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — which crates forbid `unsafe` and why the rest cannot.
