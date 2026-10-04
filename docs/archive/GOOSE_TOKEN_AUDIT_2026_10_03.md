# goose token audit, 2026-10-03

Read-only look at goose 1.52.0's own logs on the HP box
(`/root/.local/state/goose/logs/llm_request.*.jsonl`, 12 sessions, 2026-10-02/03).
Nothing was changed on the box. The TUI check that ran the same day is
[`../runbooks/goose-tui-amd64-local.md`](../runbooks/goose-tui-amd64-local.md).

## What the logs hold

One `input` record per session (the **first** request's full messages + tools),
then streamed `data` chunks; `usage` appears on a final chunk only some of the
time (10 of 12 files have it). So per-turn growth is not
logged — only the size of the prompt when each log opened.

## Findings

| Item | Measured |
|---|---|
| Input tokens, opening request (10 sessions with `usage`) | 82k – 96k (`usage.input_tokens`) |
| Prefix-cache hit on those requests | 94.7% – 99.9%, median ~99% (`cache_read_input_tokens` / `input_tokens`) |
| Static overhead: system prompt + 17 tool schemas | 4.3 KB + 13.9 KB ≈ 18 KB (~5% of a 358 KB prompt) |
| Biggest tool schemas | `delegate` 2.3 KB, `load` 1.4 KB, `read_image` 1.3 KB, `analyze` 1.3 KB |
| History in the sampled prompt (98 messages) | assistant 200 KB (whole-file `write` bodies, long shell heredocs), tool results 152 KB (`cat -n` of 400-line files, a whole design doc), user 1 KB |
| Output | thinking chunks outnumber text chunks ~4:1 (241 vs 61) at `thinking_effort: medium` |
| Model actually used | `glm-5.3-flash` in the logs; `config.yaml` says `kimi-for-coding` (the `goose-glm`/`goose-kimi` wrappers pick per run) |

**The cache is already doing the work.** Fresh (uncached) input is ~0.1–4.7k tokens
per request; the rest is billed/priced as cache read. Trimming the static prefix
buys little.

## Where there is still something to take

1. **Tool-result bulk, not schemas.** Three results alone were 19 KB, 19 KB and
   18 KB (whole-file reads). Reading ranges instead of whole files is the single
   largest lever and is a prompt/habit change, not a goose setting.
2. **Assistant-side bulk.** Whole-file `write` calls re-sent as history forever.
   Edits (`str_replace`) instead of rewrites shrink every later request.
3. **Compaction threshold.** At ~90k the sessions are far into context; lowering
   goose's auto-compact threshold (`GOOSE_AUTO_COMPACT_THRESHOLD`, unverified on
   1.52.0 — check `goose configure`) trades cache hits for a shorter prefix. It
   only wins if the cache-miss rate stays low, so A/B it on one long task.
4. **Unused platform extensions.** `apps`, `extensionmanager`, `scheduler`,
   `summon`, `tom`, `skills`, `analyze` are all enabled; only `developer` is used.
   Disabling the rest drops maybe 6–8 KB of schema (~2k tokens/request, cached).
   Small; do it for tidiness, not as the fix.
5. **Thinking effort.** `medium` → `low` for mechanical turns cuts output tokens,
   which are the uncached ones.

Not measured: wall-clock per turn on the box, or what fraction of turns were
compaction. A real before/after needs the `usage` field on every turn; goose only
sometimes logs it.
