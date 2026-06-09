# needle-server

HTTP inference server for the Needle function-routing model.  It exposes an
OpenAI-compatible `/v1/chat/completions` endpoint so `meow` can use it as a
local provider without a full LLM.

## Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Liveness check |
| POST | `/v1/chat/completions` | OpenAI-compatible chat completions (used by meow) |
| POST | `/v1/route` | Raw tool-routing query |
| POST | `/v1/retrieve` | Contrastive tool retrieval (requires contrastive head) |

## Usage

```sh
needle-server --weights /models/needle.safetensors \
              --vocab   /models/vocab.txt \
              --port    11434 \
              [--debug]
```

Configure meow to point at it:

```
[provider:needle]
base_url=http://127.0.0.1:11434
```

Then: `meow --provider needle --model needle`

## --debug flag

Pass `--debug` to log each request body, extracted query, tools count, and
engine result to stdout.  Also logs parse failures with the raw body excerpt
so mis-routed requests are immediately visible.

meow's `--debug` flag (non-TUI / `-c` mode) logs the request URL, DNS steps,
HTTP status line, and response body on errors.

## Memory requirements and the 100 MB OOM crash

**Symptom:** `needle-server` crashes with SIGSEGV after ~0.91s on a 100 MB
QEMU instance, before serving any request.  No error message is printed.

**Root cause — double allocation during load:**
`SafeTensors::load` called `libakuma::fs::read(path)` which allocates one
`Vec<u8>` for the full file, then `from_bytes` immediately called
`.to_vec()` on the data slice, producing a *second* full copy.  Peak RAM
during load was ≈ 2× the `.safetensors` file size, plus all the decoded
`Vec<f32>` tensors on top of that.  On 100 MB (81 MB user-available) this
silently exhausted the heap, the allocator returned a null pointer, and
dereferencing it caused SIGSEGV.

**Fixes applied (2026-06-09):**

1. **Eliminated the second copy** — `from_bytes` now keeps the original
   buffer and adjusts each `TensorMeta` offset by `header_end` so the data
   section is addressed in-place.  Peak allocation during load is now 1×
   the file size instead of 2×.

2. **Pre-flight size check** — `SafeTensors::load` stats the file before
   allocating.  If the file exceeds `MAX_SAFETENSORS_BYTES` (60 MB by
   default), it returns a `ParseError` with a human-readable message like:
   > `model file too large: 72 MB (limit 60 MB, increase MAX_SAFETENSORS_BYTES or add more RAM)`
   This replaces the silent SIGSEGV with a clean exit(1).

**Minimum recommended RAM:** 256 MB for the default
`Abdalrahman/needle-rs-safetensors` weights (~15 MB file → ~30 MB loaded +
tensor copies).  The 100 MB instance is too small for a loaded model plus
the OS kernel overhead.  Use `MEMORY=256 cargo run --release` or higher.

## Streaming compatibility with meow

meow sends `"stream":true` in all `/v1/chat/completions` requests and
expects SSE (`data: {...}\n\ndata: [DONE]\n\n`) in the response.  Before the
fix, needle-server returned a plain JSON response; meow's SSE parser found
no `data:` lines and silently produced an empty response.

**Fix:** `handle_chat_completions` now checks `req.stream`.  When true, it
wraps the engine result in SSE format using
`write_completions_streaming_response`:

- Tool call result → `data: {...,"finish_reason":"tool_calls",...}` + `data: [DONE]`
- Plain text result → `data: {...,"delta":{"content":"..."},...}` + `data: [DONE]`

## tools JSON parsing with meow's compact schema

meow sends the full `OPENAI_TOOLS_JSON` constant (≈ 2.8 KB, 17 tools,
minified single-line JSON) in every chat completions request.

`parse_completions_request` previously used `json_extract_raw(body, "tools")`
which searched linearly from the start of the body.  If any message content
contained the literal string `"tools":` (e.g. a tool result whose output
included JSON), the search would match inside the messages array instead of
the top-level `tools` key.

**Fix:** `extract_top_level_tools` skips past the `messages` array extent
before searching for `"tools":`, ensuring the compact tools schema from meow
is always extracted from the correct position.

## Performance (inference latency)

The 100 MB run never completed model load, so no inference timing data
exists from `100mb_needle_server.log`.  The log shows the server crashed at
T≈0.91s every attempt, consistent with an OOM during
`alloc::vec![0u8; file_size]`.

Once the memory fixes are in place, expect latency to be dominated by
the forward pass through the encoder+decoder layers.  The `[needle] done in
Xms` log line printed after each request records wall-clock inference time.

## needle is a function-router, NOT a chat model

**This is the most important thing to understand about needle-server.**  The
Needle model has **no text-generation path**.  Every forward pass emits a JSON
array of tool calls — `[{"name":...,"arguments":{...}}]` — or an empty array
`[]`.  It can never produce conversational text like "Hi! nya~".  Wiring it
behind `/v1/chat/completions` makes it *look* like a chat provider, but it can
only ever route to one of the supplied tools.

Verified on the host with the upstream `needle-cli` (`needle-rs`) binary using
the exact `bootstrap/models/needle.safetensors` weights + `vocab.txt` and
meow's exact 2788-byte 17-tool `compact-tools` schema:

| query | output |
|-------|--------|
| `create a folder called lambda.md` | `[{"name":"FolderCreate","arguments":{"path":"lambda.md"}}]` ✓ |
| `read the file config.toml` | `[{"name":"FileReadLines","arguments":{"filename":"config.toml"}}]` ✓ |
| `what is the meaning of life` | `[]` (no tool) |
| `tell me a joke` | `[]` (no tool) |
| `say hi` | `[{"name":"FolderCreate","arguments":{"path":"function":"function":"function":"lambda.md"}}]` ✗ |
| any query, **empty** tools | `[]` |

For genuine tool requests it routes correctly.  For conversational input it
either returns `[]` or is forced to pick a tool it doesn't need and degenerates.

### The `"function":"function":"function"` output is NOT truncated

The malformed `{"path":"function":"function":"function":"lambda.md"}` seen in
the QEMU logs is the **genuine, complete model output**, reproduced
deterministically on the host and visible token-by-token in `--stream` mode.
It is not a buffer cut-off, a JSON parse error, or a QEMU artifact.  It is an
out-of-distribution failure: a router trained to emit tool calls, handed a
greeting, picks the nearest tool and falls into a repetition loop on the
highest-frequency token in its input — `"function"` appears 17× in the tools
schema meow sends.

### The personality / system prompt never reaches needle

meow's `parse_completions_request` → `extract_last_user_message` sends only the
**last `user` message** as the query.  The system/personality block (e.g. the
"Meow-chan" cyberpunk-catgirl prompt) is dropped before the request is built,
so it cannot influence needle's output even in principle.

**Implication:** if conversational replies are wanted, needle is the wrong
model for the chat turn.  Use needle only as a *tool-detection* layer and route
non-tool turns to a real generative model (e.g. the `qwen3.5-0.8b-q4.gguf` in
`/models` via llama.cpp), or serve chat from the generative model directly.

## Inference is slow because of the environment, not the model

The `[needle] done in 432499ms` line (≈ 432 s for `"say hi"` + 17 tools) is
**~117–1700× slower than native**.  Same weights, same prompt, measured on the
host with `needle-cli`:

| environment | empty tools | 17-tool schema |
|-------------|-------------|----------------|
| native host (`needle-rs`) | 0.245 s | 3.7 s |
| akuma in QEMU | — | ~432 s |

The model itself is fast.  The slowdown is the runtime, in rough order of
impact:

1. **Pure TCG software emulation.**  `scripts/cargo_runner.sh` runs
   `qemu-system-aarch64 -machine virt -cpu max` with **no `-accel hvf`** and a
   single vCPU.  Every guest AArch64 float op is interpreted on the host.
2. **NEON disabled in the kernel build.**  `needle-core` has a `matvec_neon`
   SIMD path (used on the host) but the no_std kernel build falls back to
   `matvec_scalar`.
3. **Cache-hostile matmul.**  `needle-core::ops::matmul` strides the weight
   matrix by `out_dim` in the inner loop (`w[i * out_dim + j]`), column-walking
   a row-major matrix.
4. **The 17-tool blob dominates `enc_len`.**  Empty tools → 0.245 s, full
   schema → 3.7 s on host (15×); the 2.8 KB tools text saturates `enc_len`
   toward `max_enc_len` (1024) and encoder attention is ~O(enc_len²·d).

Note the weights are **I4-quantized** (4-bit) with F32 scales, so each matmul
also pays a dequant step.  Model config falls back to `TransformerConfig`
defaults (d_model=512, 12 enc + 8 dec layers, vocab 8192, max_enc_len=1024,
max_dec_len=512) because the `.safetensors` `__metadata__` is empty.

### meow times out long before needle finishes

meow's streaming reader (`api/client.rs`) aborts with `Err("Timeout")` after
`read_attempts > 6000` consecutive `WouldBlock` polls (~6 s of no bytes), and
the counter only resets when actual bytes arrive.  Because
`handle_chat_completions` uses the **non-streaming** `engine.run()` and sends
nothing for the entire ~432 s forward pass, meow gives up after ~6 s.  Even
per-token SSE streaming would not fix this on its own: the encoder pass is a
single monolithic call that produces no tokens, so the first byte still
arrives many seconds in.  A real fix needs faster inference (HVF accel) and/or
a larger meow read timeout for slow local providers.
