# Denver efficiency ledger — what a session costs, which knobs move it

The loop tunes itself from measurements, not vibes. Quota law, measured over the real
sessions (GLM-5.3-flash on z.ai, 2026-11-13/14): ~98% of input tokens are cache reads, so
**cost = context size x calls**. This file is the loop's reference; the tables update from
the ledger and future measurements, not from memory.

## Where the data lives

- `denver/docs/COST_LEDGER.md` — one line per goose session (`sh denver/bookkeep.sh --cost
  --append`; the driver runs it after every session).
- `sh denver/bookkeep.sh --cost` — the newest session's line; `--digest` regenerates
  `denver/docs/STATE.md` (the ≤60-line orientation digest).
- Raw: goose's `/root/.local/share/goose/sessions/sessions.db` (`input_tokens` = final
  context, `accumulated_*` = totals, `messages.content_json` `toolRequest` parts = calls).
- Full write-up: `docs/archive/GOOSE_GLM_LOCAL_OPTIMIZATIONS.md` on branch
  `litter/goose-bench-results` (fetch it; do not merge it).

## Per-session table (sessions.db, 2026-11-14)

| session | wall | in | out | calls | final ctx | cache read |
|---|---|---|---|---|---|---|
| denver-PHASE-0-SCAFFOLD (1st) | 27 min | 4.8M | 66k | 85 | 114k | 98% |
| denver-PHASE-0-SCAFFOLD (redo) | 11 min | 1.1M | 34k | 34 | 67k | 94% |
| denver-PHASE-0-SCAFFOLD (3rd) | 27 min | 5.2M | 67k | 82 | 112k | 98% |
| denver-PHASE-2-WX | 28 min | 4.6M | 71k | 68 | 123k | 97% |
| denver-PHASE-2-FAULTNAME | 17 min | 2.5M | 50k | 52 | 86k | 97% |
| denver-PHASE-2-VDSO | 62 min | 19.3M | 131k | 138 | 220k | 99% |
| denver-merge-main (operator) | 35 min | 11.0M | 68k | 136 | 151k | 99% |

Facts behind the changes: 23–41 calls went by before the first edit in every session
(orientation cost); the largest tool results were a `tree` of the repo root (52k chars),
`cat denver/docs/LOOP_LOG.md` (39k), `cat denver/docs/REQUIREMENTS.md` (18k) — roughly
half of the 220k final context was tool results, re-sent on every later call.

## Knobs: what is verified, what is not

- `--max-turns` — **verified harmful when tight**: the local bench cut a task off before
  its deliverable at `--max-turns 6` (goose just stops). Driver default 150
  (`DENVER_MAX_TURNS`), with a brief line: records before turn ~120.
- `--max-tool-repetitions 5` (`DENVER_MAX_TOOL_REPETITIONS`) — flag exists (goose 1.52);
  effect on GLM untested.
- `GOOSE_AUTO_COMPACT_THRESHOLD` — shrank context in the bench (22k → 3k, n=1, small
  model, not GLM). **Untested on GLM: OFF by default**; the driver forwards it only when
  `DENVER_COMPACT=1` is set.
- `GOOSE_TOOL_CALL_CUTOFF` — documented (default 10 calls, summarizes older tool output);
  never exercised, internals unverified against the goose source.
- `GOOSE_CONTEXT_STRATEGY` — documented, untested here.
- `--system` — **verified unreliable as a guard**: "never run tree" in a system prompt was
  ignored by the bench model. Brief rules are reminders; the real fix is mechanical (the
  STATE.md digest replaces whole-file reads). The driver does not use `--system`.
- `--stats` — prints per-call token stats; available, not wired into the driver yet.

## The experiment rule (one per phase)

A session may propose exactly ONE settings experiment per phase, in its research artifact:
the knob, the expected effect, and the measure from `COST_LEDGER.md` that will judge it.
The operator applies it; the following sessions' ledger rows are the verdict.
