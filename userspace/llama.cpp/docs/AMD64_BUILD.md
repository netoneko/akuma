# Building llama.cpp for Akuma/amd64

**Status: works.** Verified 2026-09-20 under Firecracker on the trashcan's
Ubuntu host (`docs/reference/firecracker-amd64/`): `llama-server` loads a real
GGUF model, answers `/health`, and serves `/v1/chat/completions` — a full chat
completion came back with real token-generation timing
(~38 tokens/s decode, ~61-68 tokens/s prompt processing, `SmolLM2-135M-Instruct`
Q8_0). This is a **manual CMake build**, not yet wired into `build.rs`/
`Cargo.toml` the way the AArch64 build is — see "What is not done yet" below.

## Why this needed its own recipe, not just a `-march=` swap

The obvious first move — `apk add llama.cpp-cpu llama-server` on the guest and
run the Alpine-packaged binary — **wedged the whole kernel**, on bare metal and
identically under Firecracker (`docs/archive/AKUMA_AMD64_UD_CRASH_CONTAINMENT.md`).
The proximate cause was a ring-3 `#UD` (illegal instruction) that the kernel
had no way to contain at the time; the *root* cause, still true after that
containment fix landed, is that Alpine's package links against **OpenBLAS**,
which picks its SIMD kernel at **runtime** via CPUID dispatch — and whatever it
picked was an instruction this vCPU does not actually support. Even with the
kernel able to survive the crash and kill just that one process, the server
still never reached `listen()`.

`llama.cpp`'s own GGML CPU backend does the opposite: it decides which SIMD
extensions to use at **compile time**, from CMake flags, with no runtime
probing at all (see `GGML_NATIVE`/`GGML_AVX2`/etc. in
`llama.cpp/ggml/CMakeLists.txt`). Build with a conservative, explicit target
and there is no OpenBLAS in the picture and no dispatch that can guess wrong.
That is the whole fix: **pick the CPU baseline at build time and disable
BLAS**, which the AArch64 build in this same crate already does
(`-march=armv8.2-a+fp16+dotprod`, `-DGGML_BLAS=OFF`) — amd64 just needed its
own numbers.

## Toolchain

Ubuntu's `musl-tools`/`musl-dev` packages only give you `musl-gcc` — a **C**
wrapper around the system `gcc` with a musl specs file, no C++ support (no
`musl-g++`, and pointing `g++` at the same specs file fails immediately on
`<iostream>` — the musl sysroot Ubuntu ships has no libstdc++ headers staged
into it). llama.cpp is C++, so this needs a real cross toolchain with its own
libstdc++ built against musl:

```bash
curl -sL -o musl-cross.tgz https://musl.cc/x86_64-linux-musl-cross.tgz
tar xzf musl-cross.tgz     # -> x86_64-linux-musl-cross/bin/{x86_64-linux-musl-gcc,g++,...}
export PATH=$PWD/x86_64-linux-musl-cross/bin:$PATH
```

This is the same class of toolchain `docs/runbooks/stage-rust-toolchain-amd64.md`
and the AArch64 recipe's own `musl.cc`/`musl-cross` note already point at —
one prebuilt static archive, no package manager involved, works the same way
whether you're building *on* the trashcan's own Ubuntu (native x86_64 host,
cross-compiling only in the "different libc" sense) or cross-compiling from
somewhere else entirely.

## The submodule trap

`userspace/llama.cpp/llama.cpp` is a git submodule
(`https://github.com/netoneko/llama.cpp.git`, currently pinned to
`gguf-v0.18.0-59-g33827726f`). A `deploy()`-synced checkout on a box can have a
**stale, non-submodule directory** at that path left over from before it
became one (dated files, no `.git`) — `git submodule update --init` then fails
with "already exists and is not an empty directory" while giving no hint that
the fix is `rm -rf` that path first. Check `git submodule status
userspace/llama.cpp/llama.cpp`: a leading `-` means uninitialized regardless of
what files are sitting in the directory.

```bash
git submodule status userspace/llama.cpp/llama.cpp   # leading '-' = not initialized
rm -rf userspace/llama.cpp/llama.cpp                  # only if that '-' is there
git submodule update --init userspace/llama.cpp/llama.cpp
```

## Build

```bash
export PATH=/path/to/x86_64-linux-musl-cross/bin:$PATH
mkdir -p build-x86 && cd build-x86
cmake ../userspace/llama.cpp/llama.cpp \
  -DCMAKE_C_COMPILER=x86_64-linux-musl-gcc \
  -DCMAKE_CXX_COMPILER=x86_64-linux-musl-g++ \
  -DCMAKE_C_FLAGS=-march=x86-64 \
  -DCMAKE_CXX_FLAGS=-march=x86-64 \
  -DCMAKE_EXE_LINKER_FLAGS="-static -Wl,--entry=_start" \
  -DCMAKE_SYSTEM_NAME=Linux \
  -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
  -DCMAKE_BUILD_TYPE=Release \
  -DGGML_NATIVE=OFF \
  -DGGML_SSE42=OFF -DGGML_AVX=OFF -DGGML_AVX_VNNI=OFF -DGGML_AVX2=OFF \
  -DGGML_BMI2=OFF -DGGML_AVX512=OFF -DGGML_AVX512_VBMI=OFF \
  -DGGML_AVX512_VNNI=OFF -DGGML_AVX512_BF16=OFF -DGGML_FMA=OFF -DGGML_F16C=OFF \
  -DGGML_OPENMP=OFF -DGGML_BLAS=OFF -DGGML_CUDA=OFF -DGGML_METAL=OFF \
  -DGGML_VULKAN=OFF -DGGML_RPC=OFF -DBUILD_SHARED_LIBS=OFF \
  -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF \
  -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF

cmake --build . --target llama-server -j$(nproc)   # 3m24s wall on the trashcan's 4 cores
x86_64-linux-musl-strip bin/llama-server            # 23.5 MB -> 11.3 MB
```

`-march=x86-64` is the **baseline** x86_64 ISA — guaranteed on anything that
calls itself x86_64, nothing newer. Every `GGML_*` instruction-set flag is
explicitly `OFF` rather than left at its `GGML_NATIVE=OFF` default, because
that default is **not** "off" — `ggml/CMakeLists.txt`'s `INS_ENB` variable
flips several of them (`SSE42`/`AVX`/`AVX2`/`BMI2`/`FMA`/`F16C`) back to `ON`
when `GGML_NATIVE` is off, which is the opposite of what "native off" sounds
like it should mean. Leaving them at that default would have compiled AVX2
code paths in regardless of `-march=`, silently reproducing the exact
crash this recipe exists to avoid. `AVX512*` already defaults `OFF`
unconditionally and needs no override, but they're listed for the reader who
wants to widen the baseline later and needs the full knob list in one place.

The linker flags (`-static -Wl,--entry=_start`) are copied from the AArch64
recipe unchanged — this kernel's loader wants a static, non-dynamically-linked
ELF with an explicit entry symbol either way, and nothing here is
architecture-specific about that requirement.

## Running it

Same flags the AArch64 `README.md` documents (`--no-mmap` is required —
Akuma's VFS has no file-backed mmap; `-c` sized to the guest's RAM; `-t`
capped well under the kernel's thread pool). Verified:

```bash
llama-server -m /model.gguf --host 0.0.0.0 --port 8080 -c 512 -t 2 --no-mmap
```

```
main: model loaded
main: server is listening on http://0.0.0.0:8080
main: starting the main loop...
```

```bash
curl http://<guest>:8080/health
# {"status":"ok"}
curl http://<guest>:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"Say hello in exactly three words."}],"max_tokens":30}'
# {"choices":[{"finish_reason":"stop", ... "content":"Hello, welcome to ..."}],
#  "timings":{"predicted_per_second":37.86, "prompt_per_second":67.90, ...}}
```

Stable across repeated requests in the same session — not a one-shot fluke.

## What is not done yet

- **Not wired into `build.rs`/`Cargo.toml`.** The AArch64 path is a normal
  `cargo build -p llama-cpp` that drives CMake through `build.rs`; this amd64
  recipe is a hand-run CMake invocation outside that crate entirely, so it
  produces nothing under `bootstrap/bin/` and nothing `scripts/populate_disk.sh`
  or `amd64/mkdisk.sh` picks up automatically. Porting it means making
  `build.rs` branch on `target_arch` (or an env var/feature) for the compiler
  names, `-march=`, `CMAKE_SYSTEM_PROCESSOR`, and the strip binary name — all
  four differ between the two architectures and are currently hardcoded to
  AArch64's.
- **Only `llama-server` was built and tested.** `llama-cli`/`llama-bench` need
  the same CMake invocation with a different `--target`, exactly as the
  AArch64 build.rs already does for all three; no reason to expect them to
  behave differently, just not yet exercised on this target.
- **Baseline-only.** `-march=x86-64` with every SIMD extension off is the
  slowest correct configuration on purpose — it is the "does this work at
  all" baseline. Once that is proven (it is, as of this doc), the next step
  is finding out which extensions Firecracker's/the trashcan's actual vCPU
  genuinely supports (`/proc/cpuinfo` `flags` line, or QEMU's `-cpu` model)
  and re-enabling them one at a time, the same incremental-widening approach
  `docs/archive/AKUMA_AMD64_UD_CRASH_CONTAINMENT.md` recommends for the
  Alpine-packaged build this replaces.

## Background

- [`docs/archive/AKUMA_AMD64_UD_CRASH_CONTAINMENT.md`](../../../docs/archive/AKUMA_AMD64_UD_CRASH_CONTAINMENT.md)
  — the `apk`-packaged OpenBLAS build's crash this recipe sidesteps, and the
  separate kernel-level fix (`#UD` no longer wedges the whole guest) that
  landed the same session but does not by itself make that build serve
  traffic.
- [`../README.md`](../README.md) — the AArch64 build this recipe is the amd64
  twin of; same runtime flags, same linker requirements, different compiler
  and instruction-set knobs.
