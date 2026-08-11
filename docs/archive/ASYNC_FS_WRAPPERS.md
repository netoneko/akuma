# Async filesystem wrappers

> **REMOVED 2026-08-10 (branch `trim-fat-sshd`).** `src/async_fs.rs` was
> deleted along with the built-in SSH server and in-kernel shell — its only
> callers (`docs/archive/BUILTIN_SSH_REMOVAL.md`). This document is kept
> verbatim below as the historical record of how it worked; it no longer
> describes anything in `src/`. See
> [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) for what replaced it.

Async-friendly wrappers around the synchronous VFS API. Source:
`src/async_fs.rs`. For the underlying `with_fs` critical section these
wrappers avoid violating, see [`vfs.md`](vfs.md) "Mount table" — read that
first.

> **Stability: A (stable).** Substance dormant since Feb 2026 (the OOM fix);
> the only touches since (`remove dead code` Mar 5, `fix clippy` Jun 19) were
> repo-wide mechanical passes, not functional changes to this file.

## What it adds over the sync API

`vfs.md` documents `with_fs` as the VFS critical section: it disables
preemption before taking the spinlock, and the invariant is **never
`yield_now()` or do slow I/O inside it**. An `async fn` that called `fs::*`
directly and then hit an `.await` point would risk yielding — or being
preempted at a point that behaves like yielding — while still notionally
"inside" that critical section from the caller's perspective, and every
`fs::*` call underneath does its own independent `with_fs`.

`async_fs.rs` doesn't change that contract; it wraps each synchronous call
so the **yield happens before** the sync call, not during or after it:

```
pub async fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    yield_now().await;
    fs::read_file(path)
}
```

(`:66-69`, and identically for `list_dir`, `read_to_string`, `write_file`,
`append_file`, `create_dir`, `remove_file`, `remove_dir`, `rename`, `exists`,
`stats` — `:60-123`.) The module doc comment (`:1-11`) spells out why no
additional locking is layered on: the VFS already has its own spinlocks,
preemption is disabled during async polls so there's no concurrent access to
race against, and using async mutexes with the kernel's no-op-waker
`block_on` would deadlock.

## `YieldOnce`

`YieldOnce` (`:26-48`) is a one-shot future: the first `poll()` calls
`cx.waker().wake_by_ref()` and returns `Poll::Pending`; the second `poll()`
returns `Poll::Ready(())`. `yield_now()` (`:51-53`) is just
`YieldOnce::new().await`. This is the same "yield once, let the executor come
back around" pattern used elsewhere in the kernel's cooperative async
scheduling — it's a scheduling courtesy, not a synchronization primitive.

## Net effect

Callers (the shell's async command paths, the SSH server) get an `async fn`
surface that plays nicely with the executor between VFS calls, while every
individual filesystem operation still runs as a single synchronous,
non-yielding `with_fs` critical section underneath — satisfying the
constraint `vfs.md` documents rather than working around it.

## Background

- `docs/archive/KERNEL_OOM_ALLOCATION_FIX.md` (Feb 2026) — at the time, this
  file also exposed a stateful `AsyncFile` handle whose `read`/`write` were
  rewritten from whole-file `fs::read_file`/`fs::write_file` to bounded
  `fs::read_at`/`fs::write_at`, to stop large log files from panicking the
  physical allocator. `AsyncFile` itself was unused and deleted as dead code
  three weeks later (`7b03a7f`, Mar 5 2026) — the free-function wrappers
  documented above (which still call whole-file `fs::read_file` /
  `fs::write_file`) are what remain, so the bounded-I/O fix from this doc no
  longer applies to anything in `async_fs.rs` itself.
