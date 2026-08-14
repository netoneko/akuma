# `unsafe` audit — `src/` and `crates/`

**Date:** 2026-08-12
**Scope:** the kernel bin crate (`src/`) and the seven extracted crates (`crates/`).
Userspace (`userspace/`) is out of scope.
**Question:** where can `unsafe` be replaced with a safe alternative, what should be
done first, and how much `unsafe` is there.

Nothing in this document has been applied. It is a work list.

---

## 1. Method

Counts are of source **lines containing the `unsafe` keyword**, excluding comment
lines. Each line is classified by the first pattern that matches it; a bare
`unsafe {` opener is classified by the next three lines. So a "site" is one
`unsafe` keyword, not one unsafe *operation* — clippy's
`multiple_unsafe_ops_per_block` (§3.3) shows the operation count is far higher.

The hygiene numbers in §3.3 are not estimates: they come from

```bash
cargo clippy --release -p akuma -- \
  --force-warn clippy::undocumented_unsafe_blocks \
  --force-warn clippy::multiple_unsafe_ops_per_block
```

which overrides the in-crate `#![allow(...)]` attributes.

---

## 2. Headline stats

| | files | LOC | `unsafe` sites | per kLOC |
|---|---:|---:|---:|---:|
| `src/` | 55 | 61,190 | 604 | 9.9 |
| `crates/` | 64 | 35,330 | 275 | 7.8 |
| **Total** | **119** | **96,520** | **879** | **9.1** |

By construct:

| Construct | src/ | crates/ | Total |
|---|---:|---:|---:|
| `unsafe { … }` block | 575 | 244 | 819 |
| `unsafe fn` | 12 | 22 | 34 |
| `unsafe impl` | 6 | 4 | 10 |
| `unsafe extern "C"` | 6 | 5 | 11 |
| `static mut` (real, not in comments) | 0 | 3 | 3 |
| `unsafe trait` | 0 | 0 | 0 |

Only **three** `static mut` in the whole tree, and no `unsafe trait` — the global
state discipline is good. The volume is concentrated in pointer work.

### 2.1 By category

| Category | src/ | crates/ | Total | % |
|---|---:|---:|---:|---:|
| `ptr-deref` — raw deref, `.add()`, `ptr::read/write`, `copy_nonoverlapping` | 114 | 78 | 192 | 21.8% |
| `user-copy` — `copy_{to,from}_user_safe` | 164 | 3 | 167 | 19.0% |
| `opaque-block` — multi-line block, no single dominant op | 56 | 48 | 104 | 11.8% |
| `volatile-ptr` — `read_volatile`/`write_volatile` (PTEs **and** MMIO) | 64 | 40 | 104 | 11.8% |
| `asm-sysreg` — `mrs`/`msr` | 64 | 15 | 79 | 9.0% |
| `asm-barrier-cache` — `dsb`/`isb`/`dc`/`ic`/`tlbi` | 37 | 12 | 49 | 5.6% |
| `unsafe-fn-decl` | 14 | 20 | 34 | 3.9% |
| `asm-halt-wait` — `wfi`/`wfe`/`sev`/`nop` | 27 | 4 | 31 | 3.5% |
| `raw-slice` — `from_raw_parts{,_mut}` | 10 | 19 | 29 | 3.3% |
| `asm-other` | 19 | 6 | 25 | 2.8% |
| `alloc-raw` — `alloc_zeroed`/`dealloc`/`Box::{into,from}_raw` | 8 | 5 | 13 | 1.5% |
| `unsafe-extern` | 6 | 5 | 11 | 1.3% |
| `attr-no_mangle` — `#[unsafe(no_mangle)]` etc. | 7 | 4 | 11 | 1.3% |
| `unsafe-impl` | 6 | 4 | 10 | 1.1% |
| `unchecked-api` — `*_unchecked` | 7 | 3 | 10 | 1.1% |
| `transmute` | 1 | 5 | 6 | 0.7% |
| `static-mut` | 0 | 4 | 4 | 0.5% |

Inline assembly across all four `asm-*` rows is **184 sites (21%)**. That is the
floor: none of it can become safe, only better encapsulated (§6.1).

### 2.2 Hot files

| `src/` | | `crates/` | |
|---|---:|---|---:|
| `src/exceptions.rs` | 141 | `akuma-exec/src/threading/mod.rs` | 63 |
| `src/tests.rs` | 86 | `akuma-exec/src/mmu/mod.rs` | 56 |
| `src/syscall/net.rs` | 36 | `akuma-exec/src/process/mod.rs` | 38 |
| `src/syscall/fs.rs` | 36 | `akuma-ext2/src/ext2.rs` | 23 |
| `src/rng.rs` | 32 | `akuma-net/src/smoltcp_net.rs` | 23 |
| `src/syscall/proc.rs` | 27 | `akuma-exec/src/elf/mod.rs` | 17 |
| `src/allocator.rs` | 18 | `akuma-exec/src/process/table.rs` | 15 |
| `src/process_tests.rs` | 18 | `akuma-net/src/hal.rs` | 9 |
| `src/smp_shared.rs` | 16 | `akuma-exec/src/sync.rs` | 8 |
| `src/syscall/term.rs` | 15 | `akuma-exec/src/mmu/user_access.rs` | 5 |

**116 of the 604 `src/` sites (19%) are in the boot self-test files**
(`tests.rs` 86, `process_tests.rs` 18, `sync_tests.rs` 8, `async_tests.rs` 2,
`daif_tests.rs` 1). Test code deliberately pokes at raw kernel state; it is a
different risk class and is not prioritised below except where the fix is free.

---

## 3. Hygiene

### 3.1 SAFETY comment coverage

| | count |
|---|---:|
| `unsafe` blocks | 819 |
| blocks **missing** a safety comment | 729 |
| **documented** | **~11%** |
| `unsafe impl` missing a safety comment | 6 of 10 |

Worst offenders: `src/exceptions.rs` 129, `src/tests.rs` 86,
`akuma-exec/src/mmu/mod.rs` 50, `akuma-exec/src/threading/mod.rs` 41,
`src/syscall/fs.rs` 36, `src/syscall/net.rs` 33.

### 3.2 Lints currently switched off

- `clippy::missing_safety_doc` — allowed in `crates/akuma-exec/src/lib.rs:46`,
  `crates/akuma-exec/src/runtime.rs:1`, `crates/akuma-net/src/runtime.rs:1`.
  `akuma-exec` owns 22 of the 34 `unsafe fn` in the tree, so this is where the
  contracts are least documented.
- `unused_unsafe` — allowed in `crates/akuma-exec/src/lib.rs`. **Verified to fire
  zero warnings today** (`--force-warn unused_unsafe`, clean tree), so removing
  the allow is free and stops the class from silently reappearing.
- `clippy::undocumented_unsafe_blocks` and
  `clippy::multiple_unsafe_ops_per_block` are clippy *restriction* lints, not in
  `all`/`pedantic`/`nursery`, so they have never run against this tree.
- `unsafe_op_in_unsafe_fn`: both crates are edition 2024, and the code already
  writes the inner `unsafe {}` explicitly. No gap here.

### 3.3 Block granularity

**145 blocks contain more than one unsafe operation.** The four largest:

| Site | ops |
|---|---:|
| `src/exceptions.rs:1644` — RT signal frame write | 131 |
| `src/exceptions.rs:1415` — signal frame write | 120 |
| `src/syscall/fs.rs:1999` — `statx` buffer marshal | 30 |
| `src/allocator.rs:804` — talc realloc | 23 |

By file: `mmu/mod.rs` 35, `exceptions.rs` 28, `tests.rs` 19, `rng.rs` 7.

The three top blocks are all the *same* anti-pattern (§4.2) and are the highest
value target in the tree.

---

## 4. Findings, by priority

### P0 — the user-copy wrapper: 167 sites, 19% of all `unsafe`

> **Status: DONE 2026-08-14.** The check and the copy are one helper now
> (`copy_to_user`, `copy_from_user`, `write_user_val`, `read_user_into`, plus
> `_with(Prefault)` forms and `as_user_bytes{,_mut}` for arrays), and
> `validate_user_ptr` + `ensure_user_pages_mapped` moved out of the bin crate into
> `akuma_exec::mmu::user_access` so the copy can fold them in. **`src/syscall/`
> went from 192 `unsafe` to 24**, `rump_proxy.rs` 12 → 0, `exceptions.rs`
> 107 → 97, `akuma-exec/src/process/mod.rs` 36 → 34. Host tests 516 → 521.
>
> **Two things this audit got wrong, one of them in the safe direction and one
> not** — read them before trusting the rest of this section:
>
> 1. **"Folding the range check in makes it unskippable" is true. "That closes the
>    hole" is not.** `add_kernel_mappings` (`mmu/mod.rs:710`, comment at `:105`)
>    identity-maps kernel RAM as **EL1-only 2 MB blocks in every user address
>    space**, and `is_current_user_range_mapped` tests *presence*, not
>    EL0-accessibility — so a mapped kernel VA still passes validation, and the
>    byte loop runs at EL1 where the EL1-only permission does not stop it. What
>    keeps this from being reachable is only that the user VA allocator avoids
>    `[KERNEL_VA_START, kernel_va_end())` — a layout convention, not a check. The
>    real fix is to test the leaf PTE's AP bits ("mapped *as user memory*"), which
>    is a contained change to `is_current_user_range_mapped` but a **behaviour
>    change** and wants its own A/B. **Done 2026-08-14**; see §4.0a below.
> 2. **The "it can't move, the prefault is bin-crate logic" objection was wrong.**
>    Raised while planning, and disproved by looking: `akuma-exec` already depends
>    on `akuma-pmm` directly, `as_lock`/`with_as_locked` are its own `Process`
>    methods, and `ExecRuntime` already carries the exact two hooks the file fill
>    needs (`read_at`, `read_at_by_inode`, `runtime.rs:114`/`:116`). The only
>    genuinely bin-crate thing was `crate::pmm::alloc_page_zeroed`, a one-line
>    wrapper over `akuma_pmm::alloc_page_zeroed`. 127 lines moved with no new hook
>    and no new dependency edge.
>
> Everything else the section predicted held: the API is safe `fn`s, the length
> invariant became unstateable-wrong, and the `(&raw const v).cast::<u8>()` +
> separate-size pairing is gone. **Full record:**
> [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md).

**Where:** `crates/akuma-exec/src/mmu/user_access.rs` defines

```rust
pub unsafe fn copy_from_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64>
pub unsafe fn copy_to_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64>
```

167 call sites, spread over `syscall/net.rs` (38), `syscall/fs.rs` (31),
`syscall/proc.rs` (23), `rump_proxy.rs` (15), `syscall/term.rs` (14),
`syscall/poll.rs` (14), `exceptions.rs` (13), and 14 more files.

**Why it can be safe.** The function already has full fault handling: it installs
a per-thread recovery handler and the byte-copy loop returns `EFAULT` on an EL1
data abort. Nothing about the *user* side needs the caller's `unsafe`. What the
raw-pointer signature does need from the caller is that `len` agrees with the
kernel-side buffer — and that is exactly what a slice encodes.

**Proposed API** (safe `fn`, no `unsafe` at any call site):

```rust
pub fn copy_to_user(dst_user: usize, src: &[u8])            -> Result<(), u64>;
pub fn copy_from_user(dst: &mut [u8], src_user: usize)      -> Result<(), u64>;
pub fn write_user_val<T: Copy>(dst_user: usize, val: &T)    -> Result<(), u64>;
pub fn read_user_val<T: Copy>(src_user: usize)              -> Result<T, u64>;
```

**Second win — this closes a real hole.** `copy_{to,from}_user_safe` performs **no
range check of its own**. The range check lives in a *separate*, opt-in helper,
`validate_user_ptr` (`src/syscall/mod.rs:486`), which has 126 call sites against
167 copies. Its own comment records that it deliberately does not exclude the
kernel physical VA range, and relies on "the EL1 data-abort recovery path" as the
safety net. That net only catches **unmapped** addresses. A call site that skips
`validate_user_ptr` and passes a *mapped kernel* VA as `dst` will scribble kernel
memory silently, with no fault and no diagnostic. Folding the range check into
the wrapper makes the check unskippable and makes the safe signature honest.

**Third win.** The `write_user_val`/`read_user_val` variants kill ~40 occurrences
of the `(&raw const st).cast::<u8>()` + `core::mem::size_of::<T>()` pairing, where
a mismatch between the two is a stack over-read.

**Effort:** large but mechanical — the great majority of the 167 sites are the
single-line form `if unsafe { copy_to_user_safe(p as *mut u8, buf.as_ptr(), n).is_err() }`
and convert by rote. **Risk:** low. **Payoff:** −167 `unsafe` (19% of the tree) plus
a soundness fix.

### 4.0 What the P0 conversion found — full record elsewhere

The conversion is written up in [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md), because it
is a history document and this is an audit. In one paragraph: ~140 of the 167 sites
converted by rote, ~25 needed a decision, and those fall into three groups —
copies that must not demand-page (`Prefault::No`: inside a spinlock with IRQs
masked, or inside an exception handler), `validate_user_ptr` calls that had to stay
(they gate a caller-sized allocation, keep the prefault off a lock, or preserve
error precedence), and two callers that stay raw on purpose
(`copy_from_user_byte`, whose string walk has no range to validate, and the test
whose subject is the fault trampoline). One real bug fell out — `mremap` never
validated its *destination*, so a lazy page in the new mapping silently truncated
the move — and three Linux divergences plus one pre-existing lock-ordering defect
were recorded without being changed.

### 4.0a The check was unskippable but wrong — FIXED 2026-08-14

> **Status: DONE.** `is_current_user_range_mapped` tests the leaf PTE's AP bits
> (bit 6 — set for `AP_RW_ALL`/`AP_RO_ALL`, clear for the two EL1-only encodings),
> so a kernel VA as a syscall buffer now returns `EFAULT`. Boot test
> `kernel_va_rejected_as_user_pointer`. Two things the plan below did not foresee:
> the *page*-granular `is_current_user_page_mapped` had to stay a presence test
> (its callers are demand-paging paths where a `PROT_NONE` page must read as
> present), and `validate_user_range` had to re-check after `prefault_user_range`,
> which skips already-present pages and would otherwise wave the kernel VA through
> on every `Prefault::Yes` path. Record: [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §7.

`user_range_ok` + `is_current_user_range_mapped` could not distinguish a user page
from a kernel page: `add_kernel_mappings` (`mmu/mod.rs:710`) identity-maps kernel
RAM as EL1-only 2 MB blocks in **every** user address space, the mapped-ness test
checks presence only, and the copy loop runs at EL1 where the EL1-only permission
does not stop it. What keeps it unreachable is a layout convention (the user VA
allocator avoids `[KERNEL_VA_START, kernel_va_end())`), not a check.

The fix is an AP-bit test at the leaf — "mapped *as user memory*" — which
`is_page_mapped_ptr` already walks to. It is a behaviour change and wants its own
A/B plus a boot test that a kernel VA as a syscall destination returns `EFAULT`.
Note before touching it that the kernel VA range is deliberately **not** excluded
today, because Bun's JSC `mmap`s at `0x5000_0000`; an AP-bit test handles that
correctly where a range exclusion would not. Rationale and the rest:
[`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §7.

### P1 — `#[repr(C)]` structs instead of hand-offset byte writes: 3 blocks, 281 unsafe ops

**Where:** `src/syscall/fs.rs:1999` (`statx`, 30 ops),
`src/exceptions.rs:1415` and `:1644` (signal frame, 120 + 131 ops).

All three build a binary ABI structure by writing typed values at hand-computed
byte offsets into a raw buffer:

```rust
core::ptr::write(p.add(28).cast::<u16>(),  mode);
core::ptr::write(p.add(32).cast::<u64>(),  ino);
core::ptr::write(p.add(40).cast::<u64>(),  size);
// … 12 more
```

**Why it can be safe.** `src/syscall/fs.rs` already does this correctly elsewhere
in the same file — `Stat` (line 238) and `Statfs` (line 1163) are `#[repr(C)]`
structs written with normal field assignment and one copy. There is no reason
`Statx` and the signal frame cannot be. The layout constants
(`SIGFRAME_SIGINFO`, `SIGFRAME_UCONTEXT`, `SIGFRAME_MCONTEXT`,
`SIGFRAME_FPSIMD`) become `offset_of!` assertions instead of load-bearing
arithmetic, so the ABI stays pinned and checked at compile time.

**Extra win on the signal path.** Those two blocks write to a **user VA directly
from EL1**, which is why the path needs the `ensure_cow_page_writable` /
`ensure_user_page_mapped` pre-flight immediately above them and leans on the EL1
data-abort recovery. Building a 1120-byte `#[repr(C)] SigFrame` on the kernel
stack and issuing one `copy_to_user` (P0) moves the whole thing onto the audited
fault path and deletes the pre-flight dance. 1120 bytes is comfortable even
against the trimmed 96 KB system stack.

**Effort:** medium — pure layout transcription, but ABI-critical.
**Risk:** medium on the signal path (regression surface is signal delivery; guard
it with the boot self-tests that already consume `TEST_SIGFRAME_*`).
**Payoff:** −3 blocks / −281 unsafe ops, and it removes an entire class of
offset-arithmetic bug.

### P2 — the three `static mut`

**`crates/akuma-ext2/src/ext2.rs:341,344`**

```rust
static mut IS_THREAD_DEAD_FN:   Option<fn(usize) -> bool> = None;
static mut CURRENT_THREAD_ID_FN: Option<fn() -> usize>    = None;
pub unsafe fn init_thread_hooks(…)   // + 2 unsafe read sites
```

This is not just ceremony — a hook read racing the initialiser is a data race,
and the `# Safety` note ("must only be called once during kernel initialization")
is unenforced. `akuma-ext2` already depends on `spinning_top`, so
`static HOOKS: Spinlock<Option<ThreadHooks>>` (or an `AtomicUsize`-guarded
once-cell if the read is too hot for a lock) is a drop-in.
**−4 `unsafe`, −1 `unsafe fn`, plus a race fix.** Low effort, low risk.

**`crates/akuma-net/src/smoltcp_net.rs:224`**

```rust
static mut SOCKET_STORAGE: [SocketStorage<'static>; MAX_SOCKETS] = …;
// one use, line 572:
let mut sockets = unsafe { SocketSet::new(&mut SOCKET_STORAGE[..]) };
```

The static exists only to hand `SocketSet` a `'static` buffer. `akuma-net` builds
smoltcp **with the `alloc` feature on**, so `SocketSet::new(Vec::with_capacity(MAX_SOCKETS))`
gives the same lifetime with no static and no `unsafe`. The `sockets` value is
immediately moved into the `NETWORK` lock, so nothing else observes the change.
**−1 `unsafe`, −1 `static mut`.** Trivial.

### P2 — the virtio HAL layer (detailed)

Four findings, of which the first two are the substantial ones.

#### (a) The `NetHal` runtime indirection is pure overhead — both translators are the identity function

`crates/akuma-exec/src/mmu/mod.rs:171-178`:

```rust
#[inline(always)] pub fn phys_to_virt(paddr: usize) -> *mut u8 { paddr as *mut u8 }
#[inline(always)] pub fn virt_to_phys(vaddr: usize) -> usize   { vaddr }
```

`src/virtio_hal.rs` calls these directly. `crates/akuma-net/src/hal.rs` instead
routes through `(runtime().virt_to_phys)(…)`, and `runtime()`
(`crates/akuma-net/src/runtime.rs`) is:

```rust
pub fn runtime() -> NetRuntime {
    RUNTIME.lock().expect("akuma-net: NetRuntime not registered — …")
}
```

So every translation takes a **`Spinlock`**, **copies the whole 10-field / 80-byte
`NetRuntime`** (it is `Copy`, so `.expect()` on the deref copies it), and passes
through a **panic path** — to compute `|x| x`.

This is not a cold path. virtio-drivers calls `Hal::share` from
`Descriptor::set_buf` (`queue.rs:722`) and `Hal::unshare` on pop
(`queue.rs:437`, `:454`, `:482`) — **once per buffer per queue operation**, i.e.
per packet on both TX and RX.

**Fix:** have `NetHal` call `akuma_exec::mmu::virt_to_phys`/`phys_to_virt`
directly. `akuma-net` already depends on `akuma-exec` unconditionally (its
`Cargo.toml` comment says "Always pulled in" — it re-exports
`akuma_exec::sync::PreemptGuard` from there), and `cargo build -p akuma-net
--target aarch64-apple-darwin` was verified to still work, so host-testability is
unaffected. `src/rng.rs:325` already calls `akuma_exec::mmu::virt_to_phys`
directly from a driver, so this is the established in-tree pattern; `NetHal` is
the outlier.

Four consequences, in increasing order of importance:

1. A spinlock acquisition and an 80-byte struct copy leave the packet path.
2. It removes a lock from the DMA path that no lock-ordering document mentions.
   `RUNTIME` is write-once and held for nanoseconds so the risk is low, but it is
   *not* covered by `PreemptGuard` the way `NETWORK`/`SOCKET_TABLE` are, and the
   BKL-free net work (`no-bkl-network`) is predicated on knowing exactly which
   locks the packet path takes.
3. `virt_to_phys` and `phys_to_virt` can then be deleted from `NetRuntime`
   (10 fields → 8), shrinking every one of the other 23 `runtime()` call sites
   in the crate.
4. **`NetHal` and `VirtioHal` become byte-identical**, which makes (b) trivial.

#### (b) The two `Hal` impls are line-for-line duplicates

`src/virtio_hal.rs` (9 unsafe) and `crates/akuma-net/src/hal.rs` (9 unsafe)
differ *only* in how they reach the translators. The net crate's own doc comment
already says "identical logic to the kernel's `VirtioHal`". Keep one; delete the
other. **−9 `unsafe`** and, more importantly, no drift between two DMA
address-translation implementations.

**With (a) applied this is free.** An earlier draft of this audit flagged a boot-order
prerequisite — `NetHal` needs `runtime()`, installed by `akuma_net::init()` at
`src/main.rs:1226`, but `VirtioHal`'s consumers `block::init()`
(`src/main.rs:969`) and `audio.rs` come up earlier. Once (a) removes the
`runtime()` lookup there is no initialisation to order against, and the
prerequisite disappears. `crates/akuma-net/src/lib.rs:7` declares `pub mod hal;`
ungated, so the shared HAL is available in every feature set including
`extreme-size`.

#### (c) DMA allocation failure panics the kernel

Both impls do:

```rust
let virt = unsafe { alloc_zeroed(layout) };
assert!(!virt.is_null(), "DMA allocation failed");
```

`Hal::dma_alloc` returns `(PhysAddr, NonNull<u8>)` with no error channel, so the
failure genuinely cannot be propagated through the trait. But note the
inconsistency: `src/rng.rs:315-318` and `:378-381` handle the *same* allocation
failing gracefully (`return Err(RngError::TransportError)` plus a `dealloc` of
what it already took), and repo policy elsewhere is "kernel OOM kills the
process, it does not panic".

Virtqueues are allocated once at device init, so the clean fix is a boot-time DMA
reservation that makes the assert unreachable rather than merely unlikely. Minimum
fix: replace the bare `assert!` with the kernel's diagnostic abort path so the
failure prints something useful.

#### (d) Smaller items

- Neither `unsafe impl Hal` carries a SAFETY comment — 2 of the 6 counted in §3.1.
- `dma_alloc` ignores `BufferDirection` in both impls. Correct given identity
  mapping and a coherent QEMU virt, but undocumented.
- `unshare` is an empty no-op in both. The kernel one explains why ("No-op for
  identity mapping (no cache management needed on QEMU)"); the net one has no
  comment at all. This is the single place where the identity-mapping and
  DMA-coherency assumption is load-bearing, and consolidating to one impl means
  it gets documented exactly once — the main *correctness* argument for (b),
  beyond the unsafe count.
- `Layout::from_size_align(pages * 4096, 4096).unwrap()` is infallible by
  construction. Fine as is.

#### (e) The hand-rolled virtqueue in `src/rng.rs`

> **STATUS 2026-08-13: both defects below are FIXED; this section is kept as the
> analysis, not as open work.** The driver also moved — it is
> `crates/akuma-virtio/src/rng.rs` now, not `src/rng.rs`.
>
> - **The length clamp** is `completion_copy_len(used_elem.len, to_read)`, which
>   clamps to what the descriptor offered rather than to the caller's remaining
>   space. There is also a guard the text below does not anticipate: a device
>   completing with `len == 0` is an error, because otherwise `bytes_read` never
>   advances and the outer loop reissues the same request forever.
> - **The acquire barrier** is there: `VirtqAvail`/`VirtqUsed`'s
>   `idx`/`flags`/`*_event` are `AtomicU16` and the poll loop does
>   `idx.load(Ordering::Acquire)` — exactly the "fix, matching the library"
>   proposed at the end of this section.
>
> Both were unverified by any test until 2026-08-13, when `completion_copy_len`
> was split out as a pure function and pinned by
> `completion_length_is_clamped_to_what_was_offered`. Until then
> `akuma-virtio` had **zero** tests across ~1,470 lines — the only crate in the
> workspace with none. See `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §6.2.

There is exactly **one** hand-rolled virtqueue in the tree — `src/rng.rs`
(`VirtqDesc`/`VirtqAvail`/`VirtqUsedElem`/`VirtqUsed` at lines 116-149, plus its
own MMIO register block and transport handshake). Nothing else defines a queue
type; `block.rs`, `audio.rs`, `smoltcp_net.rs` and `rump_tap.rs` all go through
virtio-drivers.

**It has to stay hand-rolled.** virtio-drivers 0.7.5 has no RNG device —
`device/` contains only `blk`, `console`, `gpu`, `input`, `net`, `socket`,
`sound`. So this is not a deduplication target.

But it is missing two guarantees the library provides, and both are real.

**Device-controlled length is not clamped to the staging buffer.** The buffer is
a fixed 256-byte allocation (`Layout::from_size_align(256, 64)`, line 377), and
each request asks for `to_read = min(256, remaining)`. The completion path then
copies:

```rust
let copy_len = core::cmp::min(used_elem.len as usize, buf.len() - bytes_read);
core::ptr::copy_nonoverlapping(self.buffer, buf.as_mut_ptr().add(bytes_read), copy_len);
```

`used_elem.len` is written by the **device** into the used ring. The clamp is
against the caller's remaining space, not against the 256-byte source or against
`to_read`. A device reporting `len > to_read` causes a heap over-read of up to
`buf.len() - bytes_read` bytes out of a 256-byte allocation — and `getrandom`
hands the result straight to userspace. The code already refuses to trust
`used_elem.id` (line 464) on exactly this reasoning; `len` was missed. QEMU will
not trigger it, but the fix is one word: clamp to `to_read`.

virtio-drivers does not have this bug because it has no staging buffer — device
writes land directly in the caller's slice via `share()`, and `pop_used`'s
returned length is informational.

**No acquire barrier between observing the completion and reading the data.**
The poll loop is:

```rust
loop {
    fence(Ordering::SeqCst);
    let used_idx = unsafe { read_volatile(&raw const (*self.used).idx) };
    if used_idx != self.last_used_idx { break; }
    …
}
let used_elem = unsafe { (*self.used).ring[used_ring_idx] };   // plain read
```

The fence sits at the *top* of the loop body — before the `idx` read. After the
`break` there is no barrier before reading `used.ring[…]` or before
`copy_nonoverlapping` from the DMA buffer. Observing the new `idx` is what makes
the data valid, so the ordering needed is **acquire on the `idx` load**, and
`read_volatile` does not provide it: volatile constrains the compiler, not the
CPU's load/load reordering. Harmless under QEMU TCG's serialised device model;
a genuine window under a concurrent one.

The general shape of the problem is that the rings are DMA-shared but accessed
inconsistently — MMIO registers are volatile throughout, `used.idx` is volatile,
while the descriptor fields (413-417), `avail.ring[]` and `avail.idx` (427, 429)
and `used.ring[]` (460) are all plain accesses whose ordering rests entirely on
the scattered `fence(SeqCst)` calls.

**Fix, matching the library:** virtio-drivers models exactly the cross-visible
fields as atomics — `AvailRing { flags: AtomicU16, idx: AtomicU16, ring: [u16;
N], used_event: AtomicU16 }` and the same for `UsedRing` (`queue.rs`), with
`ring[]` left plain because the acquire/release on `idx` covers it. Making
`VirtqAvail`/`VirtqUsed`'s `idx`/`flags`/`*_event` `AtomicU16` and using
`Release` on publish / `Acquire` on the completion load gives both missing
guarantees and lets most of the `fence(SeqCst)` scatter go away.

**Smaller notes on the same file:**

- `assert!(version == 2)` (line 219) panics the kernel from inside a probe loop,
  two lines after the magic-value check returns `Err(TransportError)`. The
  comment says the panic is deliberate ("fail loud and early"), but the
  inconsistency with its neighbour is worth a note.
- The completion wait is a 10,000,000-iteration `spin_loop()` with no yield
  (443-456), holding `RNG_DEVICE`. `getrandom` is on the BKL opt-out list so it
  is not holding the BKL, but it does pin the core for the duration.
- `QUEUE_SIZE` is 2 and only descriptor 0 is ever used — the path is depth-1
  synchronous. The ring-index modulo and wrapping arithmetic is machinery for a
  queue that never has more than one request in flight, and `calc_queue_layout`
  page-aligns the used ring, so ~8 KB is allocated to move 256 bytes.
- `unsafe impl Send`/`Sync for VirtioRngDevice` (175-176) **is** genuinely needed
  here — the struct holds raw pointers. Contrast with `block.rs`/`audio.rs`,
  where the identical-looking impls are redundant (next finding).

#### (f) The device discovery scaffolding *is* duplicated — 4 ways

The queue is unique, but everything around it is copy-pasted:

| Duplicate | Copies |
|---|---|
| `VIRTIO_MMIO_ADDRS: [usize; 8]` (`DEV_VIRTIO_VA` + `0x000`…`0xe00`) | **4** — `rng.rs:26`, `block.rs:23`, `audio.rs:87`, and inline at `main.rs:1216` |
| Probe loop: `read_volatile((addr + 0x008) as *const u32)` → device-id compare → `NonNull::new(addr as *mut VirtIOHeader)` → `MmioTransport::new` | **4** — `rng.rs:512`, `block.rs:259`, `audio.rs:212`, `smoltcp_net.rs:536` |

The four slot tables are character-for-character identical. The probe loops have
drifted in the way copy-paste always does: `rng.rs` uses the named constant
`VIRTIO_MMIO_DEVICE_ID`, `block.rs` and `audio.rs` hardcode `addr + 0x008`, and
`smoltcp_net.rs` hardcodes both the offset *and* the device id (`if device_id !=
1`). `main.rs` builds its own copy of the table to pass into
`akuma_net::init(&mmio_addrs, …)`, so the net crate takes it as a parameter while
the three in-kernel drivers each define their own.

One `virtio::probe(device_id) -> Option<(usize, MmioTransport)>` helper next to
the shared HAL collapses all four, removes ~4 `unsafe`, and gives the slot table
a single home. This is the cleanup worth doing when (b) lands — along with the
`dma_alloc_pages` helper `rng.rs` could share for its queue allocation.

### P2 — `UnsafeCell` in the block and sound drivers is unnecessary (10 sites)

`src/block.rs` and `src/audio.rs` both wrap their device in `UnsafeCell`, add an
`unsafe impl Sync`, and expose an `inner_mut(&self) -> &mut T`:

```rust
pub struct VirtioBlockDevice {
    inner: UnsafeCell<VirtIOBlk<VirtioHal, MmioTransport>>,
    …
}
unsafe impl Sync for VirtioBlockDevice {}          // block.rs:84
fn inner_mut(&self) -> &mut VirtIOBlk<…> {
    unsafe { &mut *self.inner.get() }              // block.rs:113
}
```

The stated justification — "`VirtIOBlk` needs `&mut self` … but we want to share
it through a `Spinlock`" — does not hold. Both devices live **inside** the
spinlock:

```rust
static BLOCK_DEVICE: Spinlock<Option<VirtioBlockDevice>> = Spinlock::new(None);
static SOUND_DEVICE: Spinlock<Option<VirtioSoundDevice>> = Spinlock::new(None);
```

so `.lock()` already yields a guard that derefs to `&mut Option<Device>`. The
code just never asks for it: every accessor binds `let guard = X.lock()`
(immutable) and then uses `guard.as_ref()` with `&self` methods, and the
`UnsafeCell` exists solely to claw the mutability back. Switching to
`guard.as_mut()` and `&mut self` methods deletes the whole apparatus.

**`src/rng.rs:556` already does it the safe way** — `let mut guard = RNG_DEVICE.lock();`
with no `UnsafeCell` anywhere in the file. Three sibling drivers, one of which
got it right.

The `unsafe impl Sync` is likewise redundant: it is needed *only* because
`UnsafeCell<T>: !Sync`. virtio-drivers already provides
`unsafe impl Send + Sync` for `VirtQueue` (`queue.rs:546`, `:550`) and
`MmioTransport` (`mmio.rs:314`, `:318`), and every remaining field of
`VirtIOBlk`/`VirtIOSound` is a plain scalar, `Vec`, `Box` or `BTreeMap` — so both
device types are auto-`Send`+`Sync`. Drop the cell and the impl is simply not
needed.

| File | Sites removed | Of total |
|---|---|---|
| `src/block.rs` | `unsafe impl Sync` (84), `inner_mut` (113) | 2 of 4 |
| `src/audio.rs` | 135, 141, 149, 157, 160, 174, 194, 198 | 8 of 10 |

`src/audio.rs` also wraps `params: UnsafeCell<Params>` and
`prepared: UnsafeCell<bool>` — same fix, they become plain fields. The two
surviving `unsafe` in `audio.rs` (212 MMIO probe, 225 `MmioTransport::new`) are
genuine.

**−10 `unsafe`.** Low effort, low risk — mechanical `as_ref()` → `as_mut()` at 1
call site in `block.rs` and 5 in `audio.rs`, plus `&self` → `&mut self` on the
methods.

### P2 — cheap `*_unchecked` removals (10 sites)

| Site | Fix |
|---|---|
| `src/virtio_hal.rs:29`, `:50`; `akuma-net/src/hal.rs:26`, `:46` — `NonNull::new_unchecked` | `NonNull::new(p).expect("DMA alloc")`. Two of the four sit one line below an `assert!(!virt.is_null())` that already did the check. Only 2 survive once the HAL dedup lands. |
| `src/tests.rs:2617`, `:2693`, `:9209`; `src/async_tests.rs:15` — `Pin::new_unchecked(&mut fut)` | `core::pin::pin!(fut)`. Exactly what the macro is for. |
| `src/exceptions.rs:2657`, `akuma-isolation/src/subdir_fs.rs:47` — `str::from_utf8_unchecked` | `from_utf8().unwrap_or("<invalid utf8>")`. Both are diagnostic formatters; a panic-path formatter must not have UB as its failure mode. |
| `src/allocator.rs:778`, `:785`, `:827`, `:833`, `:922`, `:928` — `Layout::from_size_align_unchecked`, `NonNull::new_unchecked` | **Keep** — `GlobalAlloc` hot path. Add `debug_assert!(align.is_power_of_two())`; free in release. |

**−8 `unsafe`,** near-zero effort and risk.

### P2 — the `SocketHandle` transmute

`crates/akuma-net/src/smoltcp_net.rs:1017` transmutes `SocketHandle → usize` to
read a private field, because smoltcp exposes no accessor. But the owning struct
**already caches the index** in the adjacent `handle_index` field (line 1001,
documented "Must always be < `MAX_SOCKETS`"). Assign it from the allocating side
at socket creation and the transmute — plus its `const _` size assertion — goes
away. **−1 transmute.** Low effort.

### P3 — fn-pointer transmutes in `threading/mod.rs` (3 sites)

Lines 1135, 1744, 1754: `SLOT_PURGE_CALLBACK` / `CLEANUP_CALLBACK` are
`AtomicUsize` holding an `fn(usize)`, read back with `transmute`. Well-behaved as
written, but any registration with a mismatched signature is UB with no compiler
check. A `Spinlock<Option<fn(usize)>>` with a typed setter fixes that; the
in-code comments confirm neither call site holds a module lock, so taking one is
allowed. **−3 transmute.** Modest value; the risk of touching the slot-recycle
path is what puts it at P3.

---

## 5. Structural wins (bigger, higher risk)

### 5.1 A `Pte` / `PageTable` newtype — the biggest lever in `crates/`

`crates/akuma-exec/src/mmu/mod.rs` has **91 volatile pointer operations** — by far
the densest file in the tree — nearly all of the shape
`l3_ptr.add(idx).write_volatile(entry)` / `read_volatile(l0)`. A
`PageTable(*mut u64)` wrapper with bounds-checked `entry(idx) -> Pte` /
`set_entry(idx, Pte)` and a `Pte(u64)` newtype carrying the flag accessors would
concentrate all of it into ~4 `unsafe` methods and give ~50 call sites a safe,
index-checked API. That is a large real reduction *and* it would make the flag
manipulation type-checked rather than raw bit arithmetic.

**Do not do this casually.** This file is the subject of the page-table UAF /
TTBR-gate work and the CoW/ASID-flush fixes; it is the highest-consequence file
in the repo. Gate any attempt behind the full boot self-test suite at SMP=1/2/4
plus acceptance 05/10/11/13.

### 5.2 An `Mmio<T>` register newtype

Real device MMIO (as distinct from page-table volatile access) is
`src/rng.rs` 31, `src/fw_cfg.rs` 5, `src/gic.rs` 4, `src/console.rs` 3,
`src/gic_v3.rs` 2, `src/pmm.rs` 2, `src/block.rs` 1, `src/audio.rs` 1 — ~49 sites,
almost all `read_volatile((base + OFFSET) as *const u32)`.

A `Mmio<u32>` register type with `read()`/`write()` collapses those to ~6 `unsafe`
sites at construction. `src/rng.rs` alone accounts for 31 and is a self-contained
driver — good pilot.

**Caveat:** `src/gic_v3.rs:67` documents a deliberate decision *not* to use
`read_volatile`/`write_volatile` (the optimiser lowers a volatile loop to a
post-indexed store). Respect that; exclude GICv3 from the sweep.

### 5.3 Thin safe wrappers for read-only system registers

Of the 79 `mrs`/`msr` sites, the read-only ones — `CNTVCT_EL0`, `CNTFRQ_EL0`,
`MPIDR_EL1`, `ESR_EL1`, `FAR_EL1`, `DCZID_EL0`, `TPIDR_EL0`, and DAIF reads — are
side-effect-free and can be exposed as safe `fn`s from a single `arch` module
(`pub fn mpidr() -> u64` etc.). Roughly 25–30 sites move from "unsafe at every
use" to "unsafe once". Writes (`msr TTBR0_EL1`, `SPSR_EL1`, `ELR_EL1`, DAIF set/clear)
must stay unsafe — they change execution context.

This is **encapsulation, not a safety gain**: the total number of unsafe
*operations* is unchanged. Worth doing for readability and to shrink the audit
surface; do not count it as risk reduction.

---

## 6. The irreducible core — do not chase these

Roughly **430 sites (49%)** cannot become safe under any refactor, and effort
spent there is wasted:

- **Inline assembly, 184 sites.** Context switch, exception entry/exit, cache and
  TLB maintenance, `wfi`/`wfe`, barriers. Encapsulable (§5.3), never safe.
- **Exception frame access** (`(*frame).x0`, `(*frame).elr_el1`, …) in
  `src/exceptions.rs`. The frame pointer comes from the vector asm; this *is* the
  trust boundary.
- **`GlobalAlloc` / talc** (`src/allocator.rs`, 18 sites). The allocator cannot be
  implemented in safe Rust.
- **`unsafe extern "C"`, 11 sites**, and `#[unsafe(no_mangle)]`, 11 sites. Linker
  symbols and asm-called entry points.
- **ELF loading** (`akuma-exec/src/elf/mod.rs`, 17 sites) — parsing attacker-shaped
  bytes into mappings is inherently unsafe at the boundary, though the *parsing*
  half could move behind a validated-header type.
- **`enter_user_mode`, `get_context_ptrs`, thread trampolines** in
  `akuma-exec/src/{process,threading}`. Genuine `unsafe fn` with real contracts.
- **The 6 `unsafe impl Send`/`Sync`** on device singletons — these are assertions
  about the hardware, not about Rust. They should each carry a SAFETY comment
  (§3.1) but they cannot be removed.

---

## 7. How much is actually cuttable

Sorted by effort rather than by priority — the question "what can we cut cheaply"
has a different answer from "what should we fix first".

| Tier | Work | Δ sites | % of 879 |
|---|---|---:|---:|
| **A** — hours, no design decision | drop `UnsafeCell` in `block.rs`/`audio.rs` (−10), virtio `Hal` dedup (−9), ext2 hooks → `Spinlock` (−5), `Pin::new_unchecked` → `pin!` (−4), `from_utf8_unchecked` (−2), `NonNull::new_unchecked` (−2), `SOCKET_STORAGE` → `Vec` (−1), `SocketHandle` transmute (−1) | **−34** | 3.9% |
| **B** — one API decision, then 167 rote edits | safe slice-based user-copy (§4 P0) | **−167** | 19.0% |
| **A+B — "easy"** | | **−201** | **22.9%** |
| **C** — medium, ABI-critical | `#[repr(C)]` `Statx`/`SigFrame` — small in *sites*, −281 unsafe *ops* | −3 | 0.3% |
| **D** — medium | `Mmio<T>` register type (excl. GICv3) | ~−25 | 2.8% |
| **E** — medium, relocation not removal | safe sysreg readers: ~28 uses collapse to ~1 site in an `arch` module | ~−27 | 3.1% |
| **F** — large, **high risk** | `Pte`/`PageTable` newtype in `mmu/mod.rs` | ~−50 | 5.7% |

**Cheap answer: 201 sites, 23%, at low risk** — and 167 of those are one sweep.
Everything through tier E reaches ~880 → ~625 (−29%). Tier F would reach ~575
(−35%), which is close to the hard floor: the irreducible core in §6 is ~430
sites (49%), so roughly 145 sites sit between "worth arguing about" and
"impossible".

Tier A is worth doing on its own merits regardless of the unsafe count: it also
removes a spinlock and an 80-byte struct copy from the per-packet DMA path
(§4 P2(a)), and closes a real data race in the ext2 thread hooks.

Note the site count understates tier D badly: three blocks, but 281 unsafe
operations and the entire signal-frame ABI. Judge that one by §3.3, not by this
table.

## 7.1 Suggested ordering

| # | Item | Δ`unsafe` | Effort | Risk |
|---|---|---:|---|---|
| 1 | `NetHal` → direct `akuma_exec::mmu` calls; then delete `src/virtio_hal.rs`; then drop `virt_to_phys`/`phys_to_virt` from `NetRuntime` | −9 | small | low |
| 2 | Drop `UnsafeCell` + `unsafe impl Sync` in `block.rs` / `audio.rs` | −10 | small | low |
| 3 | Remaining tier A quick wins (ext2 hooks, `SOCKET_STORAGE`, `*_unchecked`, `SocketHandle`) | −15 | small | low |
| 4 | Drop `unused_unsafe` from the `akuma-exec` allow list (fires 0 today) | 0 | trivial | none |
| 5 | ~~**P0** — safe slice-based user-copy API, all 167 sites~~ **DONE 2026-08-14** — `src/syscall/` 192 → 24 `unsafe`, `rump_proxy.rs` 12 → 0, `exceptions.rs` 107 → 97, `akuma-exec/process/mod.rs` 36 → 34; +5 host tests. ~25 of the sites needed a decision rather than a rewrite (§4.0), the unchecked-destination hole is **not** closed by the fold (§4.0a), and one real bug fell out (`mremap`'s destination was never validated) | **0 left** | — | — |
| 6 | **P1** — `#[repr(C)]` `Statx` + `SigFrame` | −3 blocks / −281 ops | medium | medium |
| 5 | Enable `clippy::undocumented_unsafe_blocks` in the clean crates (`akuma-vfs`, `akuma-terminal`, `akuma-rump`, `akuma-isolation` — 2 hits total), ratchet inward | 0 | small | none |
| 6 | Drop `clippy::missing_safety_doc` allows; document the 34 `unsafe fn` | 0 | medium | none |
| 7 | §5.2 `Mmio<T>`, piloted on `src/rng.rs` | ~−25 | medium | low |
| 8 | §5.3 safe sysreg readers | ~−28 (moved) | medium | low |
| 9 | §5.1 `Pte`/`PageTable` newtype | ~−50 | large | **high** |

Items 1–4 take the tree from **879 → ~690 sites (−22%)** and fix two real defects
(the unchecked user-copy destination, the ext2 hook race) along the way.

---

## 8. Verify

After any batch:

```bash
cargo clippy --release --all-targets                 # pre-commit hook runs this
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo build --release && SMP=4 cargo run --release   # boot self-test suite
scripts/build_extreme_size.sh                        # 4 MB floor still holds
```

Re-measure with the two commands in §1. For P0 specifically, the check that
matters is that `grep -rn 'copy_to_user_safe\|copy_from_user_safe' src crates`
returns only the definitions in `user_access.rs`.

**What that grep returns after the P0 sweep (2026-08-14), and why it is not zero:**
`user_access.rs` itself (the definitions and the safe wrappers' one call each),
`syscall/mod.rs`'s `copy_from_user_byte` (§4.0 group 3), `tests.rs`'s
`el1_user_copy_fault_recovery` test (the trampoline is the unit under test), and
three `process_tests.rs` comments that name the primitive while explaining history.
Every one is deliberate and says so at the site.

**Not covered by a test, and worth saying plainly:** the `mremap`
destination-prefault fix ([`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §5) has no
dedicated regression test. The boot suite
and the Tier 3 fork/CoW binaries pass unchanged, which shows the sweep preserved
behaviour, but neither exercises "move a mapping whose destination pages are still
lazy". A `userspace/` probe that `mremap`s a large region and verifies the moved
bytes would pin it; the truncation was silent, so nothing would fail loudly
without one.

Syscall-surface changes need boot-suite coverage in `src/process_tests.rs`, per
the repo convention.

---

## Background

- `docs/reference/subsystems/memory.md`, `.../smp-shared.md` — the invariants the
  §5.1 page-table work would have to preserve.
- `docs/archive/BKL_PHASE7_AUDIT.md`, `docs/archive/BKL_PHASE7E_ACCESS_HALF.md` — why
  `&'static mut Process` was removed from `akuma-exec/src/process/table.rs`; the
  surviving `with_process_exclusive` is the documented exception.
- `docs/archive/ALLOC_PRINT_AUDIT.md` — the same audit shape applied to console
  allocation; the precedent for a lint ratchet in this repo.
- `docs/reference/subsystems/console.md` § "Printing rules" — the exemption-list
  pattern §7 item 5 proposes reusing for `undocumented_unsafe_blocks`.
