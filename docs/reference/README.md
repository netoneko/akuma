# Reference

Current-state architecture and invariants. No history, no narrative - for the
investigation behind a design, follow the "Background" links into
[`../archive/`](../archive/).

## Subsystems

| Subsystem | Doc | Status |
|---|---|---|
| Boot / MMU / DTB | [`subsystems/boot.md`](subsystems/boot.md) | Phase 4 |
| Memory (PMM, heap, COW) | [`subsystems/memory.md`](subsystems/memory.md) | Phase 4 |
| Scheduler / threads / SMP | [`subsystems/scheduler.md`](subsystems/scheduler.md) | Phase 4 |
| Networking (box model, two stacks) | [`subsystems/networking.md`](subsystems/networking.md) | **Phase 2 (done)** |
| Rump stack (sysproxy, fiber) | [`subsystems/rump-stack.md`](subsystems/rump-stack.md) | **Phase 2 (done)** |
| SSH (built-in + userspace) | [`subsystems/ssh.md`](subsystems/ssh.md) | Phase 4 |
| VFS (ext2, procfs, pipes) | [`subsystems/vfs.md`](subsystems/vfs.md) | Phase 4 |
| Syscalls / Linux ABI | [`subsystems/syscalls.md`](subsystems/syscalls.md) | Phase 4 |
| Containers / boxes / herd | [`subsystems/containers.md`](subsystems/containers.md) | Phase 4 |
| Cargo features + env knobs | [`subsystems/config-flags.md`](subsystems/config-flags.md) | **Phase 2 (done)** |
| Drivers | [`subsystems/drivers/`](subsystems/drivers/) | Deferred (gap list) |
| Shell | [`subsystems/shell.md`](subsystems/shell.md) | Deferred (gap list) |

## ABI

| Topic | Doc | Status |
|---|---|---|
| Linux compatibility | [`abi/linux-compat.md`](abi/linux-compat.md) | Phase 4 |
| musl libc | [`abi/musl.md`](abi/musl.md) | Phase 4 |

## Build system

| Topic | Doc | Status |
|---|---|---|
| Profiles, scripts, disk images | [`build-system.md`](build-system.md) | Phase 4 |

## Authoring a reference doc

1. Describe **current state only**. If a design changed, the old design belongs
   in `archive/`, linked from a "Background" footer.
2. State invariants explicitly ("box 0 is always on the native stack unless
   `rump-default` is enabled").
3. Use `file:line` references into `src/` for the authoritative source.
4. Cross-link runbooks that build on this reference.
