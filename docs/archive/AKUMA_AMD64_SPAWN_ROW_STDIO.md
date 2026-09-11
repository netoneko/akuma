# amd64: the `Spawn` row's stdio fields — one deleted, one kept and argued

**Date:** 2026-09-11
**Status:** landed, verified on bare metal.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box.
**Follows:** `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE7.md`, whose §8 listed
this as the next item after the `execve` fold.

`Spawn::stdout_pipe` is gone. `Spawn::stdin_pipe` stays, and this document is
mostly about why — the roadmap said "the row's last two fields", and one of the
two turns out not to be removable without moving who owns a reference.

## 1. What the row was carrying

```rust
struct Spawn {
    pid: u32,
    stdin_pipe: Option<PipeId>,
    stdout_pipe: Option<PipeId>,   // deleted
    exec_slot: usize,
}
```

Four stdio fields existed originally (`stdout_pipe`, `stdin_pipe`,
`borrowed_io`, `console_io`) because a spawned child's stdio was routed **by
number**: every unbound fd 0/1/2 read or write asked the row which pipe served
it. C2 slice 6 made the child's stdio **bound descriptors** in its own
registered `SharedFdTable` (`fd::bind_stdio`) and deleted three of them.

## 2. `stdout_pipe` — deleted, because slice 6 already recorded the fact

It survived slice 6 only because it had picked up a *second, unrelated* job:

> Recorded for one reader: `child_of_stdout_pipe`, which is how a `TIOCSWINSZ`
> arriving on that descriptor finds the child whose `TerminalState` it is meant
> for.

That is the ssh window-size path. `sshd` holds the child's stdout as a plain
`PipeRead`; a `pty-req` becomes a `TIOCSWINSZ` on it, and the kernel has to
answer *which child* that pipe belongs to. On AArch64 the descriptor answers it
(`FileDescriptor::ChildStdout(pid)`), so only this target needed a link.

**But slice 6 had already recorded that link in the place that owns it.** The
child's registered fd 1 *is* a `PipeWrite(id)` — the boot suite asserts exactly
that (`spawn: fd 1 as its stdout pipe`). The row was carrying a second copy of a
fact the fd table already held, and a second copy can only drift.

`child_of_stdout_pipe` now asks the table:

```rust
akuma_exec::process::for_each_process(|p| {
    if found.is_none()
        && matches!(p.fds.table.lock().get(&1),
                    Some(FileDescriptor::PipeWrite(id)) if *id == pipe_id as u32)
    { found = Some(p.pid); }
});
```

fd **1**, not 2: `bind_stdio` binds both names to the one description, so either
answers, and 1 is the one whose meaning is "this child's stdout".

**A scan where the field was a lookup**, and that is affordable *here*
specifically: the one caller is a `TIOCSWINSZ` on a parent's stdout descriptor
— once per `pty-req`, i.e. once per ssh session. `for_each_process` runs with
IRQs disabled and its callback must not allocate; `get`ting a `Copy` descriptor
out of the table allocates nothing.

The field's own staleness argument goes with it. It read:

> The id cannot go stale under a live descriptor: a pipe is destroyed only when
> both end counts reach zero, so while the parent holds the read end this id
> names this pipe and no later spawn can be handed it.

True, and no longer something anyone has to check — the table entry and the
descriptor are the same object now.

## 3. `stdin_pipe` — kept, and here is the measurement

The roadmap said "the last two fields", so this is a deliberate stop, not an
oversight. Removing it requires changing **who owns a reference**, which is a
different change from deleting a duplicated fact.

`sys_spawn` allocates the child's stdin pipe and the spawn itself holds one
**write** reference. The child's fd 0 is the *read* end. The write end is
reached by **path** — `sshd` opens `/proc/<pid>/fd/0` — and that open *is*
refcounted (`fd::alloc_pipe_fd(_, true, false)` calls `pipe::clone_ref`), so
`sshd`'s descriptor is not the problem. The spawn's own initial reference is:
nothing holds a descriptor for it, so left to the counts alone a spawn whose
stdin nobody ever opened (every `run_sh_capture` in the boot suite) keeps one
writer forever and the pipe is never destroyed — a leak against a 64-pipe
ceiling.

The reap is the one place that knows the child is gone, so the reap drops it,
and it needs the id.

**Could it be derived instead of stored?** No, and the reason is one line in
`SharedFdTable::close_all`:

```rust
let entry = with_irqs_disabled(|| self.table.lock().pop_first());
```

`close_all` **empties** the table. It runs in `run_process`'s exit path, before
`spawn_record_exit` — so by the time the reap looks, the child's fd 0 is gone
and there is nothing to read the id out of. Deriving it the way `stdout_pipe`
is derived would have to happen while the child is alive, which is exactly what
storing it in the row does.

**What removing it would actually take**, for whoever picks this up: give the
spawn's initial write reference to a holder whose release is automatic. The
obvious candidates both fail for stated reasons — the *child's* own table would
make the child see no EOF after `sshd` closes (it would be holding its own
stdin open), and the *parent's* would tie the pipe's life to the parent rather
than the child. A third option is to change `sys_spawn`'s ABI so the parent gets
a real write descriptor back alongside the stdout one, which is a userspace
change (`sshd`) and not a kernel refactor.

So: one field left, with a reference nobody else holds, released at the one
point that knows to. The row is now `pid` + `stdin_pipe` + `exec_slot`.

## 4. Verification

The deleted field had exactly one reader, and it is not reachable from the boot
suite — it needs a real ssh client sending a `pty-req`. So the gate is an
end-to-end terminal-size check on the metal, driven from a **client pty whose
size the test sets**, because `ssh -tt` sends the *client terminal's* size and a
scripted ssh with no tty sends a default:

| set from the client | guest `busybox stty size` |
|---|---|
| 40 × 100 | **40 100** |
| 24 × 80 | **24 80** |
| 55 × 203 | **55 203** |

A wrong answer here is the documented failure of
`AMD64_SSH_TERM_SIZE_NOT_PASSED.md` break 4 — the size lands in `sshd`'s own
`TerminalState`, which nothing ever reads, and the session shell keeps the 24×80
default. Three distinct sizes rather than one, so a default cannot pass by
coincidence.

| gate | result |
|---|---|
| QEMU/TCG `SMP=4` | **641/0**, full `spawn:` block green |
| Firecracker/KVM `SMP=4` | **619/0**, full `spawn:` block green |
| bare metal `SMP=4`, `root=/dev/sda1` | **641/0**, full `spawn:` block green |
| metal ssh terminal size | **3/3 exact** (above) |
| metal ring-3, 60 sessions | **60/60 in 32.4 s**, `free` unmoved, `Slab:` +1389 KiB (tol. 8192), `ps` 6 → 5 |
| host tests | **1375** |
| clippy — aarch64 `release`, amd64 | clean |

The `spawn:` block matters more than the tally: three of its checks assert the
child's registered fd 0/1/2 *are* its pipes, which is the fact
`child_of_stdout_pipe` now reads. They were green before this change and are the
reason it is safe.

**AArch64 is untouched** — every edit is in `amd64/src/usermode.rs`. No shared
crate changed, so no binary or boot A/B was owed.

## Background

- `docs/archive/AKUMA_AMD64_C2_SLICES_6_7.md` — `bind_stdio`, which recorded the
  link this change reads.
- `docs/archive/AMD64_SSH_TERM_SIZE_NOT_PASSED.md` — break 4, the failure §4
  tests for.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE7.md` §8 — where this item was named.
