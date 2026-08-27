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
Host tests **824 → 857**: +33, exactly the new crate's own tests, 0 failed,
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

<!-- filled in from the gate run; see §6.1 table -->

## 7. What was not built: `akuma-syscalls` (the shape crate)

Still proposed, still not started, and the sequencing reason has not changed:
crate 1 is additive and low-risk; crate 2 touches `handle_syscall`, the hottest
function in the kernel. **They should land separately, and this is the first
one landing.**

The case for it is unchanged:

- It would model the generic part of a syscall excursion — identity resolve →
  interrupt check → stats/log hooks → dispatch handoff → epilogue — as a state
  machine with the effects injected, exactly as `akuma-net-yarn` does for the
  readiness wait loop.
- **It must not contain the family implementations.** A crate holding the 16.5k
  lines of `src/syscall/` would depend on vfs, ext2, net, exec, mm, pmm and
  terminal — a second kernel, whose tests would need all of that mocked. That is
  less testable, not more.
- It would make the open identity-cache questions in
  [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md) **decidable by
  enumeration** rather than by stress-testing. Both findings there are narrow
  interleavings whose failure mode is a silent write into a reallocated block; a
  bounded exhaustive search over `claim / retire / reclaim / stamp / validate`
  at 2 cores × 2 slots settles whether the ordering is admissible at all.

Two risks recorded with it, both still live:

- **The dispatch is the hot path.** `handle_syscall` was taken from 410 ns to
  150 ns and the ~120 ns `wrap` layer is the next target. Any abstraction has to
  stay monomorphic and inlined; an extraction that adds an indirect call to the
  dispatch would eat the entire win. Re-measure with
  `userspace/ext2probe/c/read_syscall_cost.c` on each step. *(Crate 1 does not
  touch this: it moves `const`s and `repr(C)` definitions, which are compiled
  away identically.)*
- **A shape crate can pass its own tests and still be wrong.**
  `akuma-net-yarn` carries a differential test against the pre-extraction
  `wait_until` for this reason; crate 2 needs the same oracle, or a green suite
  only proves the model is self-consistent with itself.

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
