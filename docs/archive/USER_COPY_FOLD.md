# Folding the range check into the user copy (2026-08-14)

`UNSAFE_AUDIT.md` §4 P0, executed. Kernel↔user copies used to be **two**
independent helpers — one that copied without checking anything, and one that
checked without copying — paired by hand at 167 call sites and, at ~41 of them,
not paired at all. They are one helper now.

This is the verbatim record: what the two helpers actually did, the premise that
nearly stopped the work and why it was wrong, the ~25 sites that needed a decision
rather than a rewrite, the bug that fell out, and the hole that is **still open**
because folding does not close it.

- **Landed:** the sweep, as one diff (26 files, +1046/−656) on top of the §5.7
  errno merge (`dd248880`).
- **Result:** `src/syscall/` **192 → 24** `unsafe`; `rump_proxy.rs` 12 → 0;
  `exceptions.rs` 107 → 97; `akuma-exec/src/process/mod.rs` 36 → 34. Host tests
  516 → 521.
- **Not fixed:** a mapped *kernel* VA still passes validation (§7). Recorded, not
  papered over.

---

## 1. The two helpers, and why the pair was unsound

### `copy_{from,to}_user_safe` — the copy, with a crash net and no checks

`crates/akuma-exec/src/mmu/user_access.rs`. An assembly byte loop plus a
trampoline:

```
__arch_copy_user_memory:  ldrb w3,[x1],#1 / strb w3,[x0],#1 / subs x2,x2,#1 / b.ne
__arch_copy_user_fault:   mov x0, #14 ; ret          // 14 = EFAULT
```

The Rust wrapper registers the trampoline as the thread's user-copy fault handler,
runs the loop, clears the handler. Recovery happens through the exception vector:
on an unmapped page the loop takes an EL1 data abort, and `src/exceptions.rs`
(`EC=0x25` with `ELR` inside kernel code and a non-zero registered handler)
**rewrites `ELR_EL1` to the trampoline** before returning — so the faulting
instruction is never retried and the function returns `Err(EFAULT)`.

That is the whole meaning of "safe" in the old name: **an unmapped user address
cannot panic the kernel.** Three consequences:

- **It validated nothing.** No range test, no user-vs-kernel test, no null-page test.
- **`unsafe` was there for the *kernel* side, not the user side.** The trampoline
  covers the user pointer; nothing covers the kernel buffer, so a `len` larger than
  the kernel array is an ordinary mapped over-read or over-write — no fault, no
  diagnostic. That invariant is exactly what a slice carries for free.
- **The direction was not enforced.** `copy_to_user_safe`'s entire body was
  `copy_from_user_safe(dst, src, len)`. The loop is symmetric; the direction lived
  only in the name, and swapping the arguments compiled.

### `validate_user_ptr` — the check, which also has side effects

`src/syscall/mod.rs`, and the name undersold it: bypass flag → reject `< 0x1000` →
`checked_add` on `ptr + len` → `end <= user_va_limit()` → mapped, **and if the
pages are lazy, demand-page them**. That last step allocates frames, takes the
address space's `as_lock`, and for a file-backed page reads through the VFS. Its
real second job was to make the range *present* so the copy could not fault.

### The pair, at a call site

```rust
if !validate_user_ptr(arg, 36) { return EFAULT; }                      // check the user side
if unsafe { copy_to_user_safe(arg as *mut u8, buf.as_ptr(), 36) }...   // copy, checking nothing
```

Two steps, no connection between them. 126 validate calls against 167 copies.

### What happened to a given address

| The user pointer is… | `validate_user_ptr` said | then the copy loop did |
|---|---|---|
| a normal mapped user page | true | copied. correct |
| a **lazy** user page (mmap'd, never touched) | **true** — and faulted it in as a side effect | copied. correct *because* the check ran |
| the same lazy page, check skipped | — | abort → trampoline → `EFAULT`, for a legitimate address |
| unmapped garbage | false | (if reached anyway) `EFAULT`, kernel survives |
| `ptr + len` wrapping past 2^64 | false | — |
| the null page (`< 0x1000`) | false | — |
| a TTBR1 kernel address (bit 63 set) | false (past the 48-bit limit) | — |
| **a mapped kernel RAM VA (e.g. `0x4100_0000`)** | **true** | **copied. silent kernel corruption** |

Row 3 is a live bug class at every unchecked site. Row 8 is §7, and it survives
the fold.

---

## 2. The premise that nearly stopped the work, and why it was wrong

The plan first offered was a *staged* one — slice API now, unskippable check later
— on the grounds that the check could not move into `akuma-exec` because
`ensure_user_pages_mapped` needs the PMM, `as_lock` and the VFS, which are
bin-crate concerns.

**That was wrong, and the pushback that it was wrong ("pmm and locks are already
part of the akuma-exec crate… we could just disable demand paging via a param")
was right.** Checking instead of assuming:

| What the objection claimed was bin-crate-only | Where it actually is |
|---|---|
| the PMM allocation | `akuma-exec` already depends on `akuma-pmm` directly (`Cargo.toml`), and `crate::pmm::alloc_page_zeroed` is a one-line wrapper over `akuma_pmm::alloc_page_zeroed` |
| the `as_lock` hold | `as_lock` / `with_as_locked` are `akuma-exec`'s own `Process` methods |
| the file fill (`crate::vfs::read_at*`) | `ExecRuntime` already carries **exactly those two hooks**: `read_at` (`runtime.rs:114`) and `read_at_by_inode` (`:116`) |
| `user_va_limit()` | not kernel state at all — the constant `0x0000_FFFF_FFFF_FFFF` |
| `lazy_region_lookup`, `map_user_page`, `is_current_user_page_mapped`, `track_user_frame` | `akuma-exec` already |

127 lines moved with **no new hook and no new dependency edge**. There was also
confirming evidence that should have been weighed before objecting:
`exceptions.rs` has a sibling `ensure_user_page_mapped`, and the 2026-08-14 DA/IA
merge had already moved *its* policy half into `akuma_exec::mmu`
(`COW_PILE_AUDIT.md` §12). The twin was half-moved.

**Lesson worth keeping:** the objection was about crate boundaries, and crate
boundaries are the one thing in this tree that `cargo tree` and a `Cargo.toml` can
settle in thirty seconds. `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.555 makes
the same point from the other direction ("counting `use` statements said the edge
was gone; `cargo tree` said otherwise").

---

## 3. The API

```rust
// safe fns: validate → prefault → copy
copy_to_user(dst_user: u64, src: &[u8])            -> Result<(), u64>
copy_from_user(dst: &mut [u8], src_user: u64)      -> Result<(), u64>
write_user_val<T: Copy>(dst_user: u64, val: &T)    -> Result<(), u64>
read_user_into<T: Copy>(dst: &mut T, src_user: u64) -> Result<(), u64>

// the same, with the prefault decision made explicit
copy_to_user_with / copy_from_user_with / write_user_val_with / read_user_into_with
enum Prefault { Yes, No }

// byte view of an array of ABI structs, so the caller's own length arithmetic
// (`ready_count * EPOLL_EVENT_SIZE`, `fd_set_bytes`) stays visible at the call site
as_user_bytes<T: Copy>(&[T]) -> &[u8]
as_user_bytes_mut<T: Copy>(&mut [T]) -> &mut [u8]

// the moved halves, now callable by the copy
user_range_ok(ptr, len) -> bool                     // pure; 5 host tests
validate_user_range(ptr, len, Prefault) -> bool
prefault_user_range(start, len) -> bool             // was `ensure_user_pages_mapped`
BYPASS_VALIDATION                                   // moved with the check it disables
```

`read_user_into` takes `&mut T` rather than returning `T` so no value has to be
fabricated out of user bytes. `src/syscall/mod.rs` re-exports the set, so every
syscall submodule reaches it through `use super::*` — which is how it already
reached the raw pair. `syscall::validate_user_ptr` survives as a thin forwarder
with `Prefault::Yes`, because plenty of arms validate a pointer they never copy
through (`futex` addresses, `mmap` args).

**`Prefault` is a parameter and not a default because prefaulting is dangerous in
two specific contexts** — inside a spinlock with IRQs masked, and inside an
exception handler. It allocates frames, takes `as_lock`, and can do block I/O.

---

## 4. The ~25 sites that needed a decision

About 140 of the 167 converted by rote. The rest fall into three groups, and a
blind sweep gets every one of them wrong.

### Group 1 — copies that must NOT demand-page (`Prefault::No`)

| Site | Held while copying |
|---|---|
| `sync.rs` futex-word read in `futex_enqueue_checked` | `FUTEX_WAITERS`, IRQs masked. The file's own header already argued this ("resolves through the byte loop's fixup rather than demand-paging under the hold"); the argument is now a parameter next to the code |
| `msgqueue.rs` `sys_msgrcv`, 4 copies | `MSGQUEUE_TABLE` inside `with_irqs_disabled` |
| `signal.rs` `rt_sigaction` oldact copy-out | `signal_actions.actions` |
| `term.rs` TCGETS / TIOCGWINSZ / TIOCGPGRP / `get_mode_flags` | the process's `TerminalState` |
| `exceptions.rs`, all 8 | an exception handler. A lazy page hit by a kernel→user copy is resolved *in place* by `try_resolve_el1_user_copy_lazy_fault`, which is the mechanism built for exactly this |
| `akuma-exec/src/process/mod.rs`, 2 | the clone path; both are diagnostic reads of a user stack |

`Prefault::No` still range-checks (a read-only page-table walk, safe under a hold),
so these sites gained a check they never had while keeping the behaviour they had.

### Group 2 — `validate_user_ptr` calls that had to stay

Three unrelated reasons, and only the second is about locks:

1. **It gates a caller-sized allocation.** `container.rs` ×4, `msgqueue.rs`
   `msgsnd`, `poll.rs` `ppoll`, `fs.rs` `readv`/`writev`, `net.rs` DNS, `proc.rs`
   spawn-stdin. Each is followed by `vec![0u8; len]` with a caller-controlled
   `len`; folding the check into the copy lets a bogus pointer allocate first and
   fail second.
2. **It keeps the prefault off a lock** — the pre-flight half of Group 1.
   `timerfd.rs` `gettime` validates *before* `TIMERFD_TABLE.lock()`, and that
   ordering is the entire reason the in-hold copy is safe. `msgrcv` and
   `sys_futex` are the same shape.
3. **It preserves error precedence.** `pipe2` must fail before two fds are
   allocated; `epoll_pwait` before the wait loop blocks; `io_setup` before the
   ring is allocated; `waitpid` before the zombie is reaped; `fb_draw`'s
   whole-buffer check keeps an all-or-nothing `EFAULT` instead of a partial draw.

### Group 3 — callers that stay raw on purpose

- **`copy_from_user_byte`**, whose only user is `copy_from_user_str`. The length of
  a NUL-terminated string is not known until it has been read, so there is no
  range to validate, and routing each byte through the folded helper would
  page-table-walk **per byte** — 4096 walks for a `PATH_MAX` path. The string
  reader does its own per-byte limit check and relies on the trampoline for
  mapped-ness, which is the right shape for an unknown-length read.
  `rump_proxy.rs`'s `copyinstr` now **reuses** it instead of hand-rolling the same
  loop.
- **`tests.rs`'s EFAULT recovery test**, because the trampoline is the unit under
  test: the folded API would reject its deliberately-invalid address at the range
  check and never reach the copy.

### Why per-chunk validation is not a cost problem

The worry that folding would re-walk the page table per chunk in a chunked loop
(`fs.rs` write, `fb_draw`, `getrandom`, `mremap`) does not hold: each chunk
validates **only its own bytes**, so the total walk across the loop is one pass
over the buffer, not one pass per chunk. The only real overhead is per-call
constant work. A site that validates a large range and then copies *the same
range* repeatedly would be the exception; there is none.

---

## 5. A real bug, found by converting

**`mremap`'s payload move (`src/syscall/mem.rs`) validated the source and never
the destination.** The copy-out to the freshly-created mapping went through the
raw helper with no check and no prefault, so a lazy page in the new mapping
faulted, `break`-ed out of the copy loop, and **silently truncated the moved
region** — the loop's `break` is indistinguishable from completion at the call
site. `copy_to_user` now prefaults it.

**No regression test.** The boot suite and the Tier 3 fork/CoW binaries pass
unchanged, which shows the sweep preserved behaviour, but neither exercises "move a
mapping whose destination pages are still lazy". A `userspace/` probe that
`mremap`s a large region and verifies the moved bytes is what would pin it; the
truncation was silent, so nothing fails loudly without one.

---

## 6. Divergences from Linux, recorded and deliberately not changed

Collected, with everything else known of this kind, in
[`LINUX_COMPATIBILITY_ISSUES.md`](LINUX_COMPATIBILITY_ISSUES.md) — which also
records that a real per-family ABI audit has never been done.

Each of these is a behaviour change, which does not belong in a deduplication
pass. All three have the same shape: the validate is the *condition* of an `if`
rather than a guard, so an unreadable pointer silently succeeds where Linux
returns `EFAULT`.

| Syscall | Today |
|---|---|
| `rt_sigaction` with an unreadable `act` | returns 0, installs nothing |
| `prctl(PR_SET_NAME / PR_GET_NAME / PR_GET_PDEATHSIG)` | returns 0, does nothing |
| `rt_sigtimedwait` with an unreadable `siginfo` | returns the signal, fills nothing |

Also pre-existing and now written down: **`timerfd_settime` prefaults while
holding `TIMERFD_TABLE`'s spinlock** — frame allocation, `as_lock`, possibly a
file read, under a lock. It predates this work (`validate_user_ptr` was already
called there from inside the hold) and the conversion neither widened nor fixed
it.

---

## 7. STILL OPEN: the check is unskippable, but it is the wrong check

The audit's "second win" was that folding closes the unchecked-destination hole.
**It does not**, and this is the most important thing in this document.

`add_kernel_mappings` (`crates/akuma-exec/src/mmu/mod.rs:710`, and the comment at
`:105`) identity-maps kernel RAM as **EL1-only 2 MB blocks in every user address
space**. `is_current_user_range_mapped` (`:1863`) walks TTBR0 and tests
`is_page_mapped_ptr` — **presence**, not EL0-accessibility. So:

- a mapped kernel VA is present in TTBR0 → it passes validation;
- the EL1-only permission does not stop the copy, because the byte loop runs at
  **EL1**;
- what keeps this from being reachable today is only that the user VA allocator
  avoids `[KERNEL_VA_START, kernel_va_end())` — a layout convention, not a check.

The fix is to test the leaf PTE's AP bits — "is this range mapped *as user
memory*" rather than "is this range mapped". `is_page_mapped_ptr` already walks to
the leaf, so it is contained. Two things to know before doing it:

1. It is a **behaviour change** and wants its own A/B plus a boot test that a
   kernel VA as a syscall destination now returns `EFAULT`.
2. `validate_user_ptr` deliberately does **not** exclude the kernel VA range, and
   the reason is recorded at the old site: Bun's JSC `mmap`s at `0x5000_0000`,
   which overlaps kernel RAM's identity window, and every such pointer is
   legitimate. An AP-bit test handles that correctly where a range exclusion does
   not — which is the argument for doing it this way.

---

## 8. Structural work that fell out, none of it planned

- Two literal ABI sizes became compile-time layout assertions:
  `size_of::<StackT>() == 24` and `size_of::<Siginfo>() == 128` (`signal.rs`). The
  copies used to state those numbers by hand; now a field added to either struct is
  a compile error rather than a silently short copy to userspace.
- A `StackT` declared **identically in both arms** of `sys_sigaltstack`, and a
  `Timespec` shadowing the parent module's identical one, became one each.
- Six copies of "read one user instruction word at fault time" in `exceptions.rs`
  collapsed into one `read_user_instr`, which is also where the `Prefault::No`
  reasoning for that whole file is written down.
- Two `ptr::write` + byte-array pairs (`eventfd`, `timerfd` read paths) became
  `to_ne_bytes()`.
- Five ABI structs gained `Copy` (`Stat`, `Statfs`, `MsgHdr`, `FBInfo`,
  `CloneArgs`, plus `Timespec`), which is the marker `write_user_val` uses for
  "plain ABI data, safe to move byte-wise".
- `clone3`'s partial-struct read is the one place a length is *not* `size_of::<T>()`
  — Linux's extensible-struct ABI lets `size` be shorter — and it now says so via
  `as_user_bytes_mut(slice::from_mut(&mut cl_args))[..struct_size]`.

---

## 9. Numbers

| | before | after |
|---|---:|---:|
| `unsafe` in `src/syscall/` | 192 | **24** |
| `unsafe` in `src/rump_proxy.rs` | 12 | **0** |
| `unsafe` in `src/exceptions.rs` | 107 | 97 |
| `unsafe` in `crates/akuma-exec/src/process/mod.rs` | 36 | 34 |
| host tests | 516 | **521** |

What the P0 grep returns now, and why it is not zero: `user_access.rs` itself,
`copy_from_user_byte`, the `tests.rs` trampoline test, and three
`process_tests.rs` comments that name the primitive while explaining history.
Every one is deliberate and says so at the site.

Judge the rest on the seam, not the line count: the kernel-side length invariant
became unstateable-wrong, the `(&raw const v).cast::<u8>()` + separately-written-size
pairing is gone from ~40 sites, and the pure half of the check
(`user_range_ok`) became host-testable — null page, wrapping length, the 48-bit
boundary, Go's 130 GB arena addresses, and zero-length ranges.

---

## 10. Verification

`docs/runbooks/verify-trim-fat-change.md`, Tiers 1–3, A/B against a worktree at
`9bc2dda8` (so the diff spans the errno merge *and* this sweep):

- 4/4 clippy configurations clean; 521 host tests, 0 failed.
- `fail_set` **empty** at SMP=1 *and* SMP=4.
- `pass_marker` 95 on both arms; `passed_marker` 276 (SMP=1) / 283 (SMP=4) —
  identical to baseline, which is the behaviour evidence that matters for a
  syscall-layer rewrite.
- All six Tier 3 exercises `ok` at both SMP levels (`elftest`, `forkprobe`,
  `cowstale`, `bssfork`, `bssfork 20 8 1`, `madvshared`).
- `host_timejumps: 0` on both arms. `bkl_stuck` 96 → 95 (load-driven, benign).
- Tier 4 not run: nothing here touches the PMM, the fault path or the reclaim
  escalation.

**One reading nobody could reproduce.** That gate run also reported
`host.failed: 1` / `host.tests: 418` — 103 short of 521, which is the shape of one
test binary aborting partway rather than a test failing. Five subsequent runs on
the identical tree (three bare `cargo test`, two full Tier 1) scored 521/0, and
`akuma-exec` alone — the only suite big enough to account for the gap — scored
236/0 three times. It is recorded because it is unexplained: the gate does not
capture `cargo test`'s output, so there is nothing left to diagnose. Teaching
`tier1_tests` to save the failing output is the change that would turn this from
noise into a finding.

---

## 11. Follow-ups this leaves behind

| | Item | Why it is not done here |
|---|---|---|
| 1 | **AP-bit test in `is_current_user_range_mapped`** (§7) | behaviour change; own A/B + boot test |
| 2 | **`mremap` destination regression test** (§5) | needs a `userspace/` probe; the bug was silent |
| 3 | Make `BYPASS_VALIDATION` per-thread with an RAII guard | it is a kernel-wide flag flipped by ~50 hand-paired `store(true)`/`store(false)` sites in the boot tests, so while it is on, validation is off for **every core**. No live leak path today (the "early returns" between pairs are all matches inside comments), but nothing enforces the pairing. The flag moved in this change, which is why the defect is written down here |
| 4 | Fix the three §6 divergences | each is a behaviour change |
| 5 | `timerfd_settime`'s prefault under `TIMERFD_TABLE` (§6) | pre-existing lock-ordering fix, unrelated to the copy API |
| 6 | P1: `#[repr(C)]` `Statx` + `SigFrame` (`UNSAFE_AUDIT.md`) | next item in that audit; the signal frame's 1120 bytes want one `copy_to_user` on the stack, which this API now makes trivial |

Item 6 is the one this work most directly unblocks: `UNSAFE_AUDIT.md` §4 P1 argued
that building a `SigFrame` on the kernel stack and issuing **one** `copy_to_user`
would delete the `ensure_cow_page_writable` pre-flight dance. That single
`copy_to_user` now exists.

---

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — §4 P0 (the plan), §4.0/§4.0a (the
  status), §4 P1 (what this unblocks)
- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  — Phase 5, and §5.7 for the errno table that landed immediately before it and
  for why it went first
- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12 — the DA/IA demand-paging merge,
  which had already moved the *fault path's* half of this policy into `akuma-exec`
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — why
  block I/O must never run under an IRQ-masked spinlock, which is what `Prefault`
  encodes
- [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md) —
  the current-state rule for which form to use at a call site
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  — the gate, and the recorded unreproducible reading
