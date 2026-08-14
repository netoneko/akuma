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
- **Not fixed by the fold, fixed later the same day:** a mapped *kernel* VA passed
  validation, because the check tested presence and not EL0-accessibility. §7 has
  the AP-bit fix, what it broke on the way in, and what it dragged in with it
  (`BYPASS_VALIDATION` had to become per-thread).

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

Row 3 is a live bug class at every unchecked site. Row 8 is §7: it survived the
fold, and was fixed separately — see that section.

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

**No regression test — added 2026-08-14.** The boot suite and the Tier 3 fork/CoW
binaries passed unchanged, which showed the sweep preserved behaviour, but neither
exercised "move a mapping whose destination pages are still lazy". That probe now
exists: `userspace/forktest/c_stress/mremapmove.c`, in `verify_trim.py`'s Tier 3
list. It grows a 4 MB region to 8 MB with `MREMAP_MAYMOVE` — the destination of a
grow is a brand-new anonymous mapping and so is lazy in its entirety — and checks
every byte, the zero tail, and a sparsely-touched source. `ALL PASS` on real Linux
arm64.

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

## 7. The check was unskippable but wrong — FIXED 2026-08-14

> **Status: DONE.** `is_current_user_range_mapped` tests the leaf PTE's AP bits.
> Recorded below as it was written, because the reasoning for *why an AP test and
> not a range exclusion* is the part worth keeping. What landed, and the two things
> that were not obvious until it was built:
>
> - **The walk is shared, so the two questions had to be separated by name.**
>   `is_page_mapped_ptr` (presence) and `is_page_user_accessible_ptr` (AP-gated) are
>   now two one-line callers of one `resolve_user_leaf`. Only the *range* check —
>   the user-pointer path — moved to the strict one. `is_current_user_page_mapped`
>   stays presence, and that is load-bearing: its callers are demand-paging and
>   teardown paths asking "is this VA filled in yet", and a `PROT_NONE` page reading
>   as *un*mapped there would make the next fault re-map it read-write.
> - **`prefault_user_range` had to be re-checked afterwards.** It skips pages that
>   are already present — deliberately, for the same reason — so on its own it
>   reports success for a range that is mapped but EL1-only, which is precisely the
>   case being closed. `validate_user_range` now re-asserts
>   `is_current_user_range_mapped` after a successful prefault. Without that line the
>   whole change is defeated on every `Prefault::Yes` path, which is most syscalls.
>
> A second, correct consequence fell out: a `PROT_NONE` page (`user_flags::NONE` is
> `AP_RO_EL1`) is now rejected as a syscall buffer, which is what Linux does. A
> read-only user page still passes — the test is EL0 *reachability*, not
> writability, and an EL1 write to a read-only user page is how a CoW break starts.
>
> Boot test: `kernel_va_rejected_as_user_pointer` (`src/tests.rs`), five legs. It
> asserts the kernel VA is **present** as well as rejected — asserting only the
> rejection would pass just as well against an unmapped address and prove nothing —
> plus a real user page still accepted, a `PROT_NONE` page rejected, and
> `validate_user_range(.., Prefault::Yes)` rejecting it, which is the leg that pins
> the re-check above.
>
> **It broke two classes of boot self-test, and both were the change working.**
> Neither showed up in review; both showed up on the first clean boot. Recorded
> because "the AP test is contained, `is_page_mapped_ptr` already walks to the leaf"
> was true of the *diff* and badly understated the blast radius.
>
> 1. **`test_futex_wake_one_of_two` — `BYPASS_VALIDATION` is global.** It is the only
>    futex test with **two** syscalls inside one bypass window (`FUTEX_WAKE(1)`, then
>    `FUTEX_WAKE(INT_MAX)`). The waiter woken by the first runs in between and ends
>    with its own `BYPASS_VALIDATION.store(false)` — which, on a kernel-wide flag,
>    closed the *main thread's* window. Its second wake then validated a kernel
>    `.bss` address for real, took `EFAULT`, and the second waiter was never woken:
>    `only 1/2 threads unblocked`. That race was always there; under a presence-only
>    check it simply had no effect. **This is §11 item 3 — "no live leak path today"
>    — becoming live**, and it is why the flag is now per-thread (see below).
> 2. **Five `epoll` tests pass kernel-STACK buffers with no bypass at all.**
>    `test_epoll_socket_waker`, `test_epoll_multi_poller_pipe`,
>    `epoll_pipe_close_write`, `epoll_eventfd_write_triggers_event`,
>    `epoll_del_removes_interest` all hand `&raw const ev` to `epoll_ctl`/`epoll_pwait`.
>    Kernel stacks are in the identity-mapped window, so they are EL1-only and now
>    correctly `EFAULT` (`err=0xff…f2`). They had simply never needed the bypass.
>    Each takes a `BypassValidationGuard` now — and `test_epoll_multi_poller_pipe`
>    needs **three**, one per poller thread, which is the per-thread flag being
>    correct rather than convenient.
>
> **`BYPASS_VALIDATION` is per-thread as of this change**, with the same
> `store`/`load` signatures, so none of the ~85 call sites changed. There is also a
> `BypassValidationGuard` (RAII, restores what it found, so windows nest) — the other
> half of §11 item 3. What is **still open** there: the ~85 raw `store(true)` /
> `store(false)` pairs are still hand-written, so an early return or panic between a
> pair leaves that one thread's slot on. Per-thread makes that a bounded, local bug
> instead of a kernel-wide one; the guard is what fixes it, one site at a time.
>
> **A latent hazard the test walked straight into — FIXED 2026-08-14.**
> `read_current_pid` (`process/children.rs`) ends by dereferencing user VA
> `PROCESS_INFO_ADDR` (0x1000), and the only thing gating that read is
> `ttbr0 != boot_ttbr0`. "Not the boot address space" is **not** the same as "there
> is a process here": a bare `UserAddressSpace::new()` — which several boot tests
> construct and `activate()` — is a non-boot TTBR0 with nothing mapped at 0x1000, so
> the guard passes and the read is a wild EL1 access. The first version of this test
> activated such an address space and reached that read through
> `address_space_owner_pid_for_fault`'s fallback; the VM wedged with no output, and
> the only reason it was easy to localize was per-step console prints. The test now
> registers a process and activates **its** address space, so the TTBR0→pid lookup
> succeeds and the fallback is never reached.
>
> `read_current_pid` now asks the page tables before it reads them:
> `is_current_user_range_mapped(PROCESS_INFO_ADDR, size_of::<ProcessInfo>())`, and
> `None` if that fails. **Not** the `owner_pid_for_l0_phys` guard this paragraph
> originally proposed — writing it down made clear that "is this L0 owned by a live
> process" is a different question from the one the `unsafe` depends on, answers it
> with an O(`MAX_PROCESSES`) table scan on a path kernel threads take repeatedly
> (where a four-level walk suffices), and would not have caught one case this
> misses: an L0 with no info page mapped is *exactly* what wedges. The AP-gated
> predicate rather than the presence one because this is a read of user memory from
> EL1, the same question `validate_user_range` asks of any syscall buffer — but
> reached through `mmu` directly, since going through `user_access` would recurse
> (`validate_user_range` → `address_space_owner_pid_for_fault` → here). Every site
> that maps the page (`image.rs` on exec, `mod.rs` on fork and on the post-CoW
> re-map) uses `user_flags::RO` = `AP_RO_ALL`, EL0 bit set, so it passes.
>
> Regression test: `test_read_current_pid_rejects_bare_address_space`
> (`src/tests.rs`), which activates a bare address space and calls
> `read_current_pid` — **on an unfixed kernel it hangs rather than failing**, the
> honest failure mode for a wild read. It asserts two things, because `pid == None`
> alone would also pass against a build that never reached the fallback; the second
> assertion is that nothing is mapped at `PROCESS_INFO_ADDR`. It skips itself if the
> running thread has a `THREAD_PID_MAP` entry, which would return from the fast path
> above and prove nothing. Verified with the same `verify_trim.py` A/B as the rest of
> §10a: `fail_set` empty at SMP=1 and SMP=4, host 528, clippy 4/4 clean, every
> counter unmoved.

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

### 10a. The AP-bit follow-up (§7), verified 2026-08-14

`scripts/verify_trim.py` on both arms — this tree against a worktree at `edd91fe7`
— carrying §7's AP-bit check, the per-thread `BYPASS_VALIDATION`, §5's
`mremapmove` probe and the `mmu` walk merge
(`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §8 item 8). **The whole diff:**

```
< host.tests: 521            > host.tests: 527
                             > smp1.ex.mremapmove: ok
< smp4.bkl_stuck: 108        > smp4.bkl_stuck: 95
                             > smp4.ex.mremapmove: ok
```

Every other line is byte-identical, which for a change that alters *what a syscall
accepts* is the whole point:

- 4/4 clippy configurations clean on both arms.
- `fail_set` **empty** at SMP=1 *and* SMP=4, both arms.
- `pass_marker` **95**, `passed_marker` **276** (SMP=1) / **283** (SMP=4) — the
  same on both arms, and the same as §10's numbers below.
- All six original Tier 3 exercises `ok` at both SMP levels on both arms, plus the
  new `mremapmove` on this one.
- `host_timejumps: 0` everywhere. `bkl_stuck` 108 → 95 at SMP=4 is load-driven
  (`BKL_TAG511_STORM` — it moves on an unmodified tree too).
- `stack_overflow: 1` on every arm is the detector's own self-test firing
  (`exercised=true detected=1`), not a fault.
- Host tests are **528** on the tree as it stands: the gate's 527 was built before
  the bypass guard's nesting test was added. 521 → +6 for the `ToggledGuard` merge,
  +1 for that.

**Getting there took three boots, and the first two are the interesting part** —
both failures were the change working, and both are written up in §7: a global
`BYPASS_VALIDATION` window closed by another thread (`test_futex_wake_one_of_two`),
and five `epoll` tests handing kernel *stack* buffers to syscalls with no bypass at
all. A behaviour change to user-pointer validation cannot be judged on the diff; it
has to be booted.

### 10b. The original fold

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
| 1 | ~~**AP-bit test in `is_current_user_range_mapped`** (§7)~~ **DONE 2026-08-14** — see the status block on §7 for what it actually took: one shared `resolve_user_leaf`, two *differently named* predicates, and a re-check after `prefault_user_range` without which the change is a no-op | 0 left |
| 2 | ~~**`mremap` destination regression test** (§5)~~ **DONE 2026-08-14** — `userspace/forktest/c_stress/mremapmove.c`, three phases (resident grow, grown-tail-zero, sparse grow), calibrated `ALL PASS` on real Linux arm64 and added to `verify_trim.py`'s Tier 3 `EXERCISES`. Megabyte regions on purpose: a fix that only prefaulted the head would still fail phase 1 | 0 left |
| 3 | ~~Make `BYPASS_VALIDATION` per-thread with an RAII guard~~ **MOSTLY DONE 2026-08-14** — it was a kernel-wide flag flipped by ~85 hand-paired `store(true)`/`store(false)` sites, so while any thread had it on, validation was off for **every thread on every core**. This row said "no live leak path today"; the §7 AP-bit fix made one, and `test_futex_wake_one_of_two` found it on the first boot (a woken waiter's `store(false)` closing the main thread's window mid-test). It is now a per-thread array behind the same `store`/`load` signatures — **zero call-site changes** — plus a `BypassValidationGuard` (RAII, restores what it found so windows nest). **Still open:** the ~85 raw pairs are still hand-written, so an early return or panic between a pair leaves that *one thread's* slot on. Per-thread turns a kernel-wide hole into a bounded local one; converting the pairs to the guard is what closes it | mostly |
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
