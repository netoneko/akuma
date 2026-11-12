# libakuma's `linux-abi` spawn handed the spawner's descriptors to every child (2026-11-12)

**Status: fixed in `userspace/libakuma` (`spawn_full`, `linux-abi`). Found by the `fdlimit`
probe.**

## Symptom

On amd64, `fdlimit` run from an ssh session opened 1019 descriptors before `EMFILE`, where Linux
and the AArch64 kernel open 1021. `ls -l /proc/$$/fd` in the session shell showed fds 3 and 4 as
`socket:[3]` and `socket:[4]`, the same two sockets sshd (pid 1) held.

## Cause

sshd is built with libakuma's `linux-abi` feature (`userspace/sshd/Cargo.toml`). Its spawn does
not use Akuma's 301, which gives the child a fresh table. It is `pipe2(O_CLOEXEC)` twice,
`fork`, `dup3` onto 0/1, then `execve`. The pipes were close-on-exec. sshd's listening and
connection sockets were not, so they survived `execve` into every session's shell and everything
the shell ran. The AArch64 guest did not show it because its disk's sshd still spawned through
301 (its shell's fd 0 read `/dev/stdin`).

## Fix

The child calls `close_range(3, ~0U, 0)` after the `dup3`s. A kernel without `close_range`
answers `ENOSYS` and the child inherits as before.

## Verified

With sshd rebuilt into the amd64 disk (`amd64/mkdisk.sh`), `fdlimit` from an ssh session opens
1021 (Linux's count) and `ls -l /proc/self/fd` shows only 0/1/2 and its own directory fd.

## Still open

- sshd's sockets are still not `SOCK_CLOEXEC`, so a fork+exec that sshd makes some other way
  would leak them again.
- This path ignores the pty flag, so an amd64 ssh session is never a tty (`ssh -tt` included).
