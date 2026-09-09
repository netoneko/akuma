# One pipe table: amd64 stops keeping a second instance

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_PIPES_EXTRACTION.md` — the 2026-09-06 extraction
that made both kernels share `akuma_pipes::PipeTable`, the *rules*.
**Why now:** `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2A.md` § "What still blocks
the fold" — a folded `close(2)` cannot work while the two kernels mint pipe ids
from different tables.

## The thing that was actually duplicated

Not the logic. The buffer, the 64 KiB cap, the end reference counts, the waiter
set and every rule joining them have been one crate since the extraction, and
both kernels used it. What each kernel additionally had was **its own instance**:

| | AArch64 | amd64 (before) |
|---|---|---|
| the static | `PipeTable<WakeHandle>` in `akuma-syscalls-glue/src/pipe.rs` | `PipeTable<()>` in `amd64/src/pipe.rs` |
| the wake | `wake_by_handle(handle)` | `sched::wake(tid)` |
| the ids | its own 1, 2, 3… | its own 1, 2, 3… |

Two instances of one implementation is not a duplicate implementation, which is
exactly why it was comfortable to leave alone. What it is, is a duplicate **id
space** — and that is the part that made it a blocker rather than an untidiness.
Glue's `sys_close` closes a `FileDescriptor::PipeRead(id)` through *its* table.
Fold `close` onto a kernel whose descriptors carry the other table's ids and the
call is not an error and not a no-op: it closes **a different pipe**, silently,
while the one the descriptor named leaks its end count forever.

## What made unifying cheap

The wake effect turned out to already be the same operation. amd64's threads
**are** `akuma-threading` slots (`sched.rs` registers `X86ArchHooks`), and
`sched::wake(slot)` is `wake_by_handle(wake_handle_for_thread(slot))` plus the
`[SCHED] wakes` counter its boot suite reports. So the token type — the reason
the two statics could not have been one — was never a real divergence, only the
counter around it.

Hence the seam is one hook, not a rewrite:
`akuma_syscalls_glue::pipe::set_wake_sink(fn(usize, WakeHandle))`, registered
from `boot::install_shared_sinks` beside the print, entropy and VFS hooks — one
call site, both boot protocols, before any pipe can exist. Unregistered (every
AArch64 build) the default `wake_by_handle` runs, so that kernel's behaviour is
unchanged by construction.

`amd64/src/pipe.rs` went from a 231-line second implementation to a shim: every
function is now a name for a `glue::pipe` call. Four small accessors were added
to glue for what the shim genuinely needs — `pipe_exists`, `pipe_counts`,
`pipe_live_count`, `pipe_destroy` — plus one entry point that is a real
semantic choice, below.

## The three things that stayed amd64's, and why

- **`MAX_PIPES = 64`.** A machine-wide ceiling is a policy, not a table rule:
  each pipe is up to 64 KiB of kernel buffer claimed on a userspace request, and
  a number this small means a leak announces itself. The AArch64 kernel runs
  workloads (a `-j4` self-host build) whose pipe count this would refuse, so it
  stays in the shim, enforced through `pipe_live_count()`.
- **No `SIGPIPE`.** Glue's `pipe_write` raises it on a broken pipe, and with a
  default disposition that delivery runs the *terminate* action inline, through
  an exit path this target does not use. amd64 answers `EPIPE` alone — "the
  whole of Linux's answer that applies" on a kernel with no signal delivery — so
  the shim calls a new `pipe_write_no_sigpipe`. Spelled as a second entry point
  rather than a global policy flag: the choice is visible at the call site that
  makes it, and nothing can change it after boot.
- **Two readiness divergences**, `readable` and `writable`, which the shim now
  builds from `pipe_can_read`/`pipe_can_write`/`pipe_counts` instead of reading
  the table directly. A **gone** pipe is readable here (a read of one returns
  EOF, so a `poll` told "not ready" waits forever) and a pipe with **no readers**
  is writable here (the caller writes, gets `EPIPE`, and acts — Linux's
  `POLLERR` shape). Glue answers `false` to both for its own epoll/tokio
  callers. Expressing them in the shim keeps both kernels' `poll` rules out of
  one shared predicate.

## Verification

The pin is a self-test that only one arrangement can pass: an id minted through
`crate::pipe::alloc()` must be the pipe `akuma-syscalls-glue` sees under that
number.

**Negative control, run rather than reasoned about.** Rebuilt with `HEAD`'s
two-table module and the same ten checks grafted on: `571 passed, 2 failed` —
`pipe: glue sees the pipe this kernel just created` and `pipe: and agrees it has
one reader and one writer`. Every other check in the block — write, read, EOF
on last-writer close, destroy on last-reader close, both divergences — **passed
on the old arrangement too**, which is the whole difficulty in one line: two id
spaces are invisible to any test that only looks at one side.

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` | 563/0 | **573/0** (+10: the pipe block) |
| QEMU/TCG `SMP=4` | 573/0 | **583/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 550/0 / 560/0 | **560/0 / 570/0** |
| bare metal | 563/0 | **573/0**, pipelines included |
| host tests (`akuma-pipes` 27, workspace) | pass | **pass** |
| clippy (amd64 + AArch64 kernel) | clean | **clean** |
| ring-3 pipelines over ssh | — | **`yes \| head -n 1` terminates; 11-stage `cat` chain; `( … ) \| wc -l`** |

**An intermittent session teardown on the metal is unchanged and unexplained.**
`ssh <box> "head -c 8 /dev/zero | wc -c"` sometimes ends with `Connection
closed by remote host` — the command's output is correct every time and the
exit status propagates, but the session is torn down rather than closed.
Measured because the shape (a pipeline whose reader exits early) is exactly
what this batch touches: **4/20 before, 6/20 after, 20/20 correct output in
both**, which at n=20 is one sample of the same rate, not a change. It does not
reproduce on QEMU (0/8). Pre-existing, metal-only, and still open; recorded
here so the next person measuring it starts from two samples instead of none.

AArch64 is unverified by boot on this machine and deliberately unchanged in
substance: `fire` gains one `OnceCopy` load, `pipe_write` becomes
`write_inner(…, true)`. Both loops (HVF asserts in QEMU, TCG panics in a
pre-existing self-test) are recorded in `AKUMA_AMD64_4B_FOLD_BATCH2A.md`.

## What this unblocks

`close(2)` can now fold into glue for this target — the descriptors, the ids and
the table finally name the same things. That is the next batch, together with
`openat`, whose own remaining prerequisites (the boot suite's process identity,
the `/proc` pre-shim, the four open flags glue does not enforce) are in
`AKUMA_AMD64_4B_FOLD_BATCH2A.md`.
