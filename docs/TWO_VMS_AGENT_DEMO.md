# Two-VM Agent Demo: meow + llama-server

This document captures the full setup, lessons learned, and operational notes for
running the two-VM meow agent workflow (acceptance test 03).

## What This Demo Does

- **llama VM** (4 GB RAM, INSTANCE=1): runs `llama-server` serving the
  Qwen3.5-0.8B-Q4 model via an OpenAI-compatible HTTP API on port 8080.
- **meow VM** (128 MB RAM, INSTANCE=0): runs the `meow` AI agent, which
  connects to the llama VM and compiles `hello.c` with `tcc`.
- **Fixed inter-VM IP**: `10.0.2.2:8180` from inside any VM always reaches
  the llama VM's port 8080 through QEMU's SLIRP user-networking layer.

## Port Map (Fixed, Deterministic)

| VM        | INSTANCE | SSH (host) | HTTP (host) | VM port |
|-----------|----------|------------|-------------|---------|
| meow VM   | 0        | 2222       | 8080        | 22 / 8080 |
| llama VM  | 1        | 2322       | **8180**    | 22 / 8080 |

QEMU SLIRP formula: `host_port = base_port + 100 * INSTANCE`  
meow config: `base_url=http://10.0.2.2:8180` → routes through host → llama VM:8080.

## Quick Start

```bash
# Build llama-server (first time only — takes ~20s):
cd userspace && cargo build --release -p llama-cpp && cd ..

# Launch both VMs + print instructions:
scripts/run_two_vms.sh

# Or skip rebuild if already built:
scripts/run_two_vms.sh --skip-build
```

## Disk Layout

| Disk                      | Size   | Contents |
|---------------------------|--------|----------|
| `tmp/two_vms/llama.img`   | 1.8 GB | full bootstrap + 508 MB GGUF model + llama-server |
| `tmp/two_vms/meow.img`    | 1 GB   | bootstrap without model, tcc + libtcc1.tar (musl via `apk add musl-dev`), base_url patched to :8180 |

`run_two_vms.sh` creates both disks from `bootstrap/` via Docker ext2 mounts.
The meow disk strips `.gguf` files to save space.

## Key Binaries in bootstrap/bin

| Binary         | Role |
|----------------|------|
| `llama-server` | OpenAI-compatible LLM inference server (static AArch64 musl, 9.8 MB) |
| `llama-cli`    | Interactive CLI for the same model |
| `meow`         | AI agent binary |
| `tcc`          | Tiny C Compiler (required for the hello.c compilation task) |
| `apk`          | Alpine package manager (for installing additional tools at runtime) |

Both `llama-cli` and `llama-server` are built by `userspace/llama.cpp/build.rs`
using the CMake cross-compilation pipeline targeting `aarch64-linux-musl`.

## llama-server Invocation

```bash
llama-server \
  --model /qwen3.5-0.8b-q4.gguf \
  --host 0.0.0.0 \
  --port 8080 \
  --no-mmap \
  --chat-template chatml \
  -c 2048
```

**Required flags:**
- `--no-mmap`: Akuma's VFS does not support file-backed mmap; the model is
  loaded entirely via `read()` syscalls (~90 seconds for 508 MB).
- `--chat-template chatml`: The Qwen3.5 model embeds a Jinja2 chat template
  that llama-server's built-in parser cannot handle. Using `chatml` as an
  explicit override fixes `"Failed to parse input at pos 20"` errors from the
  `/v1/chat/completions` endpoint. The raw `/completion` endpoint works without
  this flag.

**Health endpoint:** `GET /health` returns `{"status":"ok"}` when ready,
`503 Loading model` while loading.

## meow Configuration (on the meow disk)

```ini
current_provider=ollama
current_model=qwen3:4b

[provider:ollama]
base_url=http://10.0.2.2:8180   # patched from :11434 by run_two_vms.sh
type = openai
```

The `type = openai` key is ignored by meow's parser (only `base_url` and
`api_key` are recognized). llama-server accepts any model name in the request
and uses the loaded model regardless.

## tcc Sysroot

tcc requires its musl libc headers and `libtcc1.o` at compile time. These are
distributed as archives on the disk:

| File                    | Purpose |
|-------------------------|---------|
| `apk add musl-dev`      | musl libc + CRT objects + POSIX headers (from Alpine apk) |
| `/archives/libtcc1.tar` | `libtcc1.a` helper objects + tcc internal headers (~60 KB) |

The meow agent needs these before compiling with tcc (or the acceptance test
should do it beforehand):

```bash
apk add musl-dev && cd / && tar xf /archives/libtcc1.tar
```

## Performance Characteristics (QEMU TCG, Apple Silicon)

All measurements taken on an M-series Mac running QEMU in TCG software
emulation mode (HVF acceleration is incompatible — see below).

| Metric | Value |
|--------|-------|
| Model load time (508 MB, --no-mmap) | ~90 seconds |
| Prompt processing speed | ~0.58 tokens/second |
| Token generation speed | ~1.2 tokens/second |
| Estimated time per LLM call (1500-token prompt + 100-token response) | ~45 minutes |
| Full meow task (3–5 LLM calls) | **2–4 hours** |

**Why so slow?** QEMU TCG (Tiny Code Generator) software-emulates every AArch64
instruction on the host CPU. Even on Apple Silicon (AArch64 host), QEMU does
not run the guest natively without explicit acceleration.

## HVF Acceleration (WORKING — default on Apple Silicon since 2026-06-09)

QEMU's Apple Hypervisor.framework backend (`-accel hvf`) now runs Akuma at
near-native AArch64 speed. `scripts/cargo_runner.sh` enables it by default on
Apple Silicon (auto-detected; `HVF=0` forces TCG). Measured ~70× prompt and ~100×
generation speedup on stories15M vs TCG (see docs/QEMU_HVF_ISV_BUG.md for the full
table and root-cause writeup).

Getting there required three Akuma-side fixes, each masked by TCG's lack of real
hardware behavior:

1. **GICv3 driver** (`src/gic_v3.rs`, now default; GICv2 behind the `gic-v2`
   feature). HVF presents GICv3, whose CPU interface is system-register based; the
   old GICv2 MMIO model triggered the `(isv)` assertion in `hvf_handle_exception`.
2. **Virtual timer** (`CNTV`/PPI 27) for preemption — the physical timer (`CNTP`)
   is trapped under HVF.
3. **I-cache maintenance via the kernel alias** in the demand-pager — `IC IVAU` on
   a not-yet-mapped user VA translation-faulted on real hardware.

The earlier guess that the `(isv)` assertion came from NEON `STP`/`LDP` in the
exception handlers was incorrect.

## 64 MB meow VM Crash

The meow VM crashes with a kernel stack overflow when run with `MEMORY=64M`:

```
WARNING: Kernel SP outside thread's stack bounds!
Heap: 7644781/8388608 bytes used (41297 allocs, peak=7644781)
PMM: 7867/16384 pages free (31468 KB / 65536 KB)
```

The crash is a **kernel panic**, not a user process crash — the kernel's own
thread stack overflows when the heap is nearly full, and there is no in-kernel
recovery mechanism. Minimum working meow VM size: **128 MB**.

## Known Issues

1. **Slow inference**: TCG-only until HVF is fixed. See above.
2. **No `<think>` stripping**: Qwen3 outputs `<think>...</think>` reasoning
   tokens in the content field. meow passes these through verbatim to tool-call
   parsing. In practice this works because meow searches for `tool_calls` in
   the SSE delta fields, not in the content.
3. **SSH server drops long connections**: Akuma's SSH server may drop
   connections that are idle during prompt processing. Use
   `-o ServerAliveInterval=30 -o ServerAliveCountMax=60` in the ssh client.
4. **apk shell pipeline limitation**: The Akuma shell does not support `|` pipe
   or `&` background operators for SSH-exec commands. Run `llama-server` via a
   dedicated persistent SSH session rather than with `&`.

## Troubleshooting

```bash
# Check llama-server health from host:
curl http://localhost:8180/health

# Check llama-server is reachable from meow VM:
ssh -p 2222 root@localhost "curl http://10.0.2.2:8180/health"

# Test a quick inference from host:
curl -s http://localhost:8180/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"local","messages":[{"role":"user","content":"hi"}],"max_tokens":5}'

# Check running processes on meow VM:
ssh -p 2222 root@localhost ps

# View meow VM crash log:
tail -50 logs/two_vms/test*/meow.log

# View llama VM inference progress (look for sendto count increasing):
grep PSTATS logs/two_vms/test*/llama.log | tail -3
```
