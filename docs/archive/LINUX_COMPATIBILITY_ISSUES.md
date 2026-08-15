# Linux ABI divergences — known register (opened 2026-08-14)

Places where Akuma's syscall layer answers differently from Linux. Each row is
something a real program can observe.

> **This list is a byproduct, not an audit.** Every entry below was found while
> doing something else — the §5.7 errno-table merge and the Phase 5 user-copy
> sweep, both of which read every syscall arm for a *different* reason. Nobody has
> yet gone family by family against the Linux manual pages. **A proper audit is
> still needed** and §4 says what it would have to do; until then, treat the
> absence of a syscall from this document as "not looked at", never as "matches
> Linux".

Related current-state docs: [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md)
and the 17 per-family files under `../reference/subsystems/syscalls/`. Historical
errno work: [`SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md),
[`SYSCALL_HARDENING.md`](SYSCALL_HARDENING.md).

---

## 1. Wrong errno on a bad pointer

All four share one shape: the pointer check is the **condition of an `if`** rather
than a guard, so an unreadable or unwritable user pointer makes the arm *skip* and
report success. Linux returns `EFAULT`. Found by reading every copy site during the
Phase 5 sweep ([`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §6); **none was changed**,
because changing a return value is a behaviour change and did not belong in a
deduplication pass.

| Syscall | Akuma | Linux | Where |
|---|---|---|---|
| `rt_sigaction` with an unreadable `act` | returns 0, installs nothing | `EFAULT` | `src/syscall/signal.rs` |
| `prctl(PR_SET_NAME)` with an unreadable name | returns 0, name unchanged | `EFAULT` | `src/syscall/proc.rs` |
| `prctl(PR_GET_NAME)` / `prctl(PR_GET_PDEATHSIG)` with an unwritable buffer | returns 0, writes nothing | `EFAULT` | `src/syscall/proc.rs` |
| `rt_sigtimedwait` with an unwritable `siginfo` | returns the signal number, fills nothing | `EFAULT` | `src/syscall/signal.rs` |
| `read()` on a **timerfd** with an unreadable buffer | `EINVAL` | `EFAULT` (`EINVAL` is for `count < 8`) | `src/syscall/fs.rs`, `FileDescriptor::TimerFd` arm |

Why it matters in practice: a program that *deliberately* probes with a bad pointer
(some libc feature tests, some sandbox probes) concludes the feature works. The
silent-success shape is worse than a wrong-but-failing errno.

## 2. Wrong errno, comment/value drift

| Syscall | Akuma | Linux | Where |
|---|---|---|---|
| `ioctl(TIOCSWINSZ)` and the five sibling terminal ioctls when the process has no terminal state | `-12` (`ENOMEM`) | `ENOTTY` for an ioctl on a non-terminal | `src/syscall/term.rs` |

Found by the errno merge
([`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
§5.7). The value is *consistent* across all six arms; what was wrong was the
comment on one of them, which read `// ENXIO — no terminal attached` (`ENXIO` is 6,
not 12) and had been wrong for as long as the line existed. The code now names
`ENOMEM` honestly with the divergence written beside it.

**Before changing it:** check what busybox's and musl's `isatty()` do with
`ENOMEM` — `isatty` treats any error as "not a tty", so the current value may be
harmless in the paths that matter, and the sshd-into-box bridge depends on
`TCGETS` reporting *not a tty* for a piped stdin (there is a comment at that arm
explaining why). This is the reason it was not "just fixed".

## 3. Stubs that answer plausibly instead of failing

These are deliberate — they exist because a real program crashed or bailed without
them — but they are divergences and a conformance test will find them.

| Syscall | Akuma | Linux |
|---|---|---|
| `times()` | writes a **zeroed** `struct tms`, returns `uptime_us / 10_000` | real per-process CPU accounting |
| `getrusage()` | writes 144 zero bytes, returns 0 | real usage counters |
| `capget()` | writes 24 zero bytes, returns 0 | real capability sets |
| `prctl(PR_GET_DUMPABLE)` | always 1 | the process's actual dumpable flag |
| `prctl(PR_GET_NO_NEW_PRIVS)` | always 0 | the actual flag |
| `prctl(PR_SET_PDEATHSIG / PR_SET_DUMPABLE / PR_SET_NO_NEW_PRIVS / PR_SET_VMA)` | accepted, ignored | applied |
| `getpriority` (`nr 141`) | always 20 (nice 0) | the real nice value. Returning `ENOSYS` here used to be read as a pointer by rustc's threadpool — `AKUMA_SELF_HOSTING.md` §7i |
| `clock_getres` | ignores `clock_id`, always reports 1 ns | per-clock resolution |
| `sched_getparam` (`nr 119`) | writes 0, returns 0 | real scheduling parameters |
| xattr syscalls (5–16) | `EOPNOTSUPP` | same on a filesystem without xattrs, so **this one is fine** — listed because the *encoding* is a trap: it must be `x0 = -95`, never `!95` (which is `-96`, `EPFNOSUPPORT`), and that is now pinned by a host test |

## 4. Memory-syscall semantics (pre-existing, tracked elsewhere)

| Syscall | Divergence | Status |
|---|---|---|
| `madvise(MADV_FREE)` | returns `EINVAL` | **Deliberate**, 2026-08-13 — it unblocked redis. Allocators that probe it fall back to `MADV_DONTNEED`, which is the row below. `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §8.5 Phase 0 item 4 |
| `madvise(MADV_DONTNEED)` | zeroes the **physical frame**; Linux drops the *mapping* | **OPEN and the sharper of the two**: on a CoW-after-fork or `file_page_cache` frame this also wipes a peer's live copy. Tripwire counters `DONTNEED_SHARED_FRAME` / `DONTNEED_UNALIGNED` on the 30 s `[MADV]` PSTATS line; both read 0 as of 2026-08-13 |
| `mremap` payload move | **FIXED 2026-08-14** — the destination was never validated or prefaulted, so a lazy page in the new mapping silently truncated the move. [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §5. No regression test yet |
| `write()` to a process stdin channel | short write instead of blocking | **Deliberate**, decided 2026-08-13 with the reasoning recorded (sshd's bridge must keep draining stdout to create the stdin space it would block on). §8.5 Phase 0 item 5 |

## 5. Not a compatibility bug, but it belongs next to them

A mapped **kernel** VA passed as a syscall destination passes the user-pointer
check and gets written, because kernel RAM is identity-mapped EL1-only into every
user address space, the check tests presence only, and the copy loop runs at EL1.
That is a soundness hole rather than an ABI divergence, and it is
[`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §7 — but any audit that goes through the
syscall layer with Linux's manual pages in hand will walk past it, so it is
cross-referenced here.

---

## 6. This needs an additional audit

**What is missing.** The rows above came from two passes that were looking at
*copies* and at *errno spellings*. That biases the list hard:

- it is dense on "wrong errno for a bad pointer" and near-silent on **semantics** —
  flag handling, ordering, partial results, blocking behaviour, `EINTR`/restart
  semantics, permission checks;
- it says nothing about the **success** paths, where a wrong field offset or a
  wrong return value is far more damaging than a wrong errno;
- coverage is per-*file*, not per-*syscall*: 17 syscall families have reference
  docs, and no family has been checked against its manual page end to end;
- nothing here is a **runtime** result. Every row was established by reading code.
  A few of these predictions could be wrong.

**What an audit would have to do**, in the order that gets the most out of the
least work:

1. **Pick the families that real programs already exercise here** — `fs`, `proc`,
   `signal`, `sync`, `net`, `poll`, `mem` — and work the per-family reference doc
   against `man 2` for each syscall it lists. The families are already enumerated
   in `../reference/subsystems/syscalls/`.
2. **Write the conformance probe as a userspace binary**, not as boot self-tests.
   A boot test drives `handle_syscall` directly with `BYPASS_VALIDATION` on, which
   is exactly the code path an ABI audit must *not* use — it disables the pointer
   check whose behaviour is under test. `userspace/forktest` is the precedent for
   a self-reporting probe.
3. **Calibrate against real Linux.** The trick `madvshared` already uses is worth
   copying everywhere: build the probe as a static aarch64 binary and run the
   *identical* binary under `docker run --platform linux/arm64 alpine`. Then a
   disagreement is a finding rather than an argument about what the manual page
   means.
4. **Report per-syscall, with the decision attached.** Every divergence needs one
   of three verdicts — *fix*, *deliberate with a reason*, or *deliberate because a
   real program depends on it* (the `TCGETS`-is-not-a-tty case is the third kind,
   and it would look like a bug to anyone reading only the manual page).
5. **Then convert this document into the register the audit maintains**, and add a
   symptom-matrix row pointing at it.

**Two traps for whoever does it**, both already paid for once:

- **`BYPASS_VALIDATION` is kernel-wide.** While a boot test has it on, pointer
  validation is off for every other thread on every core. Do not build an ABI
  probe on top of it ([`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) §11 item 3).
- **A comment naming an errno is not evidence.** The one confirmed drift in §2 was
  a comment that had never matched its number, and the errno merge only found it
  because it had to touch the value. Read the number, not the name.

---

## Background

- [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) — the sweep that surfaced §1, §2's
  sibling arms, §4's `mremap` fix and §5
- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §5.7 (the errno table, and the drift it exposed) and §8.5 Phase 0 (the `madvise`
  and `write_stdin` decisions)
- [`SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md),
  [`SYSCALL_HARDENING.md`](SYSCALL_HARDENING.md) — earlier errno work
- [`MUSL_COMPATIBILITY.md`](MUSL_COMPATIBILITY.md) — musl is the libc these
  divergences are observed through
- [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md) —
  dispatch, the `sc-*` gates, and where errno values and user copies come from
