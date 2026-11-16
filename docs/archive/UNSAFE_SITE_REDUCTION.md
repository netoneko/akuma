# Unsafe site reduction: `Pod` user copies, funnelled raw access (2026-11-16)

**Date:** 2026-11-16
**Branch:** `ouch`
**Result:** production `unsafe` sites **367 → 329** (`scripts/cloc_akuma.py`, the count
`scripts/check_unsafe_budget.py` gates on); forbid crates 107 of 146. Budget:
[`unsafe-budget.json`](../reference/unsafe-budget.json); numbers:
[`crate-safety.md`](../reference/crate-safety.md).

The method was the same throughout: find N `unsafe` blocks that each vouch for the **same
fact**, and replace them with one private primitive that checks or states that fact once.
That primitive keeps one `unsafe` block, and the N callers become safe code. A count that
went down without a check moving in, or a contract being written down, would not be worth
having, so each row below says what carries the claim now.

## 1. `Pod`: what may cross the user boundary as bytes

**Before.** `read_user_into`, `write_user_val`, `as_user_bytes{,_mut}` (`akuma-user-access`),
`akuma-mmu`'s `{read,write}_current_user_val`, `akuma-mmu-raw`'s `read_user`/`write_user`/
`write_phys`, and amd64 `uaccess::{read,write}_val` were generic over `T: Copy`. `Copy`
does not promise that every bit pattern is valid (`bool`, a field-less enum, `char`,
`NonZeroU64` and `&'static u8` are all `Copy`), and it does not promise there is no
padding (`repr(C) { u32, u64 }` is `Copy` with 4 uninitialised bytes). So **safe code
could fill a reference with user-chosen bits, or copy kernel stack padding out to user
space.** Every function hid that with an internal `unsafe` that vouched for something its
bound did not say.

**After.** There is a new zero-dependency crate, `akuma-pod`. It has
`unsafe trait Pod: Copy + 'static`, impls for integers and `[T; N]`, the only byte-view
functions (`as_bytes{,_mut}`, `bytes_of{,_mut}`, `read_from`), and the `pod!` macro. The
macro admits a struct only after checking these at compile time:

1. Every listed field has the listed type. This is a by-value coercion
   `fn(&T) -> F = |v| v.f`, which works on `repr(packed)` fields that cannot be borrowed.
2. Every field type is `Pod`.
3. The fields **tile** the struct (`tiles()`): they are pairwise disjoint and their sizes
   sum to `size_of`. That means no padding, no unlisted field, and no field listed twice
   to paper over a hole.

Because the macro proves the claim, invoking it needs no `unsafe` at the call site. It
works inside `#![forbid(unsafe_code)]` crates, because the lint is not applied to
external-macro expansions (checked experimentally before relying on it). The wire-struct
crates therefore admit their own types:

- `akuma-syscalls-linux`: time, io, net, stat, signal, proc
- `akuma-net` `SockAddrIn`
- `akuma-syscalls-glue`: `Flock`, `ThreadCpuStat`, `SpawnOptions`, amd64 `EpollEventX86`
- `akuma-syscalls-abi` `X8664`
- `akuma-threading` sigframe
- amd64 signal frames
- `akuma-sched-amd64` `FxArea`
- `akuma-exec-core` `ProcessInfo`

**What the build proved.** Every call site compiled under the `Pod` bound, and every
admitted struct passed the tiling check. So **no existing call site was copying a padded
or invalid-bit-pattern type.** The hole was latent, and now the type system closes it
instead of a reviewer.

**`SiFields` was a `union`.** The `siginfo` payload was a union of the SIGCHLD and fault
layouts. `pod!` cannot admit a union, because its fields overlap by design. Writing the
smaller variant also leaves the tail as whatever bytes were there before; it was only
correct because `RtSigFrame::zeroed` cleared it first. It is now
`struct SiFields { words: [u64; 14] }` with `set_chld`/`set_fault` writers, so every byte
is a field. Callers in `akuma-exceptions` use the setters.

**Rule** (in [`syscalls.md`](../reference/subsystems/syscalls.md) and CLAUDE.md): typed
user copies take `T: Pod`. Never widen them back to `T: Copy`, and never write
`unsafe impl Pod` outside `akuma-pod`.

## 2. Per-crate changes

| component | before | after | what carries the claim now |
|---|---:|---:|---|
| `akuma-trap-aarch64` | 21 | 6 | `FrameHandle::new` states the whole `FRAME_BYTES` once. The private `word(i)`/`set_word(i, v)` assert `i < FRAME_WORDS` and do the volatile access. Every accessor, `snapshot`, `store` and `read/write_save_words` is safe. The `word_of!` offsets are const-asserted against the layout. `IrqFrame` got the same pair. |
| `akuma-user-access` | 22 | 18 | byte views come from `akuma-pod` (§1) |
| `akuma-locks-rw-cell` | 8 | 4 | private `read_guard`/`write_guard` hold the only two `UnsafeCell` derefs; six constructors call them |
| `akuma-slot-table` | 9 | 4 | private `occupant(ptr) -> Option<&T>`; `&self` bounds the returned lifetime |
| `akuma-mmu-raw` | 15 | 11 | physmap copies go through `akuma_phys::Frame` in per-page chunks. `pmm_frame` makes the one `Frame::from_pa` claim after `akuma_pmm::contains`; `resolved_frame` makes it after `mapped_range_ok`. `write_phys` is an alignment check plus `copy_to_phys(bytes_of(v))`. |
| `akuma-stack` | 7 | 4 | private `holds`/`read_word`/`write_word` (assert aligned and in-region) and `write_byte` |
| `akuma-dma` | 17 | 14 | `cell_ptr` uses `wrapping_add` (safe); `flush` uses `akuma_cpu::cache::clflush` + `barrier::dsb_ish` |
| `akuma-alloc` | 24 | 21 | `leak-instr` hooks are `OnceCopy<fn() -> u64>` instead of `usize` + `transmute`; `talc_alloc` is a safe fn with narrow blocks (null on zero size; the trailing canary uses `write_unaligned`, which a volatile `u64` store at an unaligned address was not) |
| `akuma-boot-amd64` | 16 | 14 | shootdown handler is `OnceCopy<fn()>`. `pub unsafe fn install_trampoline` is gone: the copy is private `copy_trampoline(TrampolinePage)`, and holding the page token is the proof. The VMM-layout token is a private `unsafe fn`. |
| `akuma-switch` | 24 | 22 | its private `bytes_of` and `static_location` moved to `akuma-pod` / `akuma_primitives::cell::LocationCell` |
| `akuma-primitives` | — | ±0 | new `LocationCell` (one site) replaces `preempt.rs`'s open-coded `&'static Location` round trip (one site). `akuma-threading`'s `PARK_SITES` uses it too. |
| `akuma-cpu` | 75 | 76 | `cache::clflush(va)` (amd64, `target_os = "none"`): the instruction `akuma-dma` needed, in the crate whose job it is |
| `akuma-pod` | — | 6 | new crate: `unsafe trait`, the primitive and array impls, two byte views, and the macro's impl |

The budget's forbid list also picked up `akuma-jvm-kernel` and `akuma-kernel-amd64`. Both
already forbade `unsafe`; the list had drifted.

## 3. Deliberately left alone

- **`akuma_switch::{vouch_stack, read_stack_word, read_published}`** are `pub` *safe*
  functions whose obligations exist only in their docs, and the forbid crate
  `akuma-threading` calls them. Fixing that honestly means token types, and it would
  *raise* the site count. That is a design task of its own.
- **`akuma-pagetable`'s amd64 `read_user`/`write_user` copies** cannot route through
  `akuma-phys`, because `akuma-phys` depends on `akuma-pagetable`. Doing it would create a
  dependency cycle.
- **`talc_dealloc`/`talc_realloc` canary reads** are unaligned `u64` reads, the same defect
  fixed in `talc_alloc`. They are dead behind `ENABLE_CANARIES = false`, so they were not
  touched here. Fix them before turning canaries on.
- **`akuma-slot-table::with_active_mut`** hands out `&mut T` while an `active_ref` borrow
  taken under the mask can outlive it. This aliasing question predates this work and was
  not changed.
- **`akuma-entry`'s raw reads** were not attempted.

## 4. Verification

- **Builds (all clean):**
  - AArch64 `cargo build --release`, also with `--features leak-instr`
  - amd64 `--features headless` and the default feature set
  - Taoxichuan, Shanghai and Denver for `x86_64-unknown-none` and `aarch64-unknown-none`
- **Host tests and clippy:** the touched crates pass clippy and their host tests, plus the
  pre-commit hook. New tests:
  - `akuma-pod`: tiling, byte views, packed and tuple structs
  - `akuma-mmu-raw`: `page_chunks_split_at_every_page_boundary`
  - `akuma-trap-aarch64`: 10 tests
- **Boot gate:** compared against the parent commit `c78bd0f7`. Both kernels were booted
  the same way (`MEMORY=2048 SNAPSHOT=1 scripts/cargo_runner.sh <elf>`), and the guest
  answered `scripts/vm_ready.py 2222`.

  | kernel | `[PASS]` | `[FAIL]` | `PASSED`/`FAILED`/`SKIPPED` result lines |
  |---|---:|---:|---|
  | baseline (`c78bd0f7`) | 92 | none | 315 |
  | new | 92 | none | the same 315 (`diff` empty) |

---

**Background:** [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) (the "named
function in the owning crate" method), [`UNSAFE_OWNERSHIP_TYPES_DESIGN.md`](UNSAFE_OWNERSHIP_TYPES_DESIGN.md)
(token types like `TrampolinePage`), [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md).
