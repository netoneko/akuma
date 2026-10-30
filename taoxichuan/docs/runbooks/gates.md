# Run the stage gates

The gates are the ladder's evidence: each stage's gate is a set of guest
console lines plus exit codes, collected rotation-proof (herd **replaces**
`vm-dev.log.old` mid-boot — read slices of the *current* boot, anchored by the
`epoch=` line or a per-boot timestamp, never the rotated file; measured twice
2026-10-26, see the `fill_statfs` comment in `src/main.rs`).

## The gates and their vocabulary

| stage | gate | the line(s) that pass it |
|---|---|---|
| 1 | reach init spawn, zero faults | `stage 1 gate reached -- init spawn`; zero `[Fault]`/`PANIC`/`[sig!]` |
| 2 | static musl hello in ring 3 | `hello exit code 0`, `stage 2 gate reached` |
| 3 | busybox echo + ls, sequential, byte-identical to the host reference | `stage 3 gate reached -- sequential tasks ran clean`; both exit codes 0 |
| 4 | sshd serves a session | ssh round trip from the host (`boot_dev.sh` waits for it) |
| 6/7 | `apk add curl`, then `curl -sS https://example.com` | agent-log stage entries; the `[statfs]` lines carry the anchored `tstamp=` a gate checks against this boot's epoch |
| M2/P* | pipes, blocking fork, wait4 | the PH result doc's gate list (`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md`) |

The gate runner is `scripts/kvm/goose/run_gates.sh`; per-session briefs live
beside it (`scripts/kvm/goose/taoxichuan-*.md`).

## Reading the console

- `grep -a` is mandatory (QEMU/herd logs contain control bytes; plain grep
  reads them as binary and prints nothing).
- `[sc] unhandled <Name> (<nr>)` is a checklist line, not a failure — it says
  exactly which syscall a stage still needs.
- `[Fault] ring3 …` killed the task; anything without `ring3` killed the
  kernel. Both halt the guest.
- `[cx]`, `[w]`, `[wv]`, `[o]`, `[statfs]`, `[TICK pre/post]` are probes the
  stages left in on purpose; they are bounded (per-call or per-second) and are
  part of the evidence, not noise.

## Verify

- The gate script's summary lines name PASS/FAIL per gate; a silent probe is
  never scored a pass (the runner refuses it — same rule as the repo's
  `mem_suite.py`).
- For a FAIL, the first `unhandled`/`[Fault]` line at or after this boot's
  anchor is the cause list; cross-reference `../reference/syscalls.md` for
  what the number would have needed.

## Background

`docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md`,
`docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md`,
`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md`.
