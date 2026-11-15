# ssh sessions lost stderr, and an old sshd looked like "no prompt" (2026-11-13)

## Symptom

On the ryzen box, an interactive `ssh` login printed the banner and then **no prompt**. A
non-interactive `ssh host 'ls /nonexistent'` printed nothing: the error message never arrived.
`ssh host 'echo OUT; echo ERR >&2'` showed `OUT` only. `busybox tty` said `not a tty`, and nothing
the client typed was echoed.

## Cause

Two separate things stacked, and the first was a deployment fact, not a code bug:

1. **The box's `/bin/sshd` was built 2026-11-10.** It predates
   [`SSH_SESSIONS_HAD_NO_PTY.md`](SSH_SESSIONS_HAD_NO_PTY.md) (2026-11-12), so its `linux-abi`
   spawn handed every session shell two pipes and no terminal. busybox ash writes its prompt to
   **stderr**, and the same build never pointed the child's fd 2 anywhere: the child kept the
   spawner's own fd 2, which on Akuma is the host console. The prompt went to the TV, not the
   client.
2. **`main` still had half of that bug.** After the pty fix, the *pipe* fallback of
   `libakuma::spawn_full` (an `exec` channel without `-t`, or a kernel with no `/dev/ptmx` such
   as Taoxichuan) did `dup3` onto fd 1 and fd 0 only. fd 2 stayed the spawner's. So
   `ssh host 'cmd'` lost every diagnostic the command wrote to stderr, on every kernel that
   reaches that path. The pty path was unaffected: the slave is on 0, 1 and 2.

How it was told apart from a kernel regression: `ls -l /proc/<shell>/fd` showed fd 0 and fd 1 as
`pipe:[N]` and fd 2 as `/dev/stderr`, plus an inherited `socket:[N]` at fd 4. A kernel-spawned
child has no inherited socket; this one was `fork` + `execve`, so the fault was in the spawner's
dup list, not in the kernel's `Stderr` write path.

## Fix

- `userspace/libakuma/src/lib.rs`, `spawn_full` (`linux-abi`, pipe path): `dup3(out_write, 2)`
  beside the existing `dup3(out_write, 1)`. stderr shares the stdout pipe, which is what a pty
  gives and the one stream sshd carries per session.
- **The build stamp could not be trusted, and nothing could be asked of a running sshd.**
  `libakuma/build.rs` reran only when `.git/HEAD` or the index moved, so `libakuma::GIT_REV`
  (the `rev` in sshd's and herd's startup line) could name an older commit after a `reset
  --soft`, an empty commit, or in a worktree. It now tracks the reflog, the branch ref,
  `packed-refs` and the common dir, as the kernel's `akuma-syscalls-glue/build.rs` has since
  2026-10-17. And `sshd --version` prints `sshd rev <sha>[-dirty]` and exits before it loads
  anything or binds, so a stale sshd on a box is one command to find.
- The box got an sshd built from `main` (`/bin/sshd`, previous binary kept as
  `/bin/sshd.pre-pty-fix`), restarted through herd's supervision.

## Verified

On the metal, ryzen entry 30 (the `kvmctl kvmmem=4096` herd entry):

| check | before | after |
|---|---|---|
| `ssh host 'echo ERR >&2; ls /nonexistent'` | stdout only | `ERR`, `ls: /nonexistent: No such file or directory` |
| `ssh -tt host 'tty; echo ERR2 >&2'` | `not a tty` | `/dev/pts/0`, `ERR2` |
| interactive login | banner, no prompt, no echo | prompt, echo, `tty` = `/dev/pts/0` |

## Still open

- `uname` on a box names the kernel revision only; sshd, herd and every other userspace binary
  carry their own (`<binary> --version` for sshd; herd prints its rev at startup). The old
  2026-11-10 sshd predates `--version`, so for that one `/bin/sshd`'s build date is the tell.
- stderr and stdout are merged on the pipe path. OpenSSH keeps them apart as extended data;
  sshd here has one stream per session, so `cmd 2>/dev/null | ...` over ssh still works, but a
  client cannot tell them apart.
