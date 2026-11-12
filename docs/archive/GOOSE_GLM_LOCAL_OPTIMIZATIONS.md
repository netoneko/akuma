# goose + GLM-4.7-Flash locally on a Mac: what made it fast

Running `goose` against a locally served model on an M4 Pro (16-core GPU,
48 GB unified memory, ~273 GB/s), and what moved the needle. Source of truth for
the scripts is the `yolo` repo (`run-mlx.sh`, `run-ollama.sh`) and
`~/.local/bin/goose-glm-local`. This is unrelated to `scripts/kvm/goose/goose-glm-mac.sh`,
which drives the **z.ai cloud** GLM with its own config dir.

Date of measurements: 2026-11-12. Everything below is from **one benchmark
workload** and a handful of synthetic prompts; there was no quality evaluation
beyond reading the outputs. Treat the numbers as indicative, not definitive.

## Result

Benchmark task (read-only, multi-turn): *"Read yolo.sh, yolo.py, Dockerfile and
README.md, then write a 5-bullet summary of how they fit together."*

| Model / engine                               | goose task | notes                                              |
|----------------------------------------------|-----------:|----------------------------------------------------|
| Qwen3.8-27B 4-bit, MLX (goose 1.41, default) |      5:54  | starting point                                     |
| Qwen3.8-27B 4-bit, MLX (tuned)               |      2:51  | dense 27B: ~100 tok/s prefill, 9-15 tok/s decode   |
| Gemma 4 e4b, Ollama 0.40.16                  |      0:37  | ~4B effective params; first run skipped the Dockerfile |
| **GLM-4.7-Flash 6-bit, MLX (tuned)**         |  **0:42**  | 2 requests, 4 files read in parallel, complete summary |

GLM-4.7-Flash 6-bit on `mlx_lm.server`, cold first turn vs. a cached follow-up turn
(`bench_turns`-style: same prefix, new short user message):

| context | cold TTFT | cached follow-up | decode     |
|--------:|----------:|-----------------:|-----------:|
|   ~0.5k |     1.0 s |            0.2 s |  45 tok/s  |
|     ~8k |    20 s   |            0.9 s |  33 tok/s  |
|    ~32k |   159 s   |            3.3 s | 16.5 tok/s |

Both prefill (~400 tok/s at 8k, ~200 at 32k) and decode degrade with context, so
**~32k is a sensible working budget**. GLM wires ~27 GB; it leaves little room
for a second large model.

## What actually helped (in order of impact)

### 1. Use a model with few active parameters

Prefill is compute-bound, decode is memory-bandwidth-bound. A dense 27B does
~54 GFLOP per prompt token: at the measured ~90-100 tok/s that is ~5 TFLOP/s,
which looks close to what this GPU can do (my estimate of peak is ~6-7 TFLOP/s;
as far as I know the M4 GPU has no matrix units). Decode must read ~14 GB of
weights per token against ~273 GB/s, i.e. a ~19 tok/s ceiling (measured 9-15).
GLM-4.7-Flash is a ~31B MoE with ~3B active parameters, which is why it is
roughly 3-6x faster at prefill and 1.4-4.5x faster at decode than the dense
27B, with the advantage shrinking as context grows (decode is only ~1.4x at 32k).

Dense Qwen3.8-27B cold prefill: 8k = 116 s, 32k = 451 s. GLM: 20 s and 159 s.

### 2. Keep the prompt prefix stable (prefix cache)

`mlx_lm.server` reuses a cached prefix only if the new prompt extends a stored
sequence exactly. Anything that rewrites earlier messages forces a full
re-prefill.

- **goose <= 1.41:** rewrote the *first user message* every request with a
  `<turn-context>` block containing `<current-time>` (minute resolution) plus
  extension notes. Whenever the minute ticked over, cached tokens fell back to
  just the system prompt (4,504 of 11,118 in one trace) and the whole history
  was re-prefilled. Local turns take 15-120 s, so this hit constantly.
- **goose 1.59.0** persists the block as its own message and freezes the time
  for the turn; earlier blocks stay in history. In traces, every request's
  cached count equalled the previous request's input + output (e.g. 4,723 =
  4,621 + 102), including across prompts in one session. **Upgrade goose**
  (`brew upgrade block-goose-cli`).
- In goose source, the whole turn-context block is skipped when the model's
  context limit is under `MIN_CONTEXT_FOR_MOIM = 32_000`. That is an
  alternative on old versions, at the cost of a <32k context.
- A tiny pass-through proxy that rewrote `<current-time>` to a constant also
  worked (`yolo/prefix-proxy.py`, not needed on 1.59).

### 3. Make goose's own prompt small

goose sent 18 tools (~14k of ~18.7k prompt characters). Cold prompt 4,883
tokens -> 1,956 tokens, cold start ~56 s -> ~32 s, in `~/.config/goose/config.yaml`:

- `enabled: false` for `apps`, `extensionmanager`, `summon`, `skills`, `analyze`
- `developer.available_tools: [shell, edit, write, tree]` (drops `read_image`)
- `GOOSE_DISABLE_SESSION_NAMING: true`, `GOOSE_DISABLE_TOOL_CALL_SUMMARY: true`
  (each is an extra model call that ties up a single-stream local server)

With 18 tools Qwen kept calling `read_image` to read text files (4 wasted calls,
one extra round trip). `todo` and `tom` are fine to leave on with goose 1.59
(verified: cache chain intact); they were only harmful on 1.41.

`load_skill` still showed up after disabling `skills`; I did not find which
extension provides it.

### 4. mlx-lm: upgrade, and know the gotchas

- **mlx-lm 0.31.3 bug (Qwen3.5-architecture hybrid model):** a request that hit
  the prefix cache returned tool calls with empty arguments (`{}`), so goose
  looped on "missing field `command`". Identical request cold: correct;
  with `cached_tokens` > 0: broken. **Fixed in mlx-lm 0.32.2.** All GLM runs
  were on 0.32.2, so I never saw it with GLM.
- **`mlx_lm.server` loads whatever `model` a request names.** A sanity curl with
  the wrong model name silently swapped the server to Qwen. Clients must send
  the loaded model's exact name (`goose-glm-local` sets `GOOSE_MODEL`).
- **`--prefill-step-size`:** on Qwen3.8-27B, cold 6k-token prompt: 1024-2048 ~100
  tok/s, 4096 ~95, 8192 ~83 (bigger is slower; compute-bound). `run-mlx.sh`
  defaults to 2048 (`PREFILL_STEP=` overrides). **Re-checked on GLM-4.7-Flash 6-bit**
  (cold 6k prompt, second trial after warm-up): 512 ~372, 1024 ~414, **2048 ~424**,
  4096 ~405, 8192 ~371 tok/s. 2048 is best for GLM too; the spread between
  1024 and 4096 is only ~5%. The first request after a server start is ~25% slower
  (warm-up) at every step size.
- **Cache caps:** `--prompt-cache-size 4 --prompt-cache-bytes 8589934592`. The
  default cache grew to 12 GB at 32k with Qwen; with GLM's 27 GB of weights an
  uncapped cache is a risk in 48 GB.
- **Thinking off** (`--chat-template-args '{"enable_thinking":false}'`, exposed
  as `CHAT_TEMPLATE_ARGS` in `run-mlx.sh`): *worse* on Qwen (4:05 vs 3:18; sloppier
  tool choices). Left off by default.
- The model's multi-token-prediction head was not usable (no MTP support found
  in mlx-lm 0.32.2), so no speculative decoding.

### 5. Ollama: upgrade it (0.20.2 -> 0.40.16)

On 0.20.2 the Metal flash-attention path collapsed with context. Gemma 4 e4b,
Q4_K_M, `num_ctx` 32768, ~21.6k-token prompt:

| Ollama | KV cache | flash attn | prefill | decode  |
|--------|----------|-----------:|--------:|--------:|
| 0.20.2 | q8_0     |         on |  91 t/s | 21.5 t/s |
| 0.20.2 | f16      |        off | ~500 t/s | ~35 t/s |
| 0.40.16| f16      |        off | 870 t/s | 37.6 t/s |
| 0.40.16| q8_0     |     **on** | **849 t/s** | **41.8 t/s** |

On 0.40.16 the original `q8_0` + flash attention is best again. `num_batch`
512 vs 2048 made no meaningful difference. GLM-4.7-Flash on 0.20.2 showed the
same pattern (prefill 476 -> 204 tok/s, decode 29 -> 12 tok/s at 0.6k -> 8.6k).
**Not tested: GLM-4.7-Flash on Ollama 0.40.16** (I deleted the model to free
disk), so there is no fair Ollama-vs-MLX comparison for GLM yet.

vLLM on macOS means `vllm-metal`, which runs through MLX, so it is the same
engine family as `mlx_lm.server`; its batching/paged-KV strengths target
concurrent users. Not tried. `llama-server` on the same GGUF was also not tried.

## How to run it

```sh
# one-time: model download is 24.4 GB (~22.7 GiB); mlx-lm >= 0.32.2 in yolo/.venv-mlx
brew upgrade block-goose-cli ollama          # goose >= 1.59, ollama >= 0.40

goose-glm-local                 # = goose session, starts the MLX server if needed
goose-glm-local run --no-session -t "..."   # any goose args pass through
GOOSE_GLM_RESTART=1 goose-glm-local ...     # swap away a different running model
```

`goose-glm-local` only sets env vars (provider `openai`, `OPENAI_HOST=http://127.0.0.1:8080`,
`GOOSE_MODEL`, `GOOSE_CONTEXT_LIMIT=32768`); it never edits goose's config.
`run-mlx.sh` now defaults to `lmstudio-community/GLM-4.7-Flash-MLX-6bit`.

Download tip: if `hf download` sits at 0 bytes, `HF_HUB_DISABLE_XET=1` fixed it
here. Restarting a stalled download did **not** reuse partial files.

## Open items

- Quality was only eyeballed on one summarization task; no coding-task eval.
- Try a 4-bit GLM (16.9 GB) for speed/memory; not downloaded or compared.
- GLM-4.7-Flash on Ollama 0.40.16 for the engine comparison; `llama-server` on
  the same GGUF.
- `goose-glm-mac.sh` (z.ai cloud) is a separate path and was not touched.
