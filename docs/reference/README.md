# Reference

Current-state architecture and invariants. No history, no narrative - for the
investigation behind a design, follow the "Background" links into
[`../archive/`](../archive/).

## Stability grades

Each doc carries a grade (A stable / B watch / C active risk) based on git
churn. See [`../README.md`](../README.md).

## Subsystems

| Subsystem | Doc | Grade |
|---|---|---|
| Boot / MMU / DTB | [`subsystems/boot.md`](subsystems/boot.md) | B |
| Memory (PMM, heap, COW) | [`subsystems/memory.md`](subsystems/memory.md) | **C** |
| Scheduler / threads / SMP | [`subsystems/scheduler.md`](subsystems/scheduler.md) | A |
| Networking (box model, two stacks) | [`subsystems/networking.md`](subsystems/networking.md) | **C** |
| Rump stack (sysproxy, fiber) | [`subsystems/rump-stack.md`](subsystems/rump-stack.md) | B |
| SSH (built-in + userspace) | [`subsystems/ssh.md`](subsystems/ssh.md) | A |
| VFS (ext2, procfs, pipes) | [`subsystems/vfs.md`](subsystems/vfs.md) | A |
| Syscalls / Linux ABI | [`subsystems/syscalls.md`](subsystems/syscalls.md) | A |
| Containers / boxes / herd | [`subsystems/containers.md`](subsystems/containers.md) | B |
| Cargo features + env knobs | [`subsystems/config-flags.md`](subsystems/config-flags.md) | — |

## Not yet written (deferred gap list)

These are tracked in [`../../proposals/DOCS_MIGRATION_PLAN.md`](../../proposals/DOCS_MIGRATION_PLAN.md):

- **ABI:** `abi/linux-compat.md`, `abi/musl.md` — material is in
  `archive/MUSL_COMPATIBILITY.md`, `archive/SYSCALL_HARDENING.md`.
- **Build system:** `build-system.md` — profiles + `scripts/` + disk images
  (partly covered by `subsystems/config-flags.md`).
- **Drivers:** `subsystems/drivers/` (virtio, gic, timers, block, console,
  framebuffer, audio, fw_cfg, rng) — Tier A/B/C in the gap list.
- **Shell:** `subsystems/shell.md` — `src/shell/` + `shell/commands/`.
- **Syscall sub-families:** per-family docs (most covered by
  `subsystems/syscalls.md`'s index).

For these, consult `archive/` directly in the meantime.

## Authoring a reference doc

1. Describe **current state only**. If a design changed, the old design belongs
   in `archive/`, linked from a "Background" footer.
2. State invariants explicitly ("box 0 is always on the native stack unless
   `rump-default` is enabled").
3. Use `file:line` references into `src/` for the authoritative source.
4. Cross-link runbooks that build on this reference.
5. Add a stability grade at the top (A/B/C) with a one-line justification.
