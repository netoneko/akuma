# llama.cpp on Akuma

Static musl AArch64 build of [llama.cpp](https://github.com/ggerganov/llama.cpp) for running LLM inference on Akuma OS.

## Building

```bash
cd userspace
cargo build --release -p llama-cpp
```

This runs CMake cross-compilation via `build.rs` and produces a stripped static binary at `bootstrap/bin/llama-cli` (~8MB).

### Prerequisites

- `aarch64-linux-musl-gcc` / `aarch64-linux-musl-g++` (from [musl.cc](https://musl.cc) or `brew install musl-cross`)
- `cmake`

## Getting a model onto the disk

Download a small GGUF model and place it in the bootstrap directory before populating the disk:

```bash
# SmolLM2-135M-Instruct Q4_K_M (~105MB) -- best quality that fits in 256MB QEMU
curl -L -o bootstrap/model.gguf \
  "https://huggingface.co/QuantFactory/SmolLM2-135M-Instruct-GGUF/resolve/main/SmolLM2-135M-Instruct.Q4_K_M.gguf"

# Repopulate the disk image
./scripts/populate_disk.sh
```

For a quick smoke test, use Tiny-LLM (12MB):

```bash
curl -L -o bootstrap/model.gguf \
  "https://huggingface.co/aimlresearch2023/Tiny-LLM-Q5_K_M-GGUF/resolve/main/tiny-llm.Q5_K_M.gguf"
```

## Running inside Akuma

```bash
# Basic text generation
llama-cli -m /model.gguf -p "Hello, world!" -n 64 -t 2 --no-mmap

# Interactive chat
llama-cli -m /model.gguf -cnv -t 2 --no-mmap
```

### Important flags

| Flag | Why |
|------|-----|
| `--no-mmap` | Read model with `read()` instead of `mmap()` on the file. Required -- Akuma's VFS doesn't support file-backed mmap. |
| `-t 2` | Limit to 2 inference threads. Akuma has a 32-thread kernel pool shared with the OS. |
| `-n 64` | Number of tokens to generate. Keep low for testing. |
| `-cnv` | Conversational / chat mode. |

## Memory considerations

QEMU runs with 256MB RAM by default. After kernel overhead (~64MB code+stack+heap), ~192MB is available for userspace. With `--no-mmap`, the entire model is loaded into memory via `read()`.

| Model | Quant | Size | Fits in 256MB QEMU? |
|-------|-------|------|---------------------|
| Tiny-LLM (13M) | Q5_K_M | 12MB | Yes (smoke test) |
| SmolLM2-135M-Instruct | Q4_K_M | 105MB | Yes |
| SmolLM2-135M-Instruct | Q8_0 | 145MB | Yes (tight) |
| Qwen2.5-0.5B-Instruct | Q4_K_M | ~400MB | No (need `-m 1G`) |

To run larger models, increase QEMU RAM in `scripts/run.sh`:

```bash
-m 1G    # 1GB RAM (matches Graviton free tier target)
```

## Potential syscall issues

llama.cpp may use syscalls not yet implemented in Akuma. Common ones to watch for:

- `sched_getaffinity` (nr 123) -- thread pinning, safe to stub with `-ENOSYS`
- `sysinfo` (nr 99) -- memory info, safe to stub
- `prlimit64` (nr 261) -- resource limits, safe to stub
- `getrandom` (nr 278) -- should already be implemented

Check kernel output for `[SYSCALL] unhandled` messages and add stubs as needed.

## Build configuration

The CMake build disables everything except CPU inference:

- No GPU backends (CUDA, Metal, Vulkan)
- No OpenMP (llama.cpp has its own thread pool)
- No OpenSSL / curl
- No examples or tests
- Static linking with musl libc
- `--entry=_start` for Akuma ELF loading
