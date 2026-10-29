# Taoxichuan docs — front door

Taoxichuan (陶溪川) is the minimal composed kernel: a kernel composed from this
repo's crates, built by a small open model (goose + GLM) as the
minimal-self-hosting experiment. The brief is
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, its running log is
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`. The code is
`taoxichuan/` (a composition root over `crates/`; a piece that exists in a
crate is pulled, never copied). **This folder is the living documentation of
the kernel as it is now**; the archive holds how it got here.

This folder replaced `docs/taoxichuan/` (which is now a pointer here): the
docs live next to the kernel they describe so a session touching
`taoxichuan/src/` sees them.

## Layout of this folder (the model keeps it current)

| path | what it holds |
|---|---|
| `README.md` | this front door: status, the doc rules, the symptom matrix |
| `reference/` | one doc per subsystem, current behaviour and invariants only: [`boot.md`](reference/boot.md), [`memory.md`](reference/memory.md) (address map, mmap/brk slabs, user-pointer rules), [`processes.md`](reference/processes.md) (spawn crossing, exec-replace, blocking fork, wait4, slots), [`scheduling.md`](reference/scheduling.md) (the LAPIC tick, the runtime twin, what preemption will be), [`pipes.md`](reference/pipes.md), [`syscalls.md`](reference/syscalls.md) (the table: number, name, arm, crate that decides, status), [`devices.md`](reference/devices.md) (COM1, virtio-mmio blk/net, LAPIC, PIT, RDRAND), [`rust-toolchain.md`](reference/rust-toolchain.md) (the rustc syscall list scored against this kernel's table — 7/10, AF_UNIX is the stage-9 blocker) |
| `runbooks/` | procedures with a **Verify** section: [`build-and-boot.md`](runbooks/build-and-boot.md), [`gates.md`](runbooks/gates.md) |
| `diagrams/` | mermaid source (`.md` with a fenced `mermaid` block) for the figures below; the same figures are embedded inline in the reference docs |
| `gaps.md` | what the kernel does not do yet, ranked, each with the probe that shows it |

## Status (update at the end of every session)

| ladder stage | state | evidence |
|---|---|---|
| 1–4 boot, userspace, busybox, sshd | done | agent log, stages 1–4 |
| 6 `apk add curl` | done | agent log, stage 6 |
| 7 `curl -sS https://example.com` | done | agent log, stage 7 |
| M4 process model (real fork, wait4, pipes, preemption) | F1, PK1, P1–P5 green; PK2 and a rare boot stall open | `docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` |
| 8 `git clone` | not started | `scripts/kvm/goose/taoxichuan-09j-stage8-git-clone.md` |
| tcc compiles and runs a C program in the guest | works (compile to a file, then run it) | agent log, "First compiler probes" |
| 9–12 cargo fetch, rustc, cargo, fixed point | not started | gap list in the agent log; [`gaps.md`](gaps.md); [`reference/rust-toolchain.md`](reference/rust-toolchain.md) (70% of the rustc syscall list served) |

## Figures every session must keep true (draw them, do not just describe them)

1. **Boot to init** — [`reference/boot.md`](reference/boot.md) § boot order; source [`diagrams/boot-to-init.md`](diagrams/boot-to-init.md).
2. **The syscall path** — [`reference/syscalls.md`](reference/syscalls.md) § the crossing; source [`diagrams/syscall-path.md`](diagrams/syscall-path.md).
3. **Process model** — [`reference/processes.md`](reference/processes.md); source [`diagrams/process-model.md`](diagrams/process-model.md).
4. **Scheduling** — [`reference/scheduling.md`](reference/scheduling.md); source [`diagrams/scheduling.md`](diagrams/scheduling.md).
5. **Memory map** — [`reference/memory.md`](reference/memory.md); source [`diagrams/memory-map.md`](diagrams/memory-map.md).

## Symptom matrix — "I see X on the guest console, what do I read?"

| Symptom | Start here |
|---|---|
| `[Fault] …` or `PANIC …` and a halt | [`reference/boot.md`](reference/boot.md) § traps (every vector reports and halts); a `ring3` line means the task died, anything else means the kernel did |
| `unhandled <Name> (<nr>)` | [`reference/syscalls.md`](reference/syscalls.md) — the arm does not exist yet; add the row with the arm |
| spawn/exec prints `read failed` for a path on the image | [`reference/processes.md`](reference/processes.md) § spawn; check the symlink chain (`/bin/sh` is one) and `read_symlink` vs `read_file` |
| a command writes nothing to its redirect target, rc 0/1 | [`reference/syscalls.md`](reference/syscalls.md) § fd routing (table-first; the `Session` fd materialization) |
| network call hangs with no `[cx]` line progress | [`reference/devices.md`](reference/devices.md) § the pump (nothing moves frames except `netpoll` laps) |
| the boot stalls before the banner | [`runbooks/build-and-boot.md`](runbooks/build-and-boot.md) § Verify; [`gaps.md`](gaps.md) (the rare boot stall is a known open item) |

## Doc rules

- `reference/` states what is true now. An incident, a hunt log or a "fixed on
  \<date\>" story goes to `docs/archive/` under an `UPPER_SNAKE` name, and the
  reference doc gets only the rule it established plus a link.
- Reference docs carry the repo's stability grade (A stable / B verify / C
  active risk). Everything here is **C** until the kernel stops moving.
- Every syscall arm has a row in `reference/syscalls.md`; a new arm adds its
  row in the same commit.
- A diagram changes in the commit that changes the behaviour it shows.
- Link the crate that owns each decision (`crates/<name>`), so a reader can
  follow the composition.
- Runbooks are procedures, not investigation logs; each ends with a **Verify**
  section.
- The front door `docs/README.md` already points here (subsystem index row
  "Taoxichuan"); keep that row's one-liner current.

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`,
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`,
`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`,
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md`,
`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md`.
