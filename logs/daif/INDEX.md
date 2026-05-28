# DAIF Fix Verification Runs (2026-05-28)

Tracks every kernel boot during the DAIF / IRQ-mask stability work.
See docs/STABILITY_URGENT_ISSUES.md (issue #1) for context.
Analyze any run with: `./scripts/daif_analyze.sh logs/daif/<run-dir>`

| Run | Label | TMR | Thread0 | SCHED | DAIF/5 | Verdict |
|-----|-------|-----|---------|-------|--------|---------|
| 20260528-180007 | 01a-baseline-idle (80s) | 12 | 18 | 0 | 0 (pre-tests) | OK |
| 20260528-180202 | 01b-ssh-trigger (single SSH @30s) | 17 | 26 | 0 | 0 (pre-tests) | OK |
| 20260528-183801 | 01c-ssh-stress (5x rapid SSH) | 26 | 45 | 0 | 0 (pre-tests) | OK |
| 20260528-191601 | 02-daif-tests (verify tests) | 12 | 20 | 0* | 5/5 | OK |
| 20260528-195143 | 03-boot-1 (45s smoke) | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195229 | 03-boot-2 | 6 | 10 | 0 | 5/5 | OK |
| 20260528-195315 | 03-boot-3 | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195401 | 03-boot-4 | 7 | 10 | 0 | 5/5 | OK |
| 20260528-195447 | 03-boot-5 | 7 | 12 | 0 | 5/5 | OK |
| 20260528-195544 | 04-idle-10min (caffeinate -i only) | 13 | 19 | 0 | 5/5 | host-sleep confound |
| 20260528-200717 | 04b-idle-150s (probe 100s mark) | 21 | 28 | 0 | 5/5 | OK |
| 20260528-201006 | 04c-idle-10min-dis (full caffeinate) | 83 | 125 | 0 | 5/5 | OK |

*sched=0 after excluding the deliberate test-induced warning in
test_yield_now_detects_masked_yield.

## Notes

- The originally documented hang did not reproduce. The 04-idle-10min run
  silently stalled at kernel uptime ~98s; 04c re-ran the same duration
  with `caffeinate -dis` and ran to completion 1:1 with wall time. The
  earlier stall was almost certainly macOS system sleep, not a kernel hang.
- Future endurance runs MUST use `caffeinate -dis` to avoid the same trap.
- The DAIF instrumentation in `crates/akuma-exec/src/threading/mod.rs`
  (`YIELD_WITH_IRQS_MASKED`) never triggered outside the deliberate test
  in any run.
