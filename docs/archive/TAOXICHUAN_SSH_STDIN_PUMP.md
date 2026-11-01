# sshd's stdin pump fed nothing on a `linux-abi` build (2026-11-01)

**Stability: B**: fixed and verified on the Mac TCG rig (Taoxichuan); not rerun on the ryzen.

## Symptom

On Taoxichuan, any ssh exec session that sends data on stdin hung: `ssh host 'cat > f' < file`, `ssh host sh < script`,
uploading a binary over ssh. sshd logged `torn down by the idle watch: no inbound bytes for 690s` while `cat` sat in
`read(0)`. Sessions that send no stdin (`ssh host 'cmd'`, the whole regression battery, goose) were unaffected, which is
why nothing caught it. Found while trying to copy `tokioprobe` into a running guest.

Hangs at 6 B, 4 KiB and 64 KiB on the kernel at `56d121bc`. On the kernel at `875a3d8d` 6 B got through and 4 KiB hung.
The kernel was not the cause, so that difference is incidental.

## Cause

Two halves that were each correct for the ABI they were written against:

- **The pump** (`userspace/sshd/src/protocol.rs`, `bridge_process`) writes the child's stdin through
  `open("/proc/<pid>/fd/0", O_WRONLY)` and ends it with `close_child_stdin(pid)`, Akuma's private `CLOSE_CHILD_STDIN`
  syscall. That is the native contract: the kernel's `SPAWN` gives the child a stdin channel, `/proc/<pid>/fd/0` resolves
  to it, and the kernel closes it on request.
- **The `linux-abi` spawn** (`userspace/libakuma/src/lib.rs`, `spawn_full`) is `pipe2` + `fork` + `dup3` + `execve`, and
  it piped only stdout. Its comment said `stdin` was "not wired up on this path", because the one caller it was written
  for (meow's `Shell` tool) never sends any. So the child inherited the spawner's own fd 0. `/proc/<pid>/fd/0` then
  named that inherited descriptor, not a pipe the child reads from. `CLOSE_CHILD_STDIN` was deleted from Taoxichuan in
  `f821ddda` and has never existed on Linux, so the EOF was never delivered either.

The classic pump blunder: the pump wrote, with no error, into a stdin the child was not reading, and the child waited
for input and an EOF that nothing sent.

## Fix

- `SpawnResult` has a third field, `stdin_fd: Option<u32>`: the write end of the child's stdin pipe, owned by the
  caller. Closing it is the EOF. It is `Some` only on the `linux-abi` path; natively the kernel keeps owning the channel
  and the field is `None`.
- The `linux-abi` `spawn_full` creates a second pipe (both pipes `O_CLOEXEC`, so the spawner's ends cannot leak into a
  later child's image) and `dup3`s its read end onto the child's fd 0. With no `stdin` bytes the caller gets the write end.
  With bytes, they are written and the end is closed, so the child reads them and then EOF.
- Plain `spawn()` passes empty stdin bytes on `linux-abi`, so its child gets an immediate EOF, as with std's
  `Command::output`. Its only `linux-abi` caller (meow) never writes a stdin. An open write end would leak an fd per call
  and park any child that reads stdin.
- `bridge_process` takes `child_stdin`. With it, the pump writes there and EOF is `close`. Without it, the native
  procfs path and `close_child_stdin` are unchanged.
- herd's two `SpawnResult` patterns gained `..`, and box's native construction sets `stdin_fd: None`.

## Verified

Taoxichuan on the TCG rig, image copy with the new `linux-abi` sshd staged by `debugfs`:

- `cat > f` with 6 B, 4 KiB, 64 KiB and 1.5 MB on stdin: all arrive, 0.2-0.7 s. 1.5 MB is past the 1 MiB SSH window, so
  `WINDOW_ADJUST` works on this path too.
- A 2.2 MB binary piped in: md5 identical to the host's.
- `ssh host sh` with a two-line script on stdin: runs, and its `exit 3` comes back as the session's status.
- `ssh -tt` with piped input: the pty-requested path runs the command.
- Battery 10/10 + goose, 0 faults.
- Builds: sshd `linux-abi` (x86_64), sshd native (AArch64 default, amd64 `builtin-paws`), herd, box. Host tests: sshd
  `--lib` 60, box `--lib` 175.

Not verified: the native sshd on the AArch64 kernel and amd64 was not booted. Its path is unchanged except for the new
`None` field. A `linux-abi` pty session gets a pipe, not a pty (`flags` is still not wired on that path), as before.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step 2e (`f821ddda`) deleted `CLOSE_CHILD_STDIN` with the
  rest of the private spawn path.
- [`CTRL_C_SIGINT_DELIVERY.md`](CTRL_C_SIGINT_DELIVERY.md): the native stdin chokepoint `/proc/<pid>/fd/0` reaches.
- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md): the 2026-11-01 "classic
  blunders" entry.
