# sshd

Userspace SSH server (used by the devbox, where the built-in in-kernel SSH is
smoltcp-only and unavailable under `rump-default`).

Docs live at [`userspace/sshd/docs/`](../../userspace/sshd/docs/):
- `FLOW.md` — connection/session flow.
- `INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md` — PTY bridge drain fix.
- `EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` — **open**: exec output >1 MiB is
  silently lossy (kernel `ProcessChannel` drop-oldest, no backpressure) and
  newline-mangled below that (unconditional `\n`→`\r\n` on the live path only).
- `EXIT_STATUS_FIX.md` — exit-status channel request (`ssh` no longer 255).
- `SSH_KEEPALIVE_TIMEOUT_FIX.md` — an unanswered `SSH_MSG_GLOBAL_REQUEST`
  (e.g. OpenSSH's `keepalive@openssh.com`) silently dropped instead of
  getting `SSH_MSG_REQUEST_FAILURE`, so an idle client eventually gave up
  with `Timeout, server X not responding`.
- `CLIENT_REAL_SERVER_INTEROP_FIX.md` — `ssh` client vs a real server: it
  choked on interleaved `GLOBAL_REQUEST`/`WINDOW_ADJUST` messages that only a
  real (non-Akuma) sshd sends.
- `LIMITATIONS.md`, `MIGRATION_SUMMARY.md`.

See also: [`../reference/subsystems/ssh.md`](../reference/subsystems/ssh.md),
[`../runbooks/debug-devbox.md`](../runbooks/debug-devbox.md).
