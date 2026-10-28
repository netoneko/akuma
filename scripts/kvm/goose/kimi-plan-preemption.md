# Taoxichuan: plan timer-driven preemption and an SMP-ready scheduler (PLANNING ONLY: you write one document, you may read and run anything host-side)

You are goose-kimi (k3-256k), on the Mac, in a scratch git worktree of the Akuma repo (detached at the tip of branch `taoxichuan-s4`, the kernel goose-glm is building on the ryzen box). Your job is one document: `PLAN-kimi.md` at the worktree root. The experiment is `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`; its running log is `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (read the last five sections first).

## What you are planning, and why
Taoxichuan is a minimal kernel composed from the repo's crates. It has a real process model now (a forked child is a real task, `wait4` parks the parent, the child's exit wakes it) but **no timer preemption: it switches only where a task parks**, on one core. The user has decided that this is not a place to spend more time: **the kernel gets timer-driven preemption now, with a design that is SMP-ready** (booting extra cores can be a later slice, but nothing built now may assume one core). The two other kernels in this repo, amd64 and aarch64, already preempt and run SMP through shared crates. Goose-glm (a smaller model) is planning the same thing on the box at the same time; your plan is a second opinion that the user will compare with it.

## What the plan must contain
1. **What the repo already does.** How the amd64 and aarch64 kernels do timer-driven switching and SMP: which docs describe it and which crates own which decision. Name the crates and the docs, not line numbers.
2. **What Taoxichuan pulls, adapts, or writes new**, per piece, with the reason. Pull by path dependency where a crate exists. If something lives only in `amd64/src/` the plan says it needs extracting into a crate first. Say what each choice costs.
3. **Slices.** Each slice small enough for one goose-glm session of about 100 tool calls, each with a gate that is a console line or an exit code, an order that keeps the already-green gates (`scripts/kvm/goose/run_pipes.sh` P1-P5, F1) green, and what can be a host test (`cargo test -p <crate> --target $HOST`) before any boot.
4. **Risks and unknowns**, each with the cheapest way to resolve it (a doc to read, a host test, a one-boot probe). Include the cooperative-to-preemptive hazards the repo's archive records (stale thread slots, stale trap frames, switch-while-IRQs-masked, TLB/CR3 on switch, and whatever the docs name that I have not).
5. **SMP-readiness**: what must be true of the design now so that bringing up another core later is a slice and not a rewrite.
6. **A cost estimate** in goose-glm tool calls per slice, and what you would cut if only two slices fit.
7. **Docs read**, as a list, and **questions for the user** that the docs cannot answer.

## Read first (in order; read windows, never whole large files)
1. `docs/README.md` - the front door and symptom matrix.
2. `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` - the existing M4 design; it assigned preemption to a later slice. Check what it still gets right.
3. `docs/archive/TAOXICHUAN_M4_SLICE3_PE_RESULT.md` and `TAOXICHUAN_M4_SLICE3_PE_PLAN.md` - what the last session proved and the evidence of the wedge that triggered this decision.
4. `docs/reference/subsystems/` entries for the scheduler, SMP, shared-SMP, IRQ and exceptions; `docs/reference/crate-safety.md`; the archive docs those link, chosen by title.
5. `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`.
The crates are in `crates/`; `ls crates/` and read each candidate crate's README or top-of-file docs before its code.

## Premises (verify each before you rely on it; write HOLDS / WRONG / UNCHECKED per row at the top of PLAN-kimi.md; about 8 tool calls)
| # | premise | read-only check |
|---|---|---|
| 1 | Taoxichuan has no timer-driven task switch today | the M4 plan and RESULT-pe, then `grep -n "timer_trap" taoxichuan/src/*.rs` |
| 2 | both the amd64 and aarch64 kernels preempt on a timer and run SMP through shared crates | the subsystem docs above |
| 3 | `taoxichuan/Cargo.toml` already depends on the threading, exec, and LAPIC-timer crates | read it |
| 4 | the already-green gates are F1 with `REAL_FORK` off and P1-P5 | RESULT-pe |
| 5 | something in the archive says preempting a task that is inside a syscall is unsafe on this design | search the archive; report what it says and where |

## Rules
- **Read anything, run host-side cargo.** The whole repo is yours to read, including `amd64/src/` and every crate under `crates/`. Plan from what the crates really do: run their host tests and checks in this worktree (`cargo test -p <crate> --target $(rustc -vV | sed -n 's/host: //p')`, `cargo check -p <crate>`, `cargo doc -p <crate> --no-deps`) wherever it shows you an API or a behaviour you would otherwise be guessing at. Builds go to this worktree's own `target/`; that is fine. Say in the plan what you ran and what it showed.
- **Write** exactly one file, `PLAN-kimi.md`, at the worktree root. No other source edits, no `git commit` / `push` / `reset`, no ssh, and do not touch the ryzen box (goose-glm is using it). Do not run any other agent.
- Your quota is counted in requests: **batch several tool calls in one turn**, cap every read (`sed -n 'a,bp'`, `grep -n -m 20`), do not re-read a file you have read, and keep each cargo run to the crate you are asking about. Aim for about 60 tool calls in total.
- Compose, do not invent: look in `crates/` and `docs/` first. A plan that says "write a new scheduler" without showing why no crate fits is the wrong plan.
- If a doc contradicts this brief, the doc wins; say so in the plan.
- When `PLAN-kimi.md` is complete, end your turn. Do not wait for anything.
