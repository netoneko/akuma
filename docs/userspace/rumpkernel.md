# rumpkernel

`rump_server`: the NetBSD rump kernel running as a userspace process, providing
the alternate (non-smoltcp) network stack via the kernel sysproxy.

Docs live at [`userspace/rumpkernel/docs/`](../../userspace/rumpkernel/docs/):
- `RUMP_SYSPROXY.md` — the committed sysproxy design.
- `HIJACK_VS_KERNEL_PROXY.md` — why kernel-side routing was chosen.
- `FIBER_HANDOFF.md` — cooperative scheduling handoff with the host kernel.
- `NATIVE_STACK_INTERNET.md` — validating the native (smoltcp) stack.
- `PHASE01_BUILDRUMP.md`, `PHASE2_RUMPUSER.md`, `PHASE3_KERNEL_TAP.md` — the
  bring-up phases.
- `ARCHITECTURE_QUESTIONS.md`, `FRANKENLIBC_EVAL.md`, `DEV_ZERO.md`,
  `RUMP_LATENCY_SLEEP_FIX.md`.

See also: [`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md),
[`../reference/subsystems/networking.md`](../reference/subsystems/networking.md),
[`../runbooks/debug-devbox.md`](../runbooks/debug-devbox.md).
