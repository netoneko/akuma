# cargo→rustc spawn: `Bad address (os error 14)` under real concurrency

**Date:** 2026-08-17/18
**Status:** OPEN — root cause not yet found. One sibling bug in the same family
(`NCA_FD_NONBLOCK_TOCTOU.md`) is fixed; this one survives it.
**Symptom:** `cargo build -p nca-cli` (self-hosting `nca` inside Akuma, from
`/tmp/native-cli-ai`) fails on the first registry dependency crates:

```
error: could not compile `libc` (build script)
Caused by:
  could not execute process `rustc --crate-name build_script_build ...` (never executed)
Caused by:
  Bad address (os error 14)
```

`os error 14` is `EFAULT`.

## What's ruled out

Carried over from `docs/archive/NCA_MISSING_SYSCALLS.md` §1 (2026-08-17):

- Not argv length (single args to 64 KB, 70-arg argvs exec fine).
- Not envp size (256 KB of env vars exec fine) — and specifically not the exact ~20 vars
  cargo injects for this call (`CARGO_PKG_*`, empty-string `LD_LIBRARY_PATH`); replaying them
  exactly changes nothing.
- Not `env_clear()`, not `current_dir` (forces musl std off `posix_spawn` onto `fork`+`exec`)
  — both mimicked directly and passed 40/40.
- Not disk/registry — a minimal crate with one registry dep (`cfg-if`) builds fine.
- Not a kernel-returned `EFAULT` — with `SYSCALL_ERRNO_DIAG_ENABLED` narrowed to `EFAULT`
  only, the kernel logs **zero** `EFAULT` returns while cargo reports several. A
  deliberate-`EFAULT` canary (`write(1, 0x10, 8)`) does log, so the hook itself works. The
  kernel also logs every `execve(path=...)` — **no execve line appears for the failing rustc
  spawns at all.** So whatever produces this errno happens *before* `execve`, in the spawn
  child's pre-exec dance (`chdir`/`dup2`/`CLOEXEC`-pipe read), or is fabricated in userspace
  (a stale read off the child-error pipe).
- Racy, not deterministic: a `RUSTC=/bin/sh` shim saw ~8 probe spawns succeed before the
  first real (heavy) compile spawn failed. Retrying in a loop does not grind through it —
  80 attempts, zero progress, failed crates never cache.

Added 2026-08-18, chasing the "identical rustc argv run directly works" clue from the
original doc:

- **The exact failing rustc command, run directly (not spawned by cargo), succeeds.** Same
  toolchain, same args, same source tree, same guest, immediately after a failure — clean
  exit, real output produced. So it is not the command, the toolchain, or the source.
- **Nightly vs. stable `rustc` is not it.** `/usr/bin/rustc` (Alpine `rust-1.96.1-r0`, stable)
  and `/usr/local/bin/rustc` (nightly 1.99.0) both existed on the guest; forcing
  `RUSTC=/usr/local/bin/rustc` (absolute path, nightly, zero ambiguity) reproduced the
  identical failure. Stable was then removed entirely (`apk del rust cargo`) and the failure
  persisted unchanged with nightly as the only toolchain on the system.
- **Not the weight or shape of a single real spawn.** `ncaprobe bigspawn 50` — the exact heavy
  rustc invocation cargo uses for this crate's build script, piped stdio matching cargo's
  JSON-diagnostics capture, via plain `std::process::Command`, looped sequentially — 50/50
  clean, each taking the real ~4s a build this size costs (not skipping work), well past the
  ~8-spawn point where cargo itself starts failing.
- **Concurrency is necessary but this specific `EFAULT` shape wasn't reproduced by the
  probe that catches its sibling bug.** `ncaprobe bigspawn-threads` (4 threads × 10 rounds,
  same real invocation, genuine OS-thread concurrency) reliably reproduces a *related* but
  differently-shaped failure — `the CLOEXEC pipe failed: ... WouldBlock` (`os error 11`,
  `EAGAIN`) — root-caused and fixed as `NCA_FD_NONBLOCK_TOCTOU.md`. That fix verified clean
  (40/40) against the probe. But a real `cargo build -p nca-cli -j4` against the *fixed*
  kernel still hits the original `EFAULT` (`os error 14`) failure from crates the probe
  doesn't exercise (`libc`, others) — so the probe's 4-thread/40-spawn shape does not fully
  cover whatever real cargo does at `-j4` across its whole dependency graph.
- **Pipe-ID reuse is not it.** `pipe_create` (`src/syscall/pipe.rs`) allocates from a
  monotonic `AtomicU32` (`NEXT_PIPE_ID.fetch_add`), never reused — two concurrent pipes can
  never collide on the same `pipe_id`.
- **Fork's pipe refcounting is not it.** `clone_deep_for_fork`
  (`crates/akuma-exec/src/process/fd.rs`) correctly calls `pipe_clone_ref` for every
  inherited pipe/socket fd on both `fork_process` and `vfork_process`, so a child closing its
  copy cannot prematurely EOF the parent's — checked directly against the source, not just
  inferred.

## Working theory, unconfirmed

The `EAGAIN` sibling bug (`NCA_FD_NONBLOCK_TOCTOU.md`) proved that **per-fd-number flag
state** (not per-open-file-object) is a real hazard under concurrent fd churn from a
multi-threaded process — a late `clear_nonblock` in `sys_close` let a fd number's stale flag
leak to a new, unrelated occupant. `EFAULT` from a pipe-based child-status handshake is
consistent with the *same class* of hazard hitting **pipe buffer content** instead of a flag:
if a `read()` on what a thread believes is its own dedicated child-error pipe somehow
observes bytes belonging to a different pipe object, `std`'s handshake code would decode
whatever 4 bytes it got as a raw errno — and a wrong 4-byte value landing on 14 is exactly as
plausible as landing on 11. This has **not** been confirmed: pipe IDs don't collide and
fd-number reuse for *flags* was the confirmed vector for the sibling bug, but nothing yet
demonstrates an actual cross-pipe **data** leak. It is the natural next thing to check, not
an established mechanism.

## Next steps if resumed

- Reproduce with `ncaprobe` at higher concurrency (more than 4 threads, and/or spawning a
  wider variety of crates' build scripts concurrently, not just `proc-macro2`'s) to see if
  the `EFAULT` shape reproduces outside a full `cargo build` at all — needed to build a
  minimal repro before instrumenting further.
- If it does: same technique as the sibling bug — capture the failing spawn's exact fd
  numbers on both sides (parent and child) and check whether two concurrent pipes' read/write
  ends ever end up sharing an fd number window the way the `nonblock` bug did, this time
  looking at whether the *data* read back is byte-identical to what was actually written by
  the intended child.
- If it does not reproduce standalone: the original doc's suggestion stands — instrument
  musl std's `posix_spawn`/fork child path directly, or add one-shot kernel debug prints in
  the `dup2`/`chdir`/pipe-read paths gated on a new config flag, and diff which child-side
  syscall actually returns `-14` during a live failing `cargo build -j4` run. That needs a
  method for capturing output from inside a *child* process mid-spawn, which neither this nor
  the original investigation has built yet.

## Background

- [`NCA_MISSING_SYSCALLS.md`](NCA_MISSING_SYSCALLS.md) §1 — the original investigation this
  splits out of; kept there verbatim, this file carries the thread forward.
- [`NCA_FD_NONBLOCK_TOCTOU.md`](NCA_FD_NONBLOCK_TOCTOU.md) — the sibling bug this one is
  distinguished from, fixed and verified the same day.
- `userspace/ncaprobe` — `bigspawn`/`bigspawn-threads`, the harness this investigation's
  2026-08-18 evidence was gathered with.
