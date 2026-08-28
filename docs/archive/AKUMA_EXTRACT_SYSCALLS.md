# Extracting the syscall ABI: `akuma-syscalls-linux`

Status: **crate 1 built and verified, 2026-08-28.** Crate 2 (the shape crate)
proposed and deliberately not started — see §7.

Raised 2026-08-27 while closing out the syscall performance audit
([`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md),
[`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md)) as the first
work item of the syscalls-refactor branch. This document was the proposal
(`proposals/syscall-abi-and-shape-crates.md`); it is now the record of what was
actually built, what the build found, and what it deliberately left alone.

---

## 1. The problem

`src/syscall/` was **16,969 lines** living in the bin crate, so none of it was
reachable from any library crate and none of it was host-testable:

```
3189 fs.rs   2191 net.rs   1854 proc.rs   1546 poll.rs   1442 mem.rs
1245 mod.rs  1131 sync.rs   1116 unixsock.rs  569 term.rs  482 pipe.rs  ...
```

This was not hypothetical. It was **the same failure the errno table already
had**, and `crates/akuma-primitives/src/errno.rs` exists because of it:

> Errno values were spelled five ways in this tree, and every spelling had the
> same cause as the other duplications this crate exists to end: **the table
> lived somewhere the other caller could not reach.** The bin crate kept 29
> *pre-negated* `u64` consts private to `src/syscall/mod.rs`; `akuma-net` kept
> 25 *positive* `i32` consts in `socket::libc_errno` because a library crate
> cannot reach the bin crate's privates […] 17 names were defined twice, in two
> representations.

That argument was made and won for errno
([`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
§5.7). The identical divergence was in progress for **struct layouts**:

| symbol | where |
|---|---|
| `Timespec` | `src/syscall/mod.rs`, `src/sync_tests.rs`, `src/process_tests.rs` |
| `LocalTimespec` | `src/syscall/timerfd.rs`, `crates/akuma-time/src/lib.rs` |
| statx timestamp | `statx_timestamp` and `StatxTimestamp`, `src/syscall/fs.rs` |

Five spellings of the same 16-byte structure, and the two named `Local*`
carried the reason in the name: `akuma-time` is a crate and could not reach the
bin crate's definition, so it made its own. `nr::` (261 lines of syscall
numbers) was private to the bin crate for the same reason.

The failure mode here is worse than for errno. A wrong errno is visible at the
call site; **a wrong field offset or flag bit does not crash, it corrupts** —
the class of bug a QEMU boot is worst at catching and a host test is best at.

## 2. What was built

`crates/akuma-syscalls-linux` — 1,781 lines, **no dependencies**, `no_std`,
modelled on `akuma-boot` (the ABI-decode crate) rather than on `akuma-net-yarn`
(the shape crate).

| module | holds |
|---|---|
| `nr` | the 193-entry syscall number table |
| `flags` | `open` / `at` / `fcntl` / `map` / `prot` / `mremap` / `madvise` / `poll` / `epoll` |
| `time` | `Timespec`, `Timeval`, `Itimerval`, `Timex` |
| `stat` | `Stat`, `Statx`, `StatxTimestamp`, `Statfs`, `makedev` |
| `io` | `IoVec`, `PollFd`, `EpollEvent`, `AioRingHeader` |
| `signal` | `StackT`, `Siginfo`, `SigChld`, `KernelSigaction`, `SIG_*`, `CLD_*` |
| `proc` | `CloneArgs`, `Rlimit`, `Sysinfo`, `clone_flags`, `wait_options`, `wait_idtype`, `rlimit` |
| `net` | `MsgHdr`, `Ucred`, `SockAddrHw`, `IfConfHdr`, `sock_flags`, the `ifreq` sizes |

Every call site keeps its old spelling. The bin crate's `src/syscall/mod.rs`
re-exports the whole set (`pub use akuma_syscalls_linux::{…}`), exactly as it
already re-exported the errno table, so the submodules reach them through
`use super::*` unchanged; `akuma_exec::process::open_flags` and
`crate::syscall::{fs::Stat, poll::EpollEvent, mem::MAP_FIXED}` are aliases now
rather than definitions.

### What is deliberately *not* in it

- **errno.** Already in `akuma_primitives::errno`, which is the right home. The
  crate depends on nothing at all and does not copy it — a second copy is the
  exact mistake it exists to prevent.
- **Akuma-specific types** — `SpawnOptions`, `ThreadCpuStat`, the container
  syscalls. Those are Akuma ABI, not Linux ABI. The membership test is "can it
  be checked against a Linux header?", and they cannot.
- **`sockaddr_in` / `sockaddr_un`.** Already in `akuma-net`
  (`socket::SockAddrIn`, `unix::SockAddrUn`) — already a library crate, already
  reachable, which is the condition this crate exists to create. Moving them
  would have been motion, not de-duplication, and would have put a dependency
  on `akuma-net` into what is otherwise a leaf. The proposal listed the
  "sockaddr/ifreq family"; only the `ifreq` half moved, for this reason.
  `sys_ioctl_siocgifconf`'s `GifreqAddr` record stays in `src/syscall/net.rs`
  because it *embeds* a `SockAddrIn` — with a `const _` assertion there tying
  its size to the crate's `SIZEOF_IFREQ`.

## 3. What the tests buy

**34 host tests and 83 `const _` layout assertions**, all runnable in
milliseconds by `cargo test` with zero mocking, because none of it touches
kernel state.

Assertions are written unconditionally for aarch64 LP64 and are **not**
`cfg`-gated: a second architecture is not planned, and gating them would mean
the host test run — the only place they are ever checked — skipped all of them.

The tests are not restatements of the `offset_of!` assertions. Each one names a
way the ABI is actually got wrong:

- `epoll_event_array_stride_is_16_not_12` — aarch64's `struct epoll_event` is
  **not** packed, unlike x86-64's. Builds an array of two and checks the second
  starts at byte 16.
- `o_directory_is_the_arm64_value_and_o_tmpfile_contains_it` — aarch64 keeps the
  **32-bit ARM** fcntl values, so `O_DIRECTORY` is `0o40000`, not the
  asm-generic `0o200000`. `O_TMPFILE` is `__O_TMPFILE | O_DIRECTORY` and
  inherits the split; "correcting" one silently stops `sys_openat` rejecting
  tmpfiles, so apk-tools 3 writes into a directory fd instead of taking its
  `.tmp` + `renameat` fallback.
- `kernel_sigaction_is_handler_flags_restorer_mask` — the *kernel's*
  `struct sigaction` is not libc's. Getting the order wrong runs every handler
  with another handler's flags.
- `clone_args_prefix_survives_a_short_copy` — `clone3` copies
  `min(size, sizeof)`, which is what makes `clone_args`' field order
  unchangeable.
- `stat_blksize_is_32_bit_and_padded` — the canonical shape of the bug: one
  field's width wrong, everything after it shifted four bytes, nothing crashes.
- `no_two_syscall_numbers_collide` — 193 entries, all pairs. The comment on 23
  of them in `src/syscall/mod.rs` claimed exactly this ("so a stray digit can't
  silently drift onto the wrong syscall the way `SCHED_SETSCHEDULER`'s body
  did"); nothing had ever checked it.
- `sysinfo_procs_and_totalhigh_straddle_the_alignment_hole` — see §5.

## 4. Definitions collapsed

Per `verify-trim-fat-change.md` § "What to report", line-count deltas are the
wrong metric. The count that matters:

| definition | copies before | copies after |
|---|---|---|
| `struct timespec` | 5 (2 representations) | 1 |
| `struct timeval` | 3 (2 representations) | 1 |
| `struct pollfd` | 2 | 1 |
| `struct statx_timestamp` | 2 | 1 |
| `O_NONBLOCK` | 4 (2 widths + a bare `0x800`) | 1 |
| `O_CLOEXEC` / `EPOLL_CLOEXEC` | 3 | 1 (+ 1 alias, tested equal) |
| `AT_SYMLINK_NOFOLLOW` | 2 | 1 |
| `SIGINFO_SIZE` / `sizeof(siginfo_t)` | 2 literals | 1, derived from the type |
| `sizeof(struct aio_ring)` | 2 literals | 1, derived from the type |
| `struct sysinfo` | 0 (§5) | 1 |

Net: **17 files changed, 231 insertions, 734 deletions** outside the new crate,
which is 1,781 lines. The syscall layer went 16,969 → 16,518.

Dependency edges added (`cargo tree`, not `use` statements): three, all into a
leaf with no dependencies of its own — `akuma` (bin), `akuma-exec`,
`akuma-time`. `akuma-time` was the crate that had to invent `LocalTimespec`;
it now has none of its own ABI structs.

## 5. What the move found

Four things, each a decision:

**(a) `struct sysinfo` was not a struct.** `sys_sysinfo` built a `[u8; 112]`
with five `core::ptr::write(ptr.add(N))` calls under a comment listing the
AArch64 offsets. The comment was correct; nothing checked that it stayed
correct, and the struct existed only as prose. It is a real `repr(C)` type now
with every offset asserted — including the 4-byte hole after `procs`/`pad` that
puts `totalhigh` at 88 rather than 84, which is the entire content of the
"aarch64" in "aarch64 `struct sysinfo`".

> **And converting it introduced a bug, which is why it is worth writing down.**
> The old `[u8; 112]` was **zeroed**; `write_user_val` copies `size_of::<T>()`
> bytes straight out of the value, padding included, and a `repr(C)` struct's
> tail padding is not initialised by `#[derive(Default)]`. The first version of
> `Sysinfo` therefore handed userspace four bytes of kernel stack on every
> `sysinfo(2)` — an info leak and a nondeterminism, invisible to every tier of
> the gate because nothing reads those bytes. The fix is a named `_f: [u8; 4]`
> field so `Default` zeroes them, plus
> `defaulted_sysinfo_has_no_uninitialised_bytes`, which asserts all 112 bytes
> are zero. **Any future "replace a zeroed byte buffer with a struct" move needs
> the same test**; this is the only struct in the crate with tail padding whose
> old form was a zeroed array (`MsgHdr` also has four tail padding bytes, but it
> was already a `repr(C)` struct written the same way, so its behaviour is
> unchanged).

**(b) The signed/unsigned split in `timespec`/`timeval` was a real divergence,
not a typo.** Linux's are `{ time_t; long }` — both signed — so the `i64`
spelling was the correct one and is what the crate uses. But the `u64` copies'
callers do *unsigned* saturating arithmetic on the fields, which differs from
the signed version for any value with the top bit set. **Decision: preserve the
behaviour, expose the cast.** Those sites (`timerfd`'s `timespec_to_us_safe`,
`akuma-time`'s sleep / `clock_settime` / `setitimer` paths) keep doing unsigned
arithmetic, now through an explicit `Timespec::bits()` / `from_bits()` pair
whose doc comment says "this path treats a negative `tv_sec` as an enormous
positive one". The cast is visible at the site instead of hidden in a private
struct definition. Changing it is a separate, deliberate change with its own
verification — not something to smuggle into a refactor.

**(c) `struct stat` had no assertions at all.** `Statx` had thirteen
`offset_of!` checks (from `UNSAFE_AUDIT.md` §4 P1); `Stat` — the buffer every
`ls`, every `apk` and every `cargo` stat call reads — had none. It has nine now.

**(d) The `#[cfg]`s split three ways, and only one kind was removable.**
The gates on the *constants* (`nr::REBOOT` behind `sc-reboot`, `EPOLLET` behind
`sc-epoll`) are gone: a syscall number and a bit value are facts about Linux,
not about which features a build compiles in, and gating them only meant that
turning `sc-timerfd` off erased the knowledge that 85 means `timerfd_create`.
The gates on the *dispatch arms* stay, because that is where the feature
actually decides something. And the gates on the *imports* had to stay too —
`unused_imports` is `deny` in this workspace, so a name nothing in a given
build mentions is a hard error. That last one is why `EPOLLRDHUP`,
`EpollEvent`, `IfConfHdr`, `SockAddrHw` and `Timeval` carry `#[cfg]` on their
`use` lines in `src/syscall/{mod,poll}.rs`: it is a lint constraint, not an ABI
claim.

## 6. Verification

Per [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md).

**Tier 1 — host only.** All four clippy configurations (release, extreme-size,
devbox-smoltcp, devbox-rump) end in `Finished` with **0 warnings and 0 errors**.
Host tests **824 → 858**: +34, exactly the new crate's own tests, 0 failed,
nothing removed.

Three of the four clippy configurations caught something the default one did
not — the reason the runbook insists on running all four. `extreme-size`
(no `sc-epoll`) and `devbox-rump` (no `smoltcp`) between them found every one
of the import gates in §5(d). A single `cargo clippy --release` was green
throughout.

**Tier 2/3 — boot suite and live paths.** See §6.1 below.

**Tier 4/5 — not run,** and deliberately: this change touches no allocator, no
fault path, no page-table walk. It is `repr(C)` field offsets and `const`
values. Tier 3 is the tier that can see it fail, because Tier 3 is where a
userspace binary reads a `struct stat` the kernel wrote.

### 6.1 Tier 2 / Tier 3 results

`scripts/verify_trim.py --tier all` on `869928e6`, 2026-08-28, 257 s:

| | SMP=1 | SMP=4 |
|---|---|---|
| booted | True | True |
| `[PASS]` | **99** | **99** |
| failure set | **empty** | **empty** |
| `passed_marker` | 305 | 313 |
| `host_timejumps` | **0** | **0** |
| `bkl_stuck` | 0 | 114 |
| exercises | 16/16 as expected | 15/16 as expected |

Host arm: all four clippy configurations clean, **858 tests / 0 failed**.

Two entries need reading rather than counting:

- **`smp4.ex.cowstale: UNEXPECTED` / `Segmentation fault`.** In the gate's
  known-benign register, and not a finding here. `cowstale` is the stale-write-fault
  class: a write fault judged against state a sibling's CoW break already consumed.
  It has been A/B'd on both arms twice (2026-08-14 at SMP=2, 2026-08-19 at SMP=1)
  and failed on the unmodified tree both times. It also **cannot plausibly be this
  change** — `cowstale` is a fork/CoW probe that reads no `repr(C)` struct this
  crate moved, and it passed at SMP=1 in the same run.
- **`smp4.bkl_stuck: 114`.** Load-driven — see the `[BKL] stuck tag=511` row of
  the gate's known-benign table
  ([`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)).
  A real storm is thousands of lines. `host_timejumps: 0` says the host was not
  starving QEMU, so this is guest-side contention at 4 cores, which is its normal
  shape.

**Tier 3 evidence specific to this change.** The gate's own exercises are
fork/CoW/fault-path probes and **none of them reads a struct whose offsets this
crate moved** — so a green gate, on its own, would not have been evidence for the
thing most likely to break. Four round-trips were run by hand on the same boot
(SMP=1, `MEMORY=2048`), each chosen because a userspace binary parses a structure
the kernel wrote:

| Probe | Type exercised | Result |
|---|---|---|
| `busybox stat /bin/busybox` | `Stat` | size 1116408, blocks 2181, mode `0755`, links 1, all three timestamps `2026-08-26 18:15:11` — every field at the right offset |
| `busybox df` | `Statfs` | 2097152 1K-blocks, 82 % used — `f_blocks`/`f_bfree`/`f_bavail`/`f_bsize` consistent |
| `busybox free` | `Sysinfo` | `total 2097152` KB = exactly the `MEMORY=2048` the VM booted with. **This is the one that matters**: `Sysinfo` is the struct whose conversion from a zeroed `[u8; 112]` introduced the 4-byte info leak in §5 |
| `nettest-connect ifconfig` | `IfConfHdr`, `SockAddrHw` | `checks=29 failures=0`, including `ifc_len is a multiple of sizeof(ifreq)=40` — a direct packing assertion |

Also run, as the broader ABI check: `nettest-unix all` → 8 `OK` + 2
`UNSUPPORTED` (`passfd`, `syslog`; both are known gaps, not regressions),
exercising `MsgHdr`, `IoVec` and `Ucred` — three more types this crate moved.
And `ext2probe 25 4` → `NO REGRESSION`.

**Tier 4 / Tier 5 not run, deliberately** — see §6. **The baseline arm
(`64de70c8`) is not in this table**: a worktree was prepared for it, but the A/B
was deferred rather than run on a contended host, since a starved run's
`cowstale`/`bkl_stuck` readings are exactly the ones that mislead. The arm above
was captured on a quiet host (`host_timejumps: 0` at both levels), so it stands
on its own for everything except a same-day comparison of the two flaky entries.

## 7. `akuma-syscalls` — the shape crate

**Built and verified, 2026-08-28.** This section used to say "what was not
built"; it is now the record of what was, what it decided, and the one thing it
found that nobody was looking for.

The sequencing held: crate 1 was additive, crate 2 touches `handle_syscall`, and
they landed separately.

### 7.1 What it is

`crates/akuma-syscalls` — the generic part of a syscall excursion, with the
effects left in the kernel. Its only dependency is crate 1, because it
classifies syscall *numbers* and needs nothing else.

| module | holds |
|---|---|
| `lib` | `HookConfig`, `Excursion` → `ProloguePlan` / `EpiloguePlan`, `Counter` + `counter_for`, `clears_signal_state`, `debug_io_suppressed`, `IdentitySource` |
| `slot` | the process-table slot lifecycle as an enumerable model — `claim / retire / reclaim / stamp / validate`, three validation schemes, two epilogue policies, and an exhaustive search |
| `tests` | 17 tests: the differential oracle, the gate matrix, and the six enumeration verdicts |

`handle_syscall` now reads as the state machine plus injected effects: it builds
one `Excursion`, reads plan fields where it used to spell conditions inline, and
performs every effect itself. **§7's original ban still holds and is worth
restating: the family implementations are not in it, and must not be.**

### 7.2 The shape: decisions, not injected effects

The template is `akuma-net-yarn`, and the load-bearing thing about that template
is what it does *not* do. There is no `trait Effects`, no generic parameter, no
`dyn`. The caller performs the effects and calls pure methods between them.

That is not style. It is the only shape that survives the hot path, which was
risk 1 below. Everything public in the crate is a plain-data struct or a
`const fn` returning a C-like enum, so it inlines into the caller and compiles
back into the branches it replaced. The 22-arm counter `match` became
`counter_for(nr)` (host-tested) feeding a `match` on the enum — two matches
with no indirection between them, which LLVM fuses. That was checked by
measurement, not assumed.

### 7.3 Risk 1: the dispatch is the hot path

Held — and then some. Four arms, each a **separate real build measured alone**
with no peer VM, `read_syscall_cost … 2000 5` (100 passes × 100 calls, take the
minimum), boot suite green on every one:

| arm | what it is |
|---|---|
| **A** | `1dd2def6` — before crate 2 |
| **B** | `052d581d` — crate 2 + the Finding A fix |
| **C** | + the leaf fast path + `akuma_get_version` on the BKL opt-out list |
| **D** | C with the BKL opt-out entry removed, to split C's win |

`best` column, `SMP=4` (the low-variance environment — see below):

| arm | A | B | C | D |
|---|---:|---:|---:|---:|
| `getpid` | 130 | 130 | 130 | 130 |
| `getppid` | 130 | 130 | 130 | — |
| `ENOSYS` | 120 | 130 | 130 | — |
| `akuma_get_version` | 130¹ | 130 | **90** | 100 |
| `uname` | **170** | **140** | 140 | 130 |

¹ Not implemented on A, so that cell is its ENOSYS path — which measures the
same, as every floor row does.

Four readings:

1. **Crate 2 is not a regression.** `getpid` 130 → 130. The extraction *and*
   the Finding A fix — which adds an identity-cache read to every epilogue —
   cost nothing measurable.
2. **The `uname` static image is worth 30 ns** (170 → 140, 18 %), landed in B.
   See §7.6 B.
3. **The leaf fast path is worth 30 ns** (130 → 100, arm D against arm B).
4. **The BKL enter/leave pair is worth ≤10 ns** (100 → 90, arm C against D).
   That is exactly one counter tick, so it is *bounded*, not priced — method
   warning #3 in the audit.

`SMP=1` is noisier and agrees: `version` 210 → 160 for the fast path, `uname`
290 → 250 → 240.

**`SMP=4` is the better measurement environment, which is worth writing down.**
Every floor arm reads 130 ns there against 180-230 at `SMP=1`, and the six
samples per arm are frequently identical. The probe gets a core to itself
instead of interleaving with netpoll, timer and reclaim work on the only core.
The audit's rig says `SMP=1`; for floor measurements that is the wrong default.

The `read-profile` `wrap` span — the wrapper layer *outside* `handle_syscall`,
which this work cannot touch — is the control, and it reads 167 ns, reproducing
the audit's figure exactly. Full rig and every raw number:
`logs/crate2/BASELINE.md`, `logs/crate2/sweep3.txt`, `logs/crate2/legD.txt`.

**A build-identity trap caught mid-flight, recorded because it nearly landed.**
An earlier version of this sweep measured "after" against "before" and found a
60 ns regression. The "after" binary had been built `--features no-tests`
several steps earlier while debugging a dead-code error: 2050 KB against the
"before" arm's 3386 KB. Two different kernels. The audit's method warning #4
says to verify the build before believing a boot; the sweep now prints
`[mkbin] kernel size` and the boot-suite `PASSED`/`FAILED` counts next to every
arm's numbers, so a mismatched build is visible in the results table itself
rather than needing to be remembered.

### 7.4 Risk 2: a shape crate can pass its own tests and still be wrong

Addressed the way `akuma-net-yarn` addressed it. `tests::reference` is the
prologue/epilogue decision logic of `handle_syscall` **as it shipped at
`1dd2def6`**, transcribed rather than re-derived: the long chain of `!=`
comparisons, the 22-arm `match` named after the `inc_*` each arm called, the
`track_time` / `need_timing` / `logging` locals in the original's order. Its doc
comment says do not tidy it, for the same reason yarn's does — a tidied oracle
proves the model agrees with a tidied oracle.

The differential runs the whole ABI range plus the bands above it (512, 600,
1024, 4095, `u64::MAX`) against all 16 gate combinations. Two of those gate
combinations are otherwise only reachable by building a different kernel:
`PROCESS_SYSCALL_STATS` and `PROC_SYSCALL_LOG_ENABLED` are `true` in every
profile but `kernel_profile_extreme`.

### 7.5 The payoff: the identity-cache questions are decidable, and were decided

This is what §7 promised and it delivered more sharply than expected.
`IDENTITY_CACHE_SMP_REVIEW.md` records two use-after-free findings, both by
inspection, both with the same failure mode — a silent write into a reallocated
block — and neither reproducible on demand. Finding A read `epi_stale=0` through
a full SMP=4 thread-churn soak, which the doc was careful to call "rare, not
cleared".

`slot::search` enumerates every interleaving of `claim / retire / reclaim` over
2 slots to depth 6 and checks the epilogue's write at every prefix. Six
verdicts, each a test:

| question | verdict |
|---|---|
| Finding A — epilogue writes through the prologue's pointer | **witness at depth 2**: `Retire(0)`, `Reclaim(0)` |
| …epilogue re-reads the cache instead | no witness |
| Finding B — `ACTIVE`-only validation | **witness at depth 3**, and it is the *wrong-occupant* kind |
| …`Validation::Generation` (what shipped) | no witness |
| …`Validation::PointerOnly` | **witness** — address reuse, mechanising the doc's argument |
| …`Validation::PointerAndPid` | no witness — sound, and costlier |

The depth-2 witness is exactly `kill_thread_group` retiring a sibling that is
still inside a blocking syscall, followed by any idle core's reclaim drain. It
took under a millisecond to find what a soak could not.

**So Finding A is fixed here rather than deferred.** `EPILOGUE_IDENTITY` in
`src/syscall/mod.rs` is `IdentitySource::Reresolve`: the epilogue reads the
identity cache again after the dispatch and skips its `Process` writes on a
miss, restoring exactly what the pre-cache epilogue's `lookup_process_shared`
returning `None` did — at one validated cache read instead of the lock + map
walk + IRQ-masked table scan that guard used to cost twice. `owner_pid` stays
the prologue's scalar copy, so a process that retires mid-call still files its
last log entry under the pid it had.

`src/process_tests.rs::test_epilogue_identity_revalidated_after_dispatch` drives
the witness against the real table: register a leader and a `CLONE_THREAD`
sibling, resolve, retire the leader **only** (no reclaim, no reissue — the
window opens at the first of the two ops), and assert the cache refuses, that it
refused on the state arm rather than by never having resolved, and that
`EPILOGUE_IDENTITY` is still `Reresolve`. That last assertion is the point: the
defect's whole nature is that nothing observes it, so flipping the const back
must fail a test rather than silently reopen a use-after-free.

**Keep the two instruments apart. Enumeration answers "can it?", the soak
answers "does it?", and the second is not a substitute for the first.** Nothing
here models memory ordering, the BKL, or how often a window is reached — a
witness at depth 2 says nothing about how often depth 2 occurs, which is exactly
why Finding A could read `epi_stale=0` and still be a defect.

### 7.6 What this work found that nobody was looking for

**A. `getpid` was the right floor by luck, and now there is one by
construction.** Every floor number in
[`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md) is a
`getpid`, whose arm *looks* like it resolves a process. It doesn't cost
anything: the identity cache that audit added means `sys_getpid` reads a value
the prologue already warmed. True, and never checked.

`akuma_get_version` (Akuma-private **328**, `src/syscall/version.rs`) is the
control that checks it: no arguments read, no user memory touched, nothing
resolved — a compile-time constant into `x0`. It measures the same as `getpid`,
within the counter's resolution. So the ~190 ns floor is the boundary — EL0
round trip, `wrap`, and `handle_syscall`'s prologue/epilogue — and nothing else.

It is not only a probe: a packed `[major, minor, patch, commit]` in one register
is what a compatibility check wants, and the top byte is reserved zero so no
libc wrapper can read the value as a negative errno.

**B. `uname(2)` was rebuilding a constant.** It reports the same version and git
SHA from the same `env!`s and computes nothing, but it assembled the 390-byte
`utsname` on the stack every call — a 390-byte memset plus six
`copy_from_slice`s — and *then* copied it out. ~780 bytes moved and a 390-byte
stack frame to deliver ~30 bytes of compile-time text. It is now a `static` in
`.rodata` and one `copy_to_user`, which is what Linux does (it copies straight
out of `init_uts_ns.name`). Measured after: one tick above the floor.

The cross-kernel column is the useful one. Same binary both sides, Linux in
Lima under Apple `vz`:

| | Akuma | Linux |
|---|---:|---:|
| `getpid` | ~190 | 136 |
| `uname` | ~220 | 154 |
| **`uname`/`getpid`** | **1.16×** | **1.13×** |

The 390-byte copy costs ~25 ns on Akuma and ~18 ns on Linux. **`copy_to_user`
is not where Akuma loses**; the whole gap is the fixed boundary. And that gap is
now 1.4×, on a host that is not idle, against the 3.0× this document's sibling
audit opened with.

**C. A tenth method warning, which cost a false finding.** `JITband` (4095) is
not a syscall: `src/exceptions.rs` answers anything above 500 with `ic iallu` +
an instruction replay, and on QEMU `ic iallu` calls `tb_flush()`. The arm ran
fourth of six, so every arm after it measured a cold machine.
`akuma_get_version` — an arm that returns a constant — read **290-410 ns**
against `getpid`'s 160-200, consistently, across three runs, with a plausible
mechanism ready to explain it ("the Akuma-private 300+ numbers sit deep in the
dispatch tree"). Moving the arm to the end collapsed the gap to zero.

> **Anything that flushes global state belongs at the end of a measurement, not
> in the middle of one — and three consistent runs of a wrong number are still a
> wrong number.** Repeatability is not evidence when every run shares the same
> systematic contaminant.

**D. The probe's `ENOSYS` arm is Akuma-only.** Number 107 is `timer_create`,
which Linux *implements* — it reads 390 ns there against a 136 ns `getpid`,
which looks like a catastrophic ENOSYS path and is nothing of the kind. The arm
now says so in the source. `getpid` / `getppid` / `uname` are the portable rows.

**E. The handoff's open question was not one.** It flagged "`wrap` is 167 or
~120 ns — the two docs disagree" as live. They do not: 167 ns is the `wrap` span
on a `read-profile` build, and ~120 ns is the audit's `F1f` row — a whole
plain-kernel `getpid` with the debug flags off. Different quantities, never in
conflict. Checking whether an inherited uncertainty is real is cheap; inheriting
it is not.

### 7.7 The leaf fast path

Two facts about a syscall number, deliberately two predicates rather than one
flag, because they cross:

- `takes_no_args(nr)` — the arm reads no element of `args`.
- `needs_identity(nr)` — anything in the excursion needs to know who is calling.

`getpid` takes no arguments and is *entirely* about identity; `read` needs both.
Only the corner where both are false, `FastPath::Leaf`, can skip the generic
work. A leaf skips the identity resolve, both `Process` syscall stamps, the
per-process stats, the `/proc/<pid>/syscalls` entry, the clock reads that feed
them, and the epilogue's re-resolve. It does **not** skip `CURRENT_SYSCALL_NR` /
`set_thread_current_syscall` (a crash dump reads those to say which syscall a
thread was in) or the `syscall_counters` bump (the totals would stop adding up).

**Admission needs all four, checked against the arm rather than assumed:** reads
no argument; touches no `Process`, process table, fd table or address space;
cannot block; and losing its `/proc/<pid>/syscalls` rows is acceptable — because
that is a real observable change and it is the price.

Exactly two numbers qualify today (`akuma_get_version`, `uptime`), and that is
criterion 2 being strict, not effort being short. `sched_yield` takes no
arguments and is excluded: it reaches the scheduler, which is about the current
thread.

**`takes_no_args` on its own currently buys nothing**, and saying so is more
useful than implying otherwise: `handle_syscall` does no generic argument
validation — validation is per-arm, inside `sys_*` — so there is no check here
for it to skip. It is carried because it is half of `Leaf`'s definition, and
because it is the precondition for the one real saving not yet taken: the entry
vector saves and restores ~34 GPRs on every trap
([`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)
§ "Other untested surface" item 2), and a call that reads no arguments does not
need `x0`-`x5` restored. That is an assembly change and it needs this
classification first.

`akuma_get_version` also joined the Phase 7f BKL opt-out seed — the easiest
entry that list will ever get, since the arm returns a `const` and there is no
shared state for the BKL to protect.

**Worth 30 ns and ≤10 ns respectively (§7.3).** And the tier pays for itself
beyond that: because `akuma_get_version` is the floor control, the gap between
it and `getpid` is now a **live, permanent measurement of what the prologue and
epilogue cost** — the audit's ablation ladder as an instrument rather than a
one-off build.

The differential oracle flagged this as a divergence the moment it was added,
which is what it is for. It is not excluded from the comparison: the leaf test
pins **both** halves — which fields the fast path may change, and which it must
not (signal-state clear, counter bucket, debug-IO gate, EFAULT diagnostic).
Getting the second half wrong is how a "harmless" fast path quietly drops a
signal-state clear.

`test_akuma_get_version` **counts** rather than asserting: the suite runs on a
kernel thread with no identity, so every resolution attempt misses and bumps
`IDENTITY_FALLBACKS`, which turns that counter into a direct probe for "how many
times did this excursion ask who am I?".

That test failed the first time it ran, and it was right to. The claim it was
written against — "a leaf resolves no identity at all" — was too strong, because
`is_current_interrupted()` runs between the gated prologue and the gated
epilogue and is **deliberately not** in the fast path: it decides whether a
killed process keeps executing syscalls, and relocating a signal check to save
two loads is not a trade to make casually. On a miss it costs exactly two
resolutions, and that number is a mechanism rather than an observation fitted to
the failure:

```
is_current_interrupted -> current_process_shared
    -> current_thread_own_process                      = miss 1
    -> lookup_process_shared(current_pid()?)
         -> current_pid -> current_thread_own_process   = miss 2
```

So the test pins the leaf at 2 and requires the `getpid` control to be strictly
higher. The control is load-bearing: without it a fast path that had silently
stopped being taken would still satisfy a bare upper bound if the counter simply
stopped moving for every syscall. And pinning 2 is the point — if the interrupted
check ever joins the fast path, or a new identity consumer creeps into the
prologue, this trips and somebody decides on purpose.

The 30 ns in §7.3 was measured **with** that check still running, so it is a
floor on the tier's value, not a ceiling.

### 7.8 What is still open

- **The entry-vector GPR fast path** described above. `takes_no_args` exists for
  it; nothing has tried it.
- **More `Leaf` members.** The tier is worth 30 ns to whoever qualifies, and the
  bottleneck is criterion 2, so the candidates are calls that answer from a
  global rather than from a process — not the `get*id` family.
- **`is_current_interrupted` is the last identity consumer in a leaf
  excursion**, and it costs two resolutions (the chain above). Whether a leaf
  can skip it is a *signal-delivery* question, not a performance one — the
  deferred-kill handling at the EL1→EL0 boundary may already cover it, and
  "may" is not an argument. It needs its own analysis before anyone touches it.
- `sys_uname` still calls `validate_user_ptr` and then `copy_to_user` over the
  same range, validating twice. Unmeasured, and probably inside the tick floor.
- `src/syscall/utils/read_profile.rs` has a pre-existing
  `clippy::manual_is_multiple_of` error under `--features read-profile`. Not
  reached by the pre-commit hook, which does not lint that feature set.
- **The audit's headline is stale in a good way.** It opens with Akuma at
  **3.0×** Linux on a bare `getpid` (440 vs 147 ns). Measured today with the
  same probe binary on both guests — Linux being Ubuntu in Lima under Apple
  `vz`, 4 vCPUs, so `SMP=4` is the like-for-like row:

  | | Akuma `SMP=4` | Akuma `SMP=1` | Linux (4 vCPU) |
  |---|---:|---:|---:|
  | `getpid` | **130** | 190 | **136** |
  | `uname` | 140 | 240 | 154 |
  | leaf (`akuma_get_version`) | **90** | 160 | — |

  Parity at `SMP=4`, 1.4× at `SMP=1`, and a leaf syscall *below* Linux's floor.
  The `uname`/`getpid` ratio is the honest cross-kernel number for the user-copy
  path — 1.08× on Akuma against 1.13× on Linux — which says `copy_to_user` is
  not where Akuma loses; the whole remaining gap is the fixed boundary at
  `SMP=1`. That document should be updated rather than left to read as current:
  its analysis stands, its headline does not.
## 8. Family implementations

Unchanged from the proposal: keep extracting families **opportunistically**, on
the `akuma-time` model (953 lines: a whole family plus its SNTP client), when a
family has real pure logic worth testing — never as a batch move of 16k lines.
Now that ABI marshalling has moved out, what is left in `fs.rs` / `net.rs` is
thinner glue over `akuma-vfs` / `akuma-ext2` / `akuma-net`, which are already
crates.

## Background

- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.7 — the errno extraction, and the "the table lived somewhere the other
  caller could not reach" argument this work reused.
- [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md) —
  the 150 ns floor the dispatch must keep, and deferred items 2–4.
- [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md) — the open
  interleaving questions crate 2 would make decidable.
- [`AKUMA_SCHEDULING_EXTRACTION.md`](AKUMA_SCHEDULING_EXTRACTION.md),
  [`PMM_EXTRACT.md`](PMM_EXTRACT.md) — the two previous extractions this one
  follows.
- `crates/akuma-boot/src/lib.rs` — the ABI-decode template crate 1 followed.
  `crates/akuma-net-yarn/` — the shape template crate 2 would follow.
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  — the no-regression gate §6 reports.
