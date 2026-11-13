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
docs live next to the kernel they describe.

**Where the code is (since B15, 2026-11-02).** `taoxichuan/src/main.rs` is one
`Kernel` chain over `akuma-tokio-kernel`; everything these docs describe moved,
unchanged, to [`crates/akuma-tokio-kernel-amd64/src/`](../../crates/akuma-tokio-kernel-amd64/src)
(`main.rs` became `lib.rs`; `child.rs`, `exec_twin.rs`, `net.rs`, `sched.rs`,
`smp.rs`, `ring.rs` kept their names). A file named here without a path is that
crate's. Every kernel built on the crate (Osaka is the sample) is described by
these docs too.

## Layout of this folder (the model keeps it current)

| path | what it holds |
|---|---|
| `README.md` | this front door: status, the doc rules, the symptom matrix |
| `reference/` | one doc per subsystem, current behaviour and invariants only: [`boot.md`](reference/boot.md), [`memory.md`](reference/memory.md) (address map, mmap/brk slabs, user-pointer rules), [`processes.md`](reference/processes.md) (init, fork, threads, exec, exit, wait4, the slot table, address-space lifetime), [`scheduling.md`](reference/scheduling.md) (the tick, the switch, idle, SMP and the big lock), [`pipes.md`](reference/pipes.md), [`syscalls.md`](reference/syscalls.md) (the crossing, who answers in what order, the allow-list's groups, this kernel's own arms), [`devices.md`](reference/devices.md) (COM1, virtio-mmio blk/net, LAPIC, PIT, RDRAND) |
| `runbooks/` | procedures with a **Verify** section: [`build-and-boot.md`](runbooks/build-and-boot.md), [`gates.md`](runbooks/gates.md) (the TCG rig and the ryzen gates) |
| `diagrams/` | mermaid source (`.md` with a fenced `mermaid` block) for the figures below; the same figures are embedded inline in the reference docs |
| `gaps.md` | what the kernel does not do yet, ranked, each with the probe that shows it |

## Status (update at the end of every session)

| ladder stage | state | evidence |
|---|---|---|
| 1–4 boot, userspace, busybox, sshd | done | agent log, stages 1–4 |
| 6 `apk add curl` | done | agent log, stage 6 |
| 7 `curl -sS https://example.com` | done | agent log, stage 7 |
| M4 process model (real fork, wait4, pipes, preemption) | done; since the cleanup every process is a fork child of init and wait4 is glue's | [`reference/processes.md`](reference/processes.md) |
| 8 `git clone` | done in its fixed-point form: a sparse HTTPS fetch from GitHub inside the guest (no key, ~9 s); sshd's binary-stdin caveat is G7 | `docs/archive/TAOXICHUAN_FIXED_POINT.md` § procedure; [`gaps.md`](gaps.md) |
| tcc compiles and runs a C program in the guest | works (compile to a file, then run it) | agent log, "First compiler probes" |
| 9–12 cargo fetch, rustc, cargo, fixed point | **done**: K1 == K2 byte for byte, under Linux KVM (2026-10-30) and on bare metal under Akuma's own hypervisor (2026-10-31), `-j4` at 3 GiB; `-j8` is G8 | `docs/archive/TAOXICHUAN_FIXED_POINT.md`; [`gaps.md`](gaps.md) |
| tokio programs | the B10 census (three unmodified tokio programs, 76 calls) has 0 unserved; the surface is `akuma-runtime-tokio` | [`reference/syscalls.md`](reference/syscalls.md); `docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md` |

The composition work since the fixed point (the fd family, processes, the slot table, the loader and the
syscall surface moved into shared crates) is `../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`, with each step's
gates and line counts.

## Figures every session must keep true (draw them, do not just describe them)

1. **Boot to init** — [`reference/boot.md`](reference/boot.md) § boot order; source [`diagrams/boot-to-init.md`](diagrams/boot-to-init.md).
2. **The syscall path** — [`reference/syscalls.md`](reference/syscalls.md) § the crossing; source [`diagrams/syscall-path.md`](diagrams/syscall-path.md).
3. **Process model** — [`reference/processes.md`](reference/processes.md).
4. **Scheduling** — [`reference/scheduling.md`](reference/scheduling.md); source [`diagrams/scheduling.md`](diagrams/scheduling.md).
5. **Memory map** — [`reference/memory.md`](reference/memory.md); source [`diagrams/memory-map.md`](diagrams/memory-map.md).
6. **SMP** — [`reference/scheduling.md`](reference/scheduling.md) § SMP; source [`diagrams/smp.md`](diagrams/smp.md).

## Symptom matrix — "I see X on the guest console, what do I read?"

| Symptom | Start here |
|---|---|
| `[Fault] …` or `PANIC …` | [`reference/boot.md`](reference/boot.md) § traps; a `ring3` line ended that task (SIGSEGV), anything else stopped the kernel |
| `[nosys] nr=<n>` | [`reference/syscalls.md`](reference/syscalls.md) § who answers — nothing serves that number; `nr=40` (`sendfile`) is expected ([`gaps.md`](gaps.md) G12) |
| exec of a path on the image fails | [`reference/processes.md`](reference/processes.md) § exec; check the symlink chain (`/bin/sh` is one) |
| a command writes nothing to its redirect target, or a child's stdout vanishes | [`reference/syscalls.md`](reference/syscalls.md) § who answers (the console intercept reads `Process.channel`) |
| sshd's client says `/bin/sh failed to spawn` | the image's sshd is not the `linux-abi` build ([`runbooks/build-and-boot.md`](runbooks/build-and-boot.md)) |
| network call hangs | [`reference/devices.md`](reference/devices.md) § the pump (nothing moves frames except `netpoll` laps) |
| the boot stalls before the banner | [`runbooks/build-and-boot.md`](runbooks/build-and-boot.md) § Verify; [`gaps.md`](gaps.md) G2 |

## Doc rules

- `reference/` states what is true now. An incident, a hunt log or a "fixed on
  \<date\>" story goes to `docs/archive/` under an `UPPER_SNAKE` name, and the
  reference doc gets only the rule it established plus a link.
- Reference docs carry the repo's stability grade (A stable / B verify / C
  active risk).
- Every arm `dispatch_inner` keeps locally has a row in `reference/syscalls.md`;
  a new local arm adds its row in the same commit. Calls glue answers are listed
  by the code (`akuma-runtime-tokio`'s groups), not here.
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
