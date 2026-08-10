# Missing fd-passing / cross-process socket handoff

**Stability: reference, not yet acted on.** Written 2026-08-10 while scoping
whether userspace `sshd` could hand an already-`accept()`ed client socket off
to a freshly spawned sibling process (for fault isolation — see
`userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md` and
`userspace/sshd/docs/OPTIONAL_PARALLELISM.md`). Short answer: **it can't,
today, on any existing primitive.** This doc is the survey of what exists,
what doesn't, and the three ways to close the gap, so the next person doesn't
have to re-derive it.

## What actually exists

**`/proc/<pid>/fd/<n>` resolves for any fd, but only serves data for 0/1.**
`src/vfs/proc.rs`, `ProcFilesystem`. Path parsing (`parse_fd_path`,
proc.rs:113-127) splits into `[pid, "fd", n]` with no per-type restriction;
`exists()` (proc.rs:733-739) succeeds for *any* `n` present in the target's fd
table, socket or otherwise (`p.get_fd(fd_num).is_some()`, `fd.rs:209`).

But that's just existence, not access:

- `read_at`/`read_file` (proc.rs:466-498, 633-638): `fd_num == 0` reads
  `proc.stdin`, `fd_num == 1` reads `proc.stdout`. Anything else gets only a
  synthetic description string (`fd_description`, proc.rs:148-176 — e.g.
  `"socket:[5]"`, `"pipe:[3]"`), not the underlying bytes; `read_file`'s match
  (634-638) returns `Err(NotFound)` for `fd_num > 1` outright.
- `write_file` (proc.rs:641-684): only `0` (stdin, gated to the recorded
  `spawner_pid` at 656-665) and `1` (stdout, owning process only) are handled.
  Every other fd number returns `Err(NotFound)` (line 682) — this is true even
  for the fd's *own owner*, not just other processes.
- `read_symlink` (proc.rs:963-983) explicitly refuses any `FileDescriptor`
  variant except `File` — sockets and pipes are deliberately excluded so
  `open()`/`resolve_symlinks` can't chase a fake path like `"pipe:[5]"`.

This is exactly enough to support one real feature — `userspace/sshd`'s
`bridge_process` writing to a spawned child's stdin through
`/proc/<pid>/fd/0` — and nothing more general. It's a narrow stdin-injection
hack for the spawn-parent/spawned-child relationship, not an fd-passing
primitive.

**No fd inheritance at spawn time.** `sys_spawn`'s ABI (`src/syscall/proc.rs:1409`)
is `(path_ptr, argv_ptr, envp_ptr, stdin_ptr, stdin_len, flags)` — `flags`
carries only `SPAWN_FLAG_PTY` (bit 0). `spawn_process_with_channel_ext`
(`crates/akuma-exec/src/process/spawn.rs:83-395`) wires up stdin bytes, cwd,
box/namespace inheritance, and terminal-state inheritance — no fd-table copy,
no fd-map/file-actions argument anywhere. Every spawned process starts with a
fresh fd table (implicit stdin/stdout/stderr only).
`userspace/libakuma/src/lib.rs`'s `spawn`/`spawn_pty` (lines 1373, 1388) only
expose `path`, `args`, and stdin bytes to callers — there's no fd parameter to
even ask for this at the userspace API layer.

(`CLONE_FILES`, `crates/akuma-exec/src/process/mod.rs:3168` — `fds:
parent.fds.clone()` via `Arc::clone` — shares a fd table for a same-process
`clone_thread`, i.e. pthread-style threading. Irrelevant here: it's for
threads inside one process, not for handing a fd to an independently spawned
sibling binary.)

**No `SCM_RIGHTS` / ancillary-data fd passing.** `sys_sendmsg`
(`src/syscall/net.rs:901`+, and the non-smoltcp variant at `:998`) reads
`MsgHdr.msg_control`/`msg_controllen` off the struct but never dereferences or
interprets them — only `msg_iov`/`msg_name` are used. `sys_recvmsg`
(`net.rs:1055`+) unconditionally zeroes `msg.msg_controllen` on every return
path (lines 1080, 1109, 1146, 1168) — no cmsg is ever written back. There is
no `SCM_RIGHTS` constant and no ancillary-data parser anywhere under
`src/syscall/`.

## Conclusion

"Hand an already-`accept()`ed socket fd to a freshly spawned sibling process"
is not buildable on anything that exists in this kernel today. Three ways to
add it, roughly in order of how contained the change is:

1. **Extend the `/proc/<pid>/fd/<n>` path to alias `Socket` fds.** Smallest
   surface — reuses the existing procfs fd-resolution machinery, would need a
   new `read_file`/`write_file` (or a dedicated `open`) arm that, for a
   `FileDescriptor::Socket`, installs a new fd in the *caller's* table backed
   by the same underlying socket object, plus deciding the authorization model
   (today's `spawner_pid` gate doesn't generalize past parent/child).
2. **Real `SCM_RIGHTS`.** Matches the POSIX idiom and doesn't require a
   parent/child relationship (any two processes sharing a unix socket could
   pass fds) — but touches `sendmsg`/`recvmsg`'s message-control handling and
   needs a genuine ancillary-data encode/decode path that doesn't exist at
   all right now.
3. **Add an fd-list/file-actions argument to `sys_spawn`.** Closest to
   `posix_spawn_file_actions`; only helps the spawn-time case (a fd already
   held by the parent *before* spawning), not a fd handed over after the
   child already exists — less flexible than (1) or (2) for something like
   "route an already-accepted connection to a pooled worker," but the
   simplest to reason about for a fixed spawn-then-serve pattern.

None of this is scoped or committed to; it exists to save the next
investigation from re-deriving it. See `proposals/CLEANUP.md` for where this
matters (a possible process-per-session model for userspace `sshd`) and
`userspace/sshd/docs/OPTIONAL_PARALLELISM.md` for that design's tradeoffs.
