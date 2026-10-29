# Taoxichuan (陶溪川) - the minimal composed kernel

Taoxichuan is a kernel composed from this repo's crates, built by a small open model (goose + GLM) as the
minimal-self-hosting experiment: `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` is the brief, its running log is
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`. The code is `taoxichuan/` (a composition root over
`crates/`; a piece that exists in a crate is pulled, never copied). This folder is the **living documentation of
the kernel as it is now**; the archive holds how it got here.

## Layout of this folder (the model keeps it current)

| path | what it holds |
|---|---|
| `README.md` | this front door: status, how to build/boot/gate, the doc rules |
| `reference/` | one doc per subsystem, current behaviour and invariants only: `boot.md`, `memory.md` (mmap/brk slabs, address map), `processes.md` (fork/exec/wait4, spawn crossing, slots), `scheduling.md` (tick, preemption, `switch_to`), `pipes.md`, `syscalls.md` (the table: number, name, arm, crate that decides, status), `devices.md` (virtio-mmio, console, net) |
| `runbooks/` | procedures with a **Verify** section: build, boot (Akuma `kvm0`, QEMU microvm), run the gates, build the guest image |
| `diagrams/` | mermaid source (`.md` with a fenced `mermaid` block) for the figures below |
| `gaps.md` | what the kernel does not do yet, ranked, each with the probe that shows it |

## Status (update at the end of every session)

| ladder stage | state | evidence |
|---|---|---|
| 1-4 boot, userspace, busybox, sshd | done | agent log, stages 1-4 |
| 6 `apk add curl` | done | agent log, stage 6 |
| 7 `curl -sS https://example.com` | done | agent log, stage 7 |
| M4 process model (real fork, wait4, pipes, preemption) | F1, PK1, P1-P5 green; PK2 and a rare boot stall open | `docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` |
| 8 `git clone` | not started | `scripts/kvm/goose/taoxichuan-09j-stage8-git-clone.md` |
| tcc compiles and runs a C program in the guest | works (compile to a file, then run it) | agent log, "First compiler probes" |
| 9-12 cargo fetch, rustc, cargo, fixed point | not started | gap list in the agent log |

## Figures every session must keep true (draw them, do not just describe them)

1. **Boot to init**: PVH entry, physmap, heap and PMM, virtio-blk, ext2 mount, init spawn, ring-3 entry.
2. **The syscall path**: `syscall` entry, `dispatch`, the arm, the crate that decides, the return crossing.
3. **Process model**: spawn crossing, `fork_process`, ring slots and threading tids, `wait4` park and the child's exit wake.
4. **Scheduling**: tick, the ring-3-only preemption switch, `switch_to` (CR3, `TASK_SLOT`, fs base), the idle loop.
5. **Memory map**: kernel, physmap, per-task user regions (image, brk slab, mmap slab, stack).

## Doc rules

- `reference/` states what is true now. An incident, a hunt log or a "fixed on <date>" story goes to `docs/archive/`
  under an `UPPER_SNAKE` name, and the reference doc gets only the rule it established plus a link.
- Every syscall arm has a row in `reference/syscalls.md`; a new arm adds its row in the same commit.
- A diagram changes in the commit that changes the behaviour it shows.
- Link the crate that owns each decision (`crates/<name>`), so a reader can follow the composition.
- Add this folder to `docs/README.md` (front door) and cite the archive docs a reference doc was written from in a
  "Background" footer.

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, `TAOXICHUAN_PROCESS_MODEL_PLAN.md`, `TAOXICHUAN_M4_SCHED_LITE_PLAN.md`,
`TAOXICHUAN_M4_SLICE3_PH_RESULT.md`, and the agent log above.
