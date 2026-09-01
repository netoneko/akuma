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
| SMP (shared-kernel) | [`subsystems/smp-shared.md`](subsystems/smp-shared.md) | **C** |
| Networking (box model, two stacks) | [`subsystems/networking.md`](subsystems/networking.md) | **C** |
| Rump stack (sysproxy, fiber) | [`subsystems/rump-stack.md`](subsystems/rump-stack.md) | **C** |
| SSH (userspace `/bin/sshd`) | [`subsystems/ssh.md`](subsystems/ssh.md) | B |
| VFS (ext2, procfs, pipes) | [`subsystems/vfs.md`](subsystems/vfs.md) | A |
| Syscalls / Linux ABI | [`subsystems/syscalls.md`](subsystems/syscalls.md) (per-family docs in [`subsystems/syscalls/`](subsystems/syscalls/), grades vary) | A |
| Containers / boxes / herd | [`subsystems/containers.md`](subsystems/containers.md) | B |
| Boot-registered hooks (how crates call back up) | [`subsystems/kernel-hooks.md`](subsystems/kernel-hooks.md) | A |
| Cargo features + env knobs | [`subsystems/config-flags.md`](subsystems/config-flags.md) | — |
| IRQ dispatch | [`subsystems/irq.md`](subsystems/irq.md) | B |
| Console / UART output | [`subsystems/console.md`](subsystems/console.md) | B |
| Kernel RNG (VirtIO entropy) | [`subsystems/rng.md`](subsystems/rng.md) | B |
| Exceptions (vector table, trap frame, ESR_EL1) | [`subsystems/exceptions.md`](subsystems/exceptions.md) | **C** |

### Drivers

| Driver | Doc | Grade |
|---|---|---|
| GIC (v2/v3 interrupt controller) | [`subsystems/drivers/gic.md`](subsystems/drivers/gic.md) | B |
| Timers (CNTV tick + alarm queue) | [`subsystems/drivers/timers.md`](subsystems/drivers/timers.md) | B |
| Block (VirtIO-blk + DMA HAL) | [`subsystems/drivers/block.md`](subsystems/drivers/block.md) | A |

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

[`userspace-layout.md`](userspace-layout.md) — current `userspace/` member
list with one-line purposes; what `CLAUDE.md`'s Layout section points to
instead of hardcoding names that drift.

### Scripts

[`scripts/`](scripts/) — index of the standalone `scripts/*` debugging and
regression helpers not covered by `build-system.md` or a script's own
`README.md` (log/crash analysis, multi-VM hang hunting, fork/SMP regression
harnesses, container/env helpers).

### Devbox overlays

[`overlay/`](overlay/) — the `overlays/devbox/` and `overlays/devbox-smoltcp/`
dev-VM images: what each is for, and how to tell apart the two
similarly-named "devbox-smoltcp" things.

## Not yet written (deferred gap list)

Tier A and B are now done (drivers above + the subsystems listed above); only
Tier C (niche) remains:

- **Drivers:** `subsystems/drivers/audio.md` (`src/audio.rs`). No generic
  `drivers/virtio.md` overview exists; VirtIO-blk specifics live in
  `drivers/block.md`. (`drivers/framebuffer.md` and `drivers/fw_cfg.md` were
  listed here until 2026-08-31; the framebuffer and the fw_cfg driver that
  configured it are gone — [`../archive/FRAMEBUFFER_REMOVED.md`](../archive/FRAMEBUFFER_REMOVED.md).)
- **`akuma-terminal`** (`crates/akuma-terminal/src/lib.rs` — `TerminalState`:
  termios fields, mode flags, the canonical-mode line buffer): no dedicated
  crate doc. Covered only in passing by
  [`subsystems/syscalls/term.md`](subsystems/syscalls/term.md) and
  [`subsystems/ssh.md`](subsystems/ssh.md) § "Terminal handling", which
  document its *callers*, not its own state machine.

For these, consult `archive/` directly in the meantime.

## Authoring a reference doc

1. Describe **current state only**. If a design changed, the old design belongs
   in `archive/`, linked from a "Background" footer.
2. State invariants explicitly ("box 0 is always on the native stack unless
   `rump-default` is enabled").
3. Use `file:line` references into `src/` for the authoritative source.
4. Cross-link runbooks that build on this reference.
5. Add a stability grade at the top (A/B/C) with a one-line justification.
- [`crate-safety.md`](crate-safety.md) — which extracted crates are enforced `#![forbid(unsafe_code)]`, which cannot be, and why the ban is not in `Cargo.toml`.
