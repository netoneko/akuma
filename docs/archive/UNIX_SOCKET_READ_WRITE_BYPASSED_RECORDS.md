# AF_UNIX `read`/`write` bypassed the record queue: a lost exec errno, and a wedged kernel

**Status: FIXED 2026-11-03** in `crates/akuma-syscalls-glue/src/fs.rs`, which all three kernels share
(AArch64, amd64, Taoxichuan). Found on a Taoxichuan guest during
[`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md). Verified under QEMU TCG with a new probe,
`userspace/forktest/c_stress/unixrw.c`, which wedges the kernel without the fix.

## Symptoms

Two symptoms, from one cause, and a race decided which one showed:

1. **The exec errno was lost.** cargo built `infinity-agent-cli` with a 570-argument `rustc` line. Taoxichuan's
   execve refused it with `E2BIG` (the cap was 448 entries, [`TAOXICHUAN_EXEC_ARGV_LIMIT.md`](TAOXICHUAN_EXEC_ARGV_LIMIT.md)).
   Cargo has a fallback for exactly this: on `E2BIG` from spawn it retries with `@argfile`. It never took the
   fallback. It reported `process didn't exit successfully ... (exit status: 1)`, with no diagnostics. A
   spawn probe built with Rust std showed the same thing for any failed exec, a missing binary included:
   ```
   fork+exec   n=500: spawned, status=ExitStatus(unix_wait_status(256))
   missing (fork+exec): spawned?! Ok(ExitStatus(unix_wait_status(256)))
   missing (posix_spawn): spawn error raw=Some(2)
   ```
2. **The guest wedged.** Running that same probe on the ryzen guest, in the middle of GLM's build, stopped it
   dead:
   ```
   [BKL] cpu 2 stuck: owner cpu 1
   [BKL] cpu 3 stuck: owner cpu 1
   [BKL] cpu 0 stuck: owner cpu 1
   ```
   All four vCPUs sat at 100% and ssh stopped answering. Under TCG with `-monitor`, `info registers -a` put
   three CPUs in `smp::bkl_enter` and the BKL owner in `glue::unixsock::unix_recv_anc` /
   `pipe::pipe_check_set_reader`, with interrupts off.

## Cause

Rust std's `Command::spawn` takes the fork path whenever `pre_exec` is set, which is always under cargo,
because the jobserver sets it. On Linux, std reports a failed exec over a
**`socketpair(AF_UNIX, SOCK_SEQPACKET)`**, not a pipe (`library/std/src/sys/process/unix/unix.rs`):

- The child `write(2)`s `errno` followed by `"NOEX"`, then `_exit(1)`s.
- The parent `recv(2)`s 8 bytes. If it gets 0 (EOF), it concludes the exec succeeded.

glue served the two calls differently:

- `send`/`recv` on a unix socket go through `unixsock::unix_send_anc`/`unix_recv_anc`. Those keep a `Record`
  queue per pipe. A `SOCK_SEQPACKET` or `SOCK_DGRAM` receive keys entirely off it (`has_work = front.is_some()`).
- `read`/`write` on the same fd (`sys_read`/`sys_write`'s `FileDescriptor::UnixSocket` arms) moved raw bytes
  through the endpoint's pipe and never touched the record queue.

So the child's 8 bytes reached the pipe with no record, and the parent's `recv` could not see them:

- If the child exited first, `pipe_hup` was true and `recv` returned 0. std took the exec as successful,
  and cargo saw the child's `exit(1)`.
- If the parent reached `recv` first, it looped. `has_work` was false because there was no record.
  `pipe_check_set_reader` returned true because the pipe held bytes, so the thread never parked. The loop
  spun inside the syscall with the BKL held, so the child could never run its `_exit`, and every other CPU
  piled up on the BKL.

Python's `subprocess` was unaffected: it uses a plain pipe for its error channel, and `vfork`.

## Fix

On Linux, `read`/`write` on a socket are `recv`/`send` with flags 0. Both `UnixSocket` arms now delegate when
the fd has a table entry (`sock != 0`):

- `sys_write`: `unixsock::unix_send(fd, buf, false)`.
- `sys_read`: `unixsock::unix_recv(fd, buf, false, false)`, then copyout. A zero-length read returns 0 without
  calling it, because a framed `recv` with no buffer would consume a whole record.

The raw-pipe arms remain for the pre-table pipe pairs (`sock == 0`, the rump sysproxy channel). `unix_send`
and `unix_recv` take that path themselves for `sock == 0` too.

## Verification

All under TCG, SMP=4, on the `spawn.img` scratch image.

- `unixrw`, the new probe:
  - **Fixed kernel:** 8 PASS, 2 SKIP. The SKIPs are `SOCK_DGRAM` socketpairs, which glue refuses (below).
    `std spawn error channel`: errno delivered 20/20, EOF 0.
  - **The same kernel with only the two arms backed out:** wedged on the first case
    (`seqpacket write -> recv`), with the same `bkl_enter` / `pipe_check_set_reader` RIPs as above.
- The Rust std spawn probe, run 5 times: `missing (fork+exec): spawn error raw=Some(2)` every time. Before the
  fix, each run printed `spawned?!` or hung.
- The socketpair probes (`read` and `recv` variants, stream and seqpacket, with and without the parent
  closing its copy of the child's end) all pass.
- `cargo check` passes for the AArch64 kernel and `akuma-amd64`, and Taoxichuan builds.

## Still open

- `readv`/`writev` on a framed unix socket go through `sys_read`/`sys_write` once per iovec. That makes one
  record per iovec where Linux makes one per call. `sendmsg`/`recvmsg` gather and scatter properly.
- `socketpair(AF_UNIX, SOCK_DGRAM)` is refused (STREAM and SEQPACKET only).
- `recv`'s loop spins instead of erroring when a framed socket's pipe holds bytes without a record. That
  state should now be unreachable, but a desync would still wedge the kernel rather than fail one call.
- Taoxichuan has no `setitimer` (nr 38), so musl's `alarm()` does nothing there, and `unixrw`'s per-probe
  timeouts do not fire on that kernel.

## Background

- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md): the run that found it.
- [`UNIX_SOCKET_IMPROVEMENTS.md`](UNIX_SOCKET_IMPROVEMENTS.md): the record queue.
- `docs/reference/subsystems/syscalls/net.md` § AF_UNIX socketpair: the rule.
