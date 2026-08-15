# Phase 7: `#[repr(C)]` `SigFrame` + `Statx` (2026-08-14)

`UNSAFE_AUDIT.md` §4 P1's three blocks, done: the rt_sigframe **builder**
(`exceptions.rs`, ~130 hand-offset writes into user memory), the rt_sigframe
**reader** (`do_rt_sigreturn`, ~40 hand-offset reads), and `sys_statx`'s 20-write
fill of a byte buffer. All three are now `#[repr(C)]` structs whose offsets are
derived with `offset_of!` and checked with `const _: () = assert!(…)`, so a layout
change is a build failure instead of a corrupted userspace context.

- **Struct + host tests:** `crates/akuma-exec/src/threading/sigframe.rs` (new)
- **Call sites:** `src/exceptions.rs` (`try_deliver_signal`, `do_rt_sigreturn`),
  `src/syscall/fs.rs` (`sys_statx`)
- **Phase record:** `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` Phase 7

---

## 1. What the offsets actually were

The frame is 1120 bytes: `siginfo_t` (128) + `ucontext_t` header (176) +
`sigcontext` (280) + FPSIMD record (528) + `_aarch64_ctx` terminator (8). Every one
of those numbers appeared in the old code as a literal beside a comment naming the
field it was supposed to be — `mc.add(256)` with `// sp` next to it, and nothing
connecting the two. The struct now derives all of them, and the compile-time
assertions pin them to the values the hand-written version encoded.

They agree. That is the first result worth stating plainly: **the struct
reproduces the old layout byte for byte**, which is what makes this a
behaviour-preserving change rather than an ABI edit. The assertions are in the
module and they hold on every profile the gate compiles.

## 2. Two divergences from Linux, found and deliberately not fixed

Writing `sigcontext` out as a type is what surfaced these; they were invisible as
long as the layout lived in `add(N)` calls.

Linux's `struct sigcontext` ends with
`__u8 __reserved[4096] __attribute__((__aligned__(16)))`. That alignment attribute
does two things this frame does differently:

1. **The FPSIMD record sits at frame+584, not +592.** `aligned(16)` pads
   `sigcontext` from 280 to 288 before `__reserved` starts; this frame packs it at
   280. A handler that walks the `_aarch64_ctx` chain from
   `&uc.uc_mcontext.__reserved` looks 8 bytes past where the record actually is.
   The old code knew about the resulting misalignment without naming the cause —
   its comment explained that `vregs_dst` is only 8-byte aligned and used a byte
   copy to avoid `stp q`.
2. **`__reserved` is 536 bytes, not 4096.** The frame is sized for exactly the
   FPSIMD record plus its terminator: 1120 bytes of user stack instead of ~4.7 KB.

Neither is changed here. Both are ABI changes that want their own A/B, and this
pass's whole claim is that nothing moved. They are recorded in the module header
so the next reader does not re-derive them from the Linux headers, which is how
they were found.

`vregs` is `[u64; 64]`, not `[u128; 32]`, for the same reason: `u128` carries
16-byte alignment, which would pad the record and move every offset after it. The
type has to reproduce the layout, not improve it.

## 3. What the copy changed, and what it did not

The builder now fills a frame on the kernel stack and issues **one**
`write_user_val_with(new_sp, &sf, Prefault::No)`; the reader issues one
`read_user_into_with(&mut sf, sigframe_sp, Prefault::No)`.

**`Prefault::No` at both sites, for different reasons.** Delivery runs on a
fault-handling stack, where `prefault_user_range`'s frame allocation and `as_lock`
acquisition are not allowed (`USER_COPY_FOLD.md` §4 group 1) — and the
`ensure_user_page_mapped` + `ensure_cow_page_writable` pre-flight above it is
already the fault-safe form of that job. Sigreturn passes `No` to preserve
behaviour: it used to require both frame pages to be *present* and give up
otherwise, and a frame SP pointing at an unfaulted lazy page is a corrupt frame,
not something to demand-page.

### The pre-flight does **not** go away, and `UNSAFE_AUDIT.md` §4 P1 is wrong about that

P1 argued that a single `copy_to_user` "would delete the `ensure_cow_page_writable`
pre-flight dance". It does not, and the reason is worth recording because it is a
property of the validation layer, not of this frame:

> `is_current_user_range_mapped` tests **EL0 accessibility**, not writability. Its
> own doc comment says so — `AP_RO_ALL` passes, deliberately, "because an EL1 write
> to one is how a CoW break gets triggered".

So a CoW-demoted stack page validates clean and the copy still needs the page to be
writable before it writes. The EL1 data-abort path *would* recover
(`try_resolve_el1_cow_fault`), but the copy helpers install a fault trampoline that
returns `EFAULT` first, so the copy would fail rather than resolve. The pre-flight
is what makes the copy succeed, and it stays.

### Two behaviour changes, both narrowing

- **A frame SP that is not EL0-accessible is now rejected.** The old writes
  followed such a pointer into whatever it named; kernel RAM is identity-mapped
  EL1-only in every user address space, so a mapped kernel VA passed the old
  presence test (`USER_COPY_FOLD.md` §7). Delivery now declines and the caller
  applies the default action; sigreturn returns `None` as it does for any invalid
  frame.
- **`sigframe_sp + 1120` overflowing is now rejected** rather than wrapping, via
  `user_range_ok`'s `checked_add`.

Both can only fire where the old behaviour was already wrong.

### One ordering nuance, noted in the code

`take_restore_sigmask()` *consumes* the mask `rt_sigsuspend` armed, and it now runs
before a copy that can fail — which the old per-field writes could not. The failure
path declines delivery, which ends in the default action, so there is no thread left
to restore the mask into. Recorded at the call site rather than worked around.

## 4. The NEON area was three bare literals

Both directions reached into the trap frame's FP save area at `+304` (vregs),
`+816` (fpcr) and `+824` (fpsr). Those are now one `SyncFrameNeon` struct and one
named `SYNC_FRAME_NEON_OFFSET`, with `unsafe fn sync_frame_neon(frame)` as the only
place the cast happens.

This is **not** foldable into `UserTrapFrame`, and the reason is a trap worth
keeping: the EL0 **sync** frame and the EL0 **IRQ** frame are both 832 bytes and
have *different* NEON offsets (+304 vs +288, with FPCR/FPSR at +816/+824 vs
+800/+808). `exceptions.rs`'s own assembly comment says they are "NOT
interchangeable". Extending the shared struct would have silently given the IRQ
frame the sync frame's layout. Every current `try_deliver_signal` and
`do_rt_sigreturn` caller is in the sync handler, which is what the function's safety
contract says.

## 5. Numbers

| | Before | After |
|---|---:|---:|
| `core::ptr::{write,read,copy_nonoverlapping}` in `exceptions.rs` | 110 | 13 |
| `(*frame)` derefs in `exceptions.rs` | 75 | 34 |
| `unsafe {` blocks in `exceptions.rs` | 90 | 91 |
| `core::ptr::*` in `src/syscall/fs.rs` | 20 | 6 |
| `unsafe {` blocks in `src/syscall/fs.rs` | 4 | 3 |
| `exceptions.rs` lines | 4747 | 4693 |
| `fs.rs` lines | 2518 | 2563 |
| Host tests | 528 | 533 |

Whole change: 8 files, +371 / −281, of which the new module is 466 lines —
**more lines than it removes**, because a checked layout is a type plus its
assertions plus its tests, while an unchecked one is a comment next to an
integer. The `fs.rs` growth is the same trade: `Statx` as 24 named fields and 13
`offset_of!` assertions is longer than 20 `write(p.add(N))` calls.

### Image size, `extreme-size` (the profile with a 4.0 MB floor)

A/B against a `git worktree` at the parent commit, both built with
`scripts/build_extreme_size.sh`:

| | mine | base | delta |
|---|---:|---:|---:|
| `.text` | 454,976 | 455,000 | **−24** |
| `.rodata` | 66,791 | 66,743 | +48 |
| `.data` | 33,568 | 33,568 | 0 |
| `.bss` | 268,320 | 268,320 | 0 |
| ELF total | 628,376 | 628,376 | **0** |

The two ELFs have different hashes and separate `target/` dirs, so this is a real
A/B and not one arm measured twice — a trap this repo has hit before
(`docs/archive/PMM_EXTRACT.md` §8). Net effect on the size floor: none.

The frame is now a 1120-byte **kernel stack** local in two functions
(`try_deliver_signal` on the fault stack, `do_rt_sigreturn` on the syscall stack,
never both at once). `UNSAFE_AUDIT.md` §4 P1 had already argued that is
comfortable against the trimmed 96 KB system stack; the boot suite's
`stack_overflow` count stayed at 1 — the deliberate canary test — on every run.

**The `unsafe` *block* count in `exceptions.rs` went up by one, and that is the
honest number.** The audit's `−281` was in unsafe *operations* — writes, reads and
derefs — and those are what collapsed (110 raw pointer ops → 13). The blocks moved
the other way because the two sites that used to be one enormous `unsafe { … }` are
now small ones around the two pointer→reference conversions that remain. Judge this
phase on §3.3's operation count and on the offsets becoming compile-checked, not on
block count — the same lesson Phase 4 recorded about line count.

## 6. Verification

Gate: `docs/runbooks/verify-trim-fat-change.md`, via `scripts/verify_trim.py`,
against a baseline run on the parent commit (`8ff2a1c5`) of the same tree.

**Tiers 1–3, twice, against the baseline run on the parent commit.** Every runtime
measurement is identical; the only diffs are the +5 host tests this change adds and
`bkl_stuck` (load-driven, 96 vs 93).

| | base | mine |
|---|---|---|
| clippy × 4 configs | clean | clean |
| host tests / failed | 528 / 0 | **533** / 0 |
| `smp1` booted, `[PASS]`, fail set | True, 95, empty | True, 95, empty |
| `smp4` booted, `[PASS]`, fail set | True, 95, empty | True, 95, empty |
| exercises (7) at both widths | all `ok` | all `ok` |
| `stack_overflow` (the canary test) | 1 | 1 |
| `host_timejumps` | 0 | 0 |

`elftest`, `forkprobe`, `bssfork` (both invocations), `cowstale`, `madvshared` and
`mremapmove` all `ok` at SMP=1 and SMP=4, twice. The first attempt failed the three
`no-tests` clippy configs on an unused import — `SIGFRAME_MCONTEXT`/`SIGFRAME_FPSIMD`
are now used only by the `kernel_tests` re-exports — which is exactly why the gate
compiles four configurations and not one.

### The SMP=2 detour, and what it found

SMP=2 is **not** in the gate's default set (`--smp 1,4`), so it was run as extra
coverage. `cowstale` failed there — a process SIGSEGV, which looks exactly like a
regression in a change that touches the fault path.

It is not one. **Both arms fail at the same rate:** 5 runs each at SMP=2, mine 2/5,
baseline 2/5 (3/6 and 2/6 counting the first round), with a byte-identical
signature on every occurrence — `FAR=0x420908 ELR=0x403a90 ISS=0x4f`, `[WPF] …
va=0x420000 cow_ref=0 … ap_rw=true`, always `pid=232`. `ap_rw=true` is the tell: the
page table **already grants** the write, so the fault was taken before a sibling
repaired the page and judged after — the stale-write-fault class that
`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §0/§12 root-caused and `COWSTALE_FORK_THREAD_SEGV.md`
records as solved, whose absorb (`stale_write_fault_absorbed`) has a residual hole
at this width. Its boot test passes in the same boot that then crashes.

The new `[signal] … frame copy … failed` marker fired **zero** times in every log on
every arm, so the new EFAULT path was never taken; and no `[sigreturn] WARNING`
lines appeared. Whatever this is, no signal was involved in it.

Recorded as a finding, not fixed here: see
[`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
→ "Known-benign", the SMP=2 row.

**Method note.** A single run on each arm said "mine fails, baseline passes" — the
most alarming possible reading, and wrong. Five per arm reversed it. This is the
third time in this repo's history that a 1-of-1 stress result pointed at the wrong
conclusion (`project_stress_ab_needs_deterministic_probe`, and the two
mis-attributions in the runbook's F8 row).

### `extreme-size`

Builds clean; `.text` −24 bytes, ELF byte size unchanged (see §5).

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — §4 P1, the plan this closes
- [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) — §7 (the AP-bit test this relies on),
  §4 group 1 (why `Prefault::No` on a fault stack), §11 item 6 (this item)
- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  — Phase 7
- [`../reference/subsystems/exceptions.md`](../reference/subsystems/exceptions.md) —
  the EL0 sync/IRQ frame layouts
