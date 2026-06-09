# needle-server

A no_std HTTP inference server for [Needle](https://github.com/geekgineer/needle-rs) — a 26M-parameter transformer that routes user queries to function calls without a cloud API.

Runs on Akuma OS as a bare-metal AArch64 binary. Weights are downloaded from the Cactus API or HuggingFace on first run.

## Usage

Quickstart: 
```
needle-server --weights /models/needle.safetensors --vocab /models/vocab.txt --port 11434
```

```
needle-server [OPTIONS]

OPTIONS:
  --port <PORT>        HTTP port to listen on          [default: 8080]
  --weights <PATH>     Path to .safetensors weights file
  --vocab <PATH>       Path to vocabulary text file
  --weights-dir <DIR>  Directory to cache downloaded weights [default: /data/needle]
  --download           Download missing weights from Cactus API on startup
  --model <ID>         HuggingFace model ID to download  [default: Abdalrahman/needle-rs-safetensors]
  --hf-token <TOKEN>   HuggingFace token for gated models

EXAMPLES:
  # Serve with pre-downloaded weights
  needle-server --weights /data/needle.safetensors --vocab /data/vocab.txt

  # Auto-download weights from Cactus API, then serve
  needle-server --download --weights-dir /data/needle

  # Custom port
  needle-server --download --port 9090

  # Gated HuggingFace model
  needle-server --download --model org/model --hf-token hf_xxx
```

## API

### `GET /health`

```json
{"status":"ok","model":"needle","loaded":true}
```

### `POST /v1/route`

Routes a natural language query to the best matching tool call.

**Request:**
```json
{
  "query": "What's the weather in Paris?",
  "tools": [
    {
      "name": "get_weather",
      "description": "Get current weather for a location",
      "parameters": {
        "type": "object",
        "properties": {
          "location": {"type": "string"},
          "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
        },
        "required": ["location"]
      }
    }
  ],
  "stream": false
}
```

**Response `200 OK`:**
```json
{
  "tool_call": [{"name":"get_weather","arguments":{"location":"Paris","unit":"celsius"}}],
  "latency_ms": 280
}
```

**Streaming (`"stream": true`)** — newline-delimited JSON, one token per line, final `done` frame:
```
{"token":"get"}
{"token":"_weather"}
{"done":true,"tool_call":[{"name":"get_weather","arguments":{"location":"Paris"}}]}
```

### `POST /v1/retrieve`

Ranks tools by cosine similarity to a query using the model's contrastive head. Returns an empty result if the loaded weights don't include a contrastive head.

**Request:**
```json
{
  "query": "book a flight to Tokyo",
  "tools": ["book_flight", "get_weather", "send_email"],
  "top_k": 3
}
```

**Response:**
```json
{
  "results": [
    {"name": "book_flight", "score": 0.9412},
    {"name": "get_weather", "score": 0.2103},
    {"name": "send_email",  "score": 0.0891}
  ]
}
```

### `GET /openapi.json`

`501 Not Implemented` — stub placeholder.

## Weight downloading

On startup with `--download`, needle-server:

1. Queries `https://www.cactuscompute.com/api/models` to discover download URLs for the requested model.
2. Falls back to direct HuggingFace CDN (`https://huggingface.co/<model>/resolve/main/`) if the model isn't listed in the Cactus API.
3. Downloads `needle.safetensors` and `vocab.txt` into `--weights-dir` via HTTPS.
4. Subsequent startups skip the download if both files already exist.

Pass `--hf-token` (or set it once in your workflow) for models behind a HuggingFace access gate.

## Architecture

needle-server is fully `no_std` and targets `aarch64-unknown-none` — the same bare-metal target as every other Akuma userspace binary. It links against:

- **libakuma** — syscall wrappers, memory allocator, TCP sockets
- **libakuma-tls** — HTTPS client (TLS 1.3 via embedded-tls) for weight download
- **needle-core** — the actual transformer compute kernel (already no_std)

The inference stack from [needle-infer](https://github.com/geekgineer/needle-rs/tree/main/crates/needle-infer) (tokenizer, SafeTensors parser, constrained decoder, engine) is vendored directly into `src/` with minimal changes: `std::collections::HashMap` replaced by `alloc::collections::BTreeMap`, `std::fs` replaced by `libakuma::fs`, and `std::io::Error` replaced by a local `ParseError` type.

## Build

```bash
# From the userspace/ workspace root:
cargo build --release -p needle-server

# Or via the full build script:
./build.sh --needle-server-only
```

The workspace release profile already applies `opt-level = "z"`, `lto = true`, `strip = true`, targeting a binary under 600 KB.

## License

MIT — see [LICENSE](LICENSE).

This server vendors and adapts code from [needle-rs](https://github.com/geekgineer/needle-rs)
(MIT, © 2026 Abdalrahman Ibrahim). The files `src/tokenizer.rs`, `src/constrained.rs`,
`src/engine.rs`, and `src/safetensors.rs` are ported from its `crates/needle-infer` with
minimal `no_std` changes, and `needle-core` is used as an upstream dependency. The Needle
transformer model and weights served by this binary are by Cactus Compute
(Henry Ndubuaku et al.), also MIT-licensed.
