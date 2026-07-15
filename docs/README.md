# Akuma OS Documentation

This is the front door for all Akuma documentation. Use the tables below to
find what you need.

## How these docs are organized

```
runbooks/     Action-first. "Do X, expect to see Y." For debugging and building.
reference/    Current-state architecture and invariants. No history.
userspace/    Thin pointers to per-binary docs (kept co-located with source).
archive/      Every historical doc, moved verbatim. Linked from new docs, never rewritten.
```

For the reasoning behind this structure, see
[`proposals/DOCS_MIGRATION_PLAN.md`](../proposals/DOCS_MIGRATION_PLAN.md).

## Symptom matrix - "I see X, what do I read?"

| Symptom | Start here |
|---|---|
| VM won't boot / hangs early | `runbooks/debug-boot-hang.md` *(Phase 3)*; archive: `BOOT_STACK_BUG.md`, `DYNAMIC_DTB.md`, `QEMU_HVF_ISV_BUG.md` |
| SSH unreachable or slow to connect | `runbooks/debug-devbox.md` (devbox) / `runbooks/debug-ssh-latency.md` *(Phase 3)* |
| VM wedged, 100% CPU, unresponsive | `runbooks/recover-wedged-vm.md` |
| Kernel panic / OOM / allocation failure | `runbooks/debug-memory-oom.md` *(Phase 3)*; archive: `KERNEL_OOM_ALLOCATION_FIX.md`, `OOM_*` |
| Network doesn't work / can't connect out | `runbooks/debug-network.md` *(Phase 3)*; `reference/subsystems/networking.md` |
| `cargo` or `rustc` crashes in the devbox | `runbooks/debug-devbox.md` -> "Toolchain crashes" |
| `git clone` hangs or wedges | `runbooks/debug-devbox.md`; archive: `SCRATCH_CLONE_DECOMPRESSION_FIX.md`, `SIDEBAND_PARSER_FIX.md` |
| Fork / exec / signal misbehaviour | archive: `THREADING_RACE_CONDITIONS.md`, `SIGNAL_DELIVERY.md`, `CONTEXT_SWITCH_*` |
| Filesystem / ext2 / procfs errors | archive: `EXT2_FIRST_DATA_BLOCK_FIX.md`, `GETDENTS64_DIR_CACHE_FIX.md`, `PROCFS.md` |
| Porting a new binary (missing syscalls) | `reference/abi/linux-compat.md` *(Phase 4)*; archive: `*_MISSING_SYSCALLS.md` |

## Task list - "I want to do X"

| Task | Start here |
|---|---|
| Boot a devbox and SSH in | `runbooks/build-devbox.md` |
| Build the devbox image from scratch | `runbooks/build-devbox.md` |
| Compile the Akuma kernel *inside* Akuma (self-host) | `runbooks/selfhost-kernel-build.md` *(Phase 3)*; archive: `AKUMA_SELF_HOSTING.md` |
| Add an apk package to the devbox image | `runbooks/add-apk-package.md` *(Phase 3)* |
| Add a kernel `sc-*` feature and keep builds in sync | `runbooks/add-syscall-feature.md` *(Phase 3)* |
| Recover a wedged/hung VM without losing logs | `runbooks/recover-wedged-vm.md` |
| Understand how networking works | `reference/subsystems/networking.md`, `reference/subsystems/rump-stack.md` |
| Toggle debug knobs (`RUMP_SP_TRACE`, etc.) | `reference/subsystems/config-flags.md` |
| Run the acceptance playbooks | [`acceptance/`](../acceptance/) |

## Subsystem reference index

| Subsystem | Doc | Status |
|---|---|---|
| Boot / MMU / DTB | `reference/subsystems/boot.md` | Phase 4 |
| Memory (PMM, heap, COW) | `reference/subsystems/memory.md` | Phase 4 |
| Scheduler / threads / SMP | `reference/subsystems/scheduler.md` | Phase 4 |
| Networking (box model, two stacks) | `reference/subsystems/networking.md` | **Phase 2 (done)** |
| Rump stack (sysproxy, fiber) | `reference/subsystems/rump-stack.md` | **Phase 2 (done)** |
| SSH (built-in + userspace) | `reference/subsystems/ssh.md` | Phase 4 |
| VFS (ext2, procfs, pipes) | `reference/subsystems/vfs.md` | Phase 4 |
| Syscalls / Linux ABI | `reference/subsystems/syscalls.md` | Phase 4 |
| Containers / boxes / herd | `reference/subsystems/containers.md` | Phase 4 |
| Cargo features + env knobs | `reference/subsystems/config-flags.md` | **Phase 2 (done)** |
| Drivers (virtio, gic, timers...) | `reference/subsystems/drivers/` | Deferred (gap list) |
| Shell | `reference/subsystems/shell.md` | Deferred (gap list) |

See [`proposals/DOCS_MIGRATION_PLAN.md`](../proposals/DOCS_MIGRATION_PLAN.md)
for the full gap list of undocumented subsystems.

## When in doubt

If a runbook or reference doc doesn't exist yet for your problem, search
`archive/` for the topic - the historical investigations are thorough even if
not action-oriented. Every new doc has a "Background" footer linking back to
the relevant archive originals.
