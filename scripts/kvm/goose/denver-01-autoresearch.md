# Denver: run one autoresearch session (one gate)

You are driving **Denver**, a teaching kernel whose curriculum runs from tokio to a Clojure web
server, with an open-weight model doing the development on Akuma, guests booting under Akuma KVM.
The whole procedure is one document: `denver/docs/runbooks/AUTORESEARCH.md`. Read it first and follow
it; it is the procedure, the code rules (R1–R8), the phase map, and the stall protocol. The frozen
plan it serves is `denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md` — read it
once for the why, never edit it.

## Session

- One session = **one gate**. Orient per the runbook's step 1; take the gate named by the last
  `NEXT GATE:` line (or the phase definition if there is none); finish with a record.
- No code before the research artifact exists (`denver/docs/research/<pass>.md`, rule R1). If your
  implementation exceeds ~2× the LOC estimate, stop and re-research (R5).
- A gate passes only when the named probe line appears in captured output saved to
  `denver/gates/<gate>.log`. Self-assessment is not evidence.
- **Booting the guest: use `sh denver/vm-probe.sh 2>&1 | tee denver/gates/<gate>.log`**, not a bare
  `herd vm-run`. The host runs one VM at a time; a stale guest answers `the kernel refused the spec
  (errno 16)` (EBUSY), which is a taken slot and not a broken KVM. The script frees the slot, boots,
  waits for an ssh round trip, runs `uname` + `tokioprobe`, and stops the guest. Phase 0's gate
  passed through it on the metal (2026-11-13). A different probe for a later gate: copy its shape,
  keep the free-the-slot step and the stop at the end.

## Gates (per phase; the runbook holds the table)

Stop at the first that fails and write down why. Three hypotheses without a verdict = stall
protocol: record it, insert a diagnostic pass if one is missing, re-rank `REQUIREMENTS.md` openly.

## House rules

- **Commit WIP every ~15 minutes**: `git commit --no-verify -m "denver(wip): <gate> - <what>"` and
  push to `litter`, but only while what you touched still builds (`cargo build -p <crate>` / the
  kernel target). WIP commits are protection, never evidence — the gate's record still commits only
  through the driver after the bookkeeping check.
- Branch `denver`; the driver commits and pushes to `litter` (`git push litter denver`) after the
  bookkeeping check — docs with `--no-verify`, code after targeted `cargo test -p` on the crates you
  touched; never push to `origin`; never rewrite history.
- You change only `denver/`, `akuma-jvm-kernel/`, and existing crates behind **off-by-default
  features** (R6). No middleware, no hook tables (R7). Workspace lints stay clean (R3).
- Boot guests with whatever the machine has: Akuma KVM or the QEMU line in the relevant README.
  Never kill a VM by pattern; stop only the one you started (its pid file).
- Log every intervention you needed in `RESULT.md` — interventions are data, not failure.
- Decisions have precedents (R9): search `docs/archive/`, `docs/reference/` and the other kernels
  (`shanghai/`, `taoxichuan/`, the `akuma-tokio-kernel*` crates) before deciding; cite what you found.
- Stuck beyond the stall protocol? Write `denver/ESCALATE.md` (`QUESTION:` / `CONTEXT:`), leave
  `NEXT GATE:` pointing at this same gate, and stop — the driver stops the loop for a human or a
  stronger model (`ESCALATE_MODEL=<model> sh denver/autoresearch.sh 1 <gate>`).

## End

Update `denver/docs/LOOP_LOG.md` (Record voice: what we did, what decision it forced), the living
lesson's *Do*/*Record* sections, `RESULT.md`, commit, push, and leave the last line of `LOOP_LOG.md`
as `NEXT GATE: <name>`.
