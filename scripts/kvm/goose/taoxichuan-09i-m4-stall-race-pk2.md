# Taoxichuan stage 8, M4 session pi: the residual stall race, then PK2 (the probe's ALL PASS), then merge-readiness

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: ph and ph2 (`/root/dev-cycle/taoxichuan-s7/RESULT-ph.md` and `PLAN-ph.md`; your tree is `cde2247d`, also on `litter` as `taoxichuan-s4`).
`git log --oneline -3` first. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Build: `kbuild -p taoxichuan -j 4 --features no-tests`. Box log times are UTC.

## The goal
Session ph closed slice 3's main gates: in one clean boot (ph-9) with `REAL_FORK` on, F1, PK1 and P1-P5 passed with exact client bytes and `[SSH] Exit status: 0`, zero faults, no `[tmp]` prints. Two things are open, and the first explains the second:
1. **An intermittent stall.** Of three batteries on the same tree family, ph-7 was clean, ph-8 stalled at its first exec, ph-9 was clean. A stalled boot shows the guest going silent (no `hlt`, host core pinned, both relay directions frozen). ph converted five wait paths to halt; something else still spins or never yields the core.
2. **PK2** (`grandfork`, in the image, exits 0 after the `fs_base` fix) lost its output to a stall, so its `ALL PASS` has not been seen.
**Make the battery reliably clean (a stall rate you can state: N consecutive clean boots), see PK2's `ALL PASS` in one, and leave the tree ready to merge into `rust-org-il-experiment` (no `[tmp]`, `REAL_FORK` on, `amd64/` and aarch64 still build, host tests and clippy green for touched crates).** The runner's fault check also reads a stale mark file (`logmark-pf.txt`); fix it so a gate's verdict only reflects its own boot. Two phases.

## Phase 1 - plan (before any edit or boot)
Copy `PLAN-ph.md` to `PLAN-pi.md`, mark what ph proved, and write what is left: facts with console evidence apart from guesses, hypotheses ordered by cost per bit of information, a fix sequence with a stop condition per step. Commit it. Include as its own sections:
1. **What the other kernels do when idle.** The amd64 and aarch64 kernels have an idle contract (what runs when no task is runnable, who owns the halt, who pumps the network). Find it in `docs/reference/subsystems/` and the crates, and compare with Taoxichuan's `idle_main`, the tick path and every place a task can run without ever parking or syscalling. Pull a crate rather than re-derive its decision.
2. **The archive, for the symptom.** `grep -n -i` the stall's signature (relay frozen, `rx_starved`, `in_flight=0`, pinned core, handshake watchdog) in `docs/README.md`, `docs/runbooks/*.md` and `docs/archive/`; write the matching rows, or "no row", before reading source.
3. **A measurement that separates the hypotheses.** The stalled boot's host-side counters (`hlt`, `c4`, `rx_starved`) are already printed; say which hypothesis each reading rules out, and the cheapest instrument (one bounded print, one boot) for the next bit.

## Docs to read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `RESULT-ph.md` (the section "What the session proved", the hooks table, "What is next") and `PLAN-ph.md`.
2. `docs/archive/KVM_RELAY_SYN_STALL.md` and the docs `RESULT-ph.md` names for the one-pinned-core note.
3. `docs/README.md` symptom matrix; `docs/reference/subsystems/` entries for the scheduler, IRQ and console.
Probes already exist: `userspace/forkprobe`, `userspace/forktest/c_stress/`, `userspace/nettest/` (table in `docs/reference/userspace-layout.md`), host tests in the owning crate (`cargo test -p <crate> --target $HOST`).

## Premises (HOLDS / WRONG / UNCHECKED per row in RESULT-pi.md before your first edit; about 6 calls)
| # | premise | how to check |
|---|---|---|
| 1 | your tree is `cde2247d` and builds | `git log --oneline -3`; `kbuild` exit code |
| 2 | one clean battery still passes F1, PK1, P1-P5 | one boot, read `*.client` with `od -c` and the console exit lines |
| 3 | a stall can start before the first exec (ph-8) | read ph's ph-8 note and the capture it names |
| 4 | the runner's fault check uses a stale mark | read `run_fork.sh`/`run_pipes.sh` for `logmark-` |

## Guest facts (verified; do not rediscover)
One core, and the host relay thread shares the pinned host core with the vCPU. No procfs, no `/dev`. `PACE=15` after every exec. Ticks: PIT-calibrated LAPIC, vector 32, about 10 ms. Preemption switches only from ring 3; kernel paths run IF-masked. `vm-dev.log` rotates at about 250 KB: judge faults from your boot's own start line, never a whole-log grep. `boot_dev.sh` installs the artifact you name: use the default `/root/ktarget/...` path and compare the install line's size/md5 with the fresh build before trusting a boot.

## Composition rule (mandatory)
Look first (`grep -rn <name> crates/ amd64/src/`), pull by path dependency, extract from `amd64/src/` into a crate only if it lives only there (new crates `forbid(unsafe_code)`, unsafe budget 0), write new only when it exists nowhere, and say which for each piece. `amd64/` must keep building (`cargo check -p akuma-amd64 --target x86_64-unknown-none --features no-tests`); gate amd64-only code so the aarch64 build stays green. `wc -l taoxichuan/src/*.rs` at start and end (7918 now). Decisions already made (do not reopen): timer preemption stays in, SMP-ready (slot accessors, no new current-task `static`); ring-0 preemption stays out; keep M2's scaffold compiled and selectable.

## Gates (`run_fork.sh` and `run_pipes.sh`; at most 8 execs per boot)
- F1, P1-P5, PK1 as in ph's RESULT (unchanged).
- PK2: the probe's client bytes contain `grandfork: ALL PASS` and the console shows `[SSH] Exit status: 0`; zero `Fault|PANIC|sig!` after your boot's own start line; ticks still advance.
- Stability: report N consecutive full batteries (all of the above green in each, files timed within their boot). Target 5; say honestly what you got.
A gate is green only when the console exit line AND the client bytes are seen in the same boot. Read captures with `od -c`.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); fresh `.orig` image copy per boot.
- **Restore everything last and verify it before your final line**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`, then `herd status` (never bare `herd`).
- Never run python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. Do not copy keys or edit ssh/sshd config.
- Debug prints carry `[tmp]` and are listed under "Remove before final" in RESULT-pi.md. The pre-commit hook runs clippy `-D warnings` for the HOST target: run host clippy on any crate you touch before committing.

## Commit rule (you have no clock)
Commit before every build and after each piece that builds; write RESULT-pi.md early (premise rows first) and after every build, boot and commit. **Run `git push litter taoxichuan-s4` after every boot cycle that taught you something** (WIP is fine; say so in the message). Never push `origin`. Commit messages say "the user", not "owner".

## Budget
About 140 tool calls and 8 boots (stability runs are boots). If a step yields no new evidence after 2 boots, say so in the plan file, re-plan from what you now know (another pass over the archive and the other kernels), and continue.

## When to stop
PK2 `ALL PASS` seen and N consecutive clean batteries reported, with the tree merge-ready; or 8 boots with no new evidence; or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pi.md` (premise rows, hooks table updated, gates with console evidence, stall-rate statement, pulled/extracted/written-new, wc -l, what is next), restore ops, and end the turn.
