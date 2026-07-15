# sshd

Userspace SSH server (used by the devbox, where the built-in in-kernel SSH is
smoltcp-only and unavailable under `rump-default`).

Docs live at [`userspace/sshd/docs/`](../../userspace/sshd/docs/):
- `FLOW.md` — connection/session flow.
- `INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md` — PTY bridge drain fix.
- `LIMITATIONS.md`, `MIGRATION_SUMMARY.md`.

See also: [`../reference/subsystems/ssh.md`](../reference/subsystems/ssh.md),
[`../runbooks/debug-devbox.md`](../runbooks/debug-devbox.md).
