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
| Scheduler / threads | [`subsystems/scheduler.md`](subsystems/scheduler.md) | A |
| SMP / multikernel | [`subsystems/smp.md`](subsystems/smp.md) | **C** |
| Networking (box model, two stacks) | [`subsystems/networking.md`](subsystems/networking.md) | **C** |
| Rump stack (sysproxy, fiber) | [`subsystems/rump-stack.md`](subsystems/rump-stack.md) | **C** |
| SSH (built-in + userspace) | [`subsystems/ssh.md`](subsystems/ssh.md) | A |
| VFS (ext2, procfs, pipes) | [`subsystems/vfs.md`](subsystems/vfs.md) | A |
| Syscalls / Linux ABI | [`subsystems/syscalls.md`](subsystems/syscalls.md) (per-family docs in [`subsystems/syscalls/`](subsystems/syscalls/), grades vary) | A |
| Containers / boxes / herd | [`subsystems/containers.md`](subsystems/containers.md) | B |
| Cargo features + env knobs | [`subsystems/config-flags.md`](subsystems/config-flags.md) | — |
| IRQ dispatch | [`subsystems/irq.md`](subsystems/irq.md) | B |
| Console / UART output | [`subsystems/console.md`](subsystems/console.md) | B |
| Kernel RNG (VirtIO entropy) | [`subsystems/rng.md`](subsystems/rng.md) | B |
| Async filesystem wrappers | [`subsystems/async-fs.md`](subsystems/async-fs.md) | A |
| In-kernel shell | [`subsystems/shell.md`](subsystems/shell.md) | **C** |
| In-kernel editor ("neko") | [`subsystems/editor.md`](subsystems/editor.md) | A |
| Exceptions (vector table, trap frame, ESR_EL1) | [`subsystems/exceptions.md`](subsystems/exceptions.md) | **C** |

### Drivers

| Driver | Doc | Grade |
|---|---|---|
| GIC (v2/v3 interrupt controller) | [`subsystems/drivers/gic.md`](subsystems/drivers/gic.md) | B |
| Timers (CNTV tick + alarm queue) | [`subsystems/drivers/timers.md`](subsystems/drivers/timers.md) | B |
| Block (VirtIO-blk + DMA HAL) | [`subsystems/drivers/block.md`](subsystems/drivers/block.md) | A |
| fw_cfg (QEMU firmware config) | [`subsystems/drivers/fw_cfg.md`](subsystems/drivers/fw_cfg.md) | A |

### ABI

| Doc | Covers |
|---|---|
| [`abi/linux-compat.md`](abi/linux-compat.md) | Syscall calling convention, errno encoding, ELF/auxv contract |
| [`abi/musl.md`](abi/musl.md) | What musl expects of the kernel, posix_spawn/CLONE_VFORK |

### Build system

[`build-system.md`](build-system.md) — the profile/feature pairing model
(`release`/`size`/`extreme-size`/`release-smp`/`release-smp-shared`/`devbox`/
`devbox-smoltcp`), their `scripts/build_*.sh` wrappers, the disk-image
lifecycle, and the userspace build.

[`build-profiles.md`](build-profiles.md) — the same seven targets as a
"which one do I build/run" comparison table (sizes, networking, purpose).

## Not yet written (deferred gap list)

Tier A and B are now done (drivers above + the subsystems listed above); only
Tier C (niche) remains:

- **Drivers:** `subsystems/drivers/audio.md` (`src/audio.rs`),
  `subsystems/drivers/framebuffer.md` (`src/ramfb.rs` — distinct from
  `subsystems/syscalls/fb.md`, which covers the `fb.rs` syscall wrapper, not
  the ramfb device itself). No generic `drivers/virtio.md` overview exists;
  VirtIO-blk specifics live in `drivers/block.md`.

For these, consult `archive/` directly in the meantime.

## Authoring a reference doc

1. Describe **current state only**. If a design changed, the old design belongs
   in `archive/`, linked from a "Background" footer.
2. State invariants explicitly ("box 0 is always on the native stack unless
   `rump-default` is enabled").
3. Use `file:line` references into `src/` for the authoritative source.
4. Cross-link runbooks that build on this reference.
5. Add a stability grade at the top (A/B/C) with a one-line justification.
