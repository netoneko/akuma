# rumpkernel

`rump_server`: the NetBSD rump kernel running as a userspace process, providing
the alternate (non-smoltcp) network stack via the kernel sysproxy.

For current-state architecture, see
[`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md)
(internals: sysproxy, the fiber backend, syscall marshaling, known
limitations) and
[`../reference/subsystems/networking.md`](../reference/subsystems/networking.md)
(box routing, the two-stack model). To build/boot it, see
[`../runbooks/build-devbox.md`](../runbooks/build-devbox.md) and
[`../runbooks/debug-devbox.md`](../runbooks/debug-devbox.md).

History (build-out narrative, bug post-mortems, rejected designs) lives in
`archive/`, not co-located with the source anymore:
`RUMP_SYSPROXY.md`, `HIJACK_VS_KERNEL_PROXY.md`, `FIBER_HANDOFF.md`,
`RUMP_LATENCY_SLEEP_FIX.md`, `ARCHITECTURE_QUESTIONS.md`,
`FRANKENLIBC_EVAL.md`, `NATIVE_STACK_INTERNET.md`, `DEV_ZERO.md`,
`PHASE01_BUILDRUMP.md`, `PHASE2_RUMPUSER.md`, `PHASE3_KERNEL_TAP.md`,
`IMPLEMENTATION_PLAN.md`, `RUMP_PLUS_HERD.md`, `OPTIONAL_SMOLTCP.md`,
`MULTIKERNEL_NETWORKING_EXPERIMENT.md`.
