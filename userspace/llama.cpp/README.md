# llama.cpp on Akuma

Static musl AArch64 build of [llama.cpp](https://github.com/ggerganov/llama.cpp) for running LLM inference on Akuma OS. This works — inference runs end-to-end on Akuma with SmolLM2-135M.

## Building

```bash
cd userspace
cargo build --release -p llama-cpp
```

This runs CMake cross-compilation via `build.rs` and produces a stripped static binary at `bootstrap/bin/llama-cli`. The build uses `-march=armv8.2-a+fp16+dotprod` to enable NEON SIMD and dot-product instructions for GGML's optimized AArch64 kernels.

### Prerequisites

- `aarch64-linux-musl-gcc` / `aarch64-linux-musl-g++` (from [musl.cc](https://musl.cc) or `brew install musl-cross`)
- `cmake`

## Getting a model onto the disk

Download a small GGUF model and place it in the bootstrap directory before populating the disk:

```bash
# SmolLM2-135M-Instruct Q4_K_M (~105MB)
curl -L -o bootstrap/model.gguf \
  "https://huggingface.co/QuantFactory/SmolLM2-135M-Instruct-GGUF/resolve/main/SmolLM2-135M-Instruct.Q4_K_M.gguf"

# Repopulate the disk image
./scripts/populate_disk.sh
```

## Running inside Akuma

```bash
# Conversational mode (recommended)
llama-cli -m /model.gguf -cnv -t 2 --no-mmap -c 256

# One-shot text generation
llama-cli -m /model.gguf -p "Hello, world!" -n 64 -t 2 --no-mmap -c 256
```

### Important flags

| Flag | Why |
|------|-----|
| `--no-mmap` | Required. Akuma's VFS doesn't support file-backed mmap, so the model is loaded via `read()`. |
| `-c 256` | Required for 256MB RAM. Limits context window to reduce KV cache from ~70MB to ~2MB. |
| `-cnv` | Conversational / chat mode. Applies SmolLM2's chat template automatically. Without this, output will be gibberish. |
| `-t 2` | Limit inference threads. Akuma has a 32-thread kernel pool shared with the OS. |
| `-n 64` | Number of tokens to generate (for one-shot mode). |

## Memory budget (256MB QEMU)

After kernel overhead (~33MB), about 228MB is available for userspace.

| Component | Size |
|-----------|------|
| llama-cli binary + runtime | ~15MB |
| SmolLM2-135M Q4_K_M weights | ~105MB |
| KV cache (`-c 256`) | ~2MB |
| Scratch buffers | ~5MB |
| **Total** | **~127MB** |

To run larger models, increase QEMU RAM in `scripts/run.sh`:

```bash
-m 1G    # 1GB RAM (matches Graviton free tier target)
```

## Build configuration

The CMake build disables everything except CPU inference:

- NEON SIMD + FP16 + dot-product enabled (`-march=armv8.2-a+fp16+dotprod`)
- No GPU backends (CUDA, Metal, Vulkan)
- No OpenMP (llama.cpp has its own thread pool)
- No OpenSSL / curl
- Static linking with musl libc
- `--entry=_start` for Akuma ELF loading
