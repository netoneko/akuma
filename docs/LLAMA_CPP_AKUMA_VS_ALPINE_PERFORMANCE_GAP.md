# llama.cpp Performance: Akuma OS vs Alpine Linux

## Observed numbers

| Environment | Prompt (t/s) | Generation (t/s) |
|-------------|-------------|-------------------|
| Alpine Linux (QEMU, HVF) | 435.4 | 325.0 |
| Akuma OS (QEMU, TCG) | 1.7 | 0.1 |
| **Ratio** | **~256x** | **~3250x** |

Same binary (`llama-cli`, static musl, `-march=armv8.2-a+fp16+dotprod`), same model (SmolLM2-135M-Instruct Q4_K_M), same host (Apple Silicon Mac).

## Root cause: QEMU TCG vs HVF

The dominant factor is **CPU emulation mode**, not the kernel.

### HVF (Hardware Virtualization Framework)

Alpine was tested with `-accel hvf`. Under HVF, QEMU delegates execution to Apple Silicon's hardware virtualization extensions. Guest AArch64 instructions — including all NEON/ASIMD/FP16/dot-product operations — run at native speed on the host CPU's execution units. The hypervisor only intervenes on VM exits (MMIO, interrupt injection, etc.).

### TCG (Tiny Code Generator)

Akuma runs via `cargo run --release`, which uses the runner defined in `.cargo/config.toml` — this invokes QEMU **without** `-accel hvf`, so QEMU falls back to TCG. TCG is a software JIT that translates every guest instruction to host instructions at runtime. For scalar code this introduces a ~5-10x overhead. For NEON-heavy workloads the penalty is catastrophic:

- Each 128-bit Q-register operation (FMLA, SDOT, etc.) is translated into a sequence of host function calls or emulation routines.
- llama.cpp's GGML kernels (`ggml_vec_dot_q4_K_q8_K`, etc.) are tight loops of NEON intrinsics — the exact worst case for TCG.
- TCG cannot fuse operations, speculate, or use host SIMD for guest SIMD.
- The JIT translation cache itself adds TLB pressure, instruction cache misses, and branch mispredictions.

The generation phase (autoregressive, one token at a time) is even worse than prompt processing because TCG overhead is per-instruction and the compute-to-overhead ratio drops when doing less arithmetic per syscall/interrupt cycle.

### Why Akuma can't use HVF (for now)

`scripts/run.sh` does specify `-accel hvf`, but QEMU crashes on boot with:

```
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883.
```

This is a **QEMU bug**, not an Akuma bug. When the guest executes certain AArch64 instructions that don't set the ISV (Instruction Syndrome Valid) bit in ESR_EL2 — notably `STP`/`LDP` of Q-registers — and those instructions trigger a VM exit (e.g. page fault during NEON save in the exception handler), QEMU's HVF backend asserts because it cannot decode the faulting instruction without ISV. The NEON save/restore code in `src/exceptions.rs` uses `stp q0, q1, [sp, #offset]` sequences, which are exactly the instructions that lack ISV information.

Possible workarounds (not yet implemented):
1. Pre-fault the exception handler stack pages so the NEON save instructions never trigger page faults during VM execution.
2. Replace Q-register `STP`/`LDP` in exception handlers with `STR`/`LDR` (single-register variants may behave differently for ISV).
3. Use `ST1`/`LD1` NEON store/load instructions instead, which may set ISV.
4. Wait for upstream QEMU to fix the HVF ISV handling (QEMU issue tracker has reports of this).

## Secondary factors (Akuma kernel overhead)

Even with HVF, Akuma would likely be somewhat slower than Alpine due to kernel-level differences. These are minor compared to the TCG gap but worth documenting.

### 1. NEON save/restore on every exception entry

Every syscall, IRQ, and page fault saves and restores 528 bytes of NEON/FP state (32 Q-registers + FPCR + FPSR) in addition to 304 bytes of GPR state. This is 36 `STP`/`LDP` pairs plus 4 `MRS`/`MSR` + `STR`/`LDR` instructions per exception entry/exit.

Linux avoids this cost via **lazy FP context switching**: it sets the `TIF_FOREIGN_FPSTATE` flag and traps on first FP use after a context switch, only saving/restoring when the FP unit is actually used by both the outgoing and incoming threads. Akuma saves eagerly on every exception because implementing lazy switching requires tracking per-thread FP-dirty state and handling the FP trap (ESR EC=0x07), which adds complexity.

**Estimated overhead:** ~200-400 ns per exception entry/exit on real hardware (negligible on Graviton for inference workloads where syscalls are infrequent relative to computation).

### 2. Syscall instrumentation

When `config::PROCESS_SYSCALL_STATS` is enabled (currently `true`), every syscall reads the timer twice (`uptime_us()`) and updates per-syscall counters. This adds ~100 ns per syscall.

### 3. Spinlock-based synchronization

Akuma uses spinlocks for kernel data structures (scheduler, PMM, VFS). Linux uses more sophisticated sleeping locks, RCU, and per-CPU data. Under contention from llama.cpp's thread pool this could add latency, though with only 2 inference threads (`-t 2`) contention should be minimal.

### 4. No `mmap` for model loading

llama.cpp is run with `--no-mmap` because Akuma's VFS doesn't support file-backed mmap. The model is loaded via `read()` syscalls into anonymous memory. This means:
- Model loading is slower (sequential reads vs. demand paging).
- No page sharing if multiple processes load the same model.
- No kernel page cache benefits.

This only affects startup time, not inference throughput.

### 5. Timer quantum and scheduling

Akuma uses a 10ms round-robin quantum. llama.cpp with `-t 2` spawns worker threads that synchronize via futexes. If the scheduler doesn't colocate related threads well, context switch overhead at quantum boundaries adds up. However, llama.cpp workers spend most of their time in compute, so preemption overhead is small relative to total runtime.

### 6. Memory pressure

With 256MB total RAM and ~127MB used by llama.cpp, only ~100MB remains for the kernel and other services. The kernel heap is 16MB. There's no swap. If the PMM runs low on free pages, demand-paging stalls can cause latency spikes during inference. The 135M model fits comfortably, but larger models would be constrained.

## Expected performance on AWS Graviton

On real AArch64 hardware (the target deployment), the TCG penalty disappears entirely. Expected performance:

| Factor | Impact |
|--------|--------|
| TCG overhead | **Gone** — native execution |
| NEON save/restore | ~0.1% overhead (rare relative to compute) |
| Syscall instrumentation | Negligible (can be disabled) |
| Spinlocks | Negligible at `-t 2` |
| No mmap | Startup only |

Graviton 2/3 instances support NEON, FP16, and dot-product natively. With 1GB RAM (free tier) and a Q4_K_M quantized model, inference should approach speeds comparable to Alpine on the same hardware — likely within 10-20% of a Linux kernel for this workload.

## How to reproduce

```bash
# TCG (slow) — default cargo runner
cargo run --release
# Inside Akuma:
llama-cli -m /model.gguf -p "Hello" -n 16 -t 2 --no-mmap -c 256

# HVF (fast, currently crashes due to QEMU bug)
scripts/run.sh
```

## Summary

The ~3000x performance gap is almost entirely explained by QEMU's TCG software emulation versus HVF hardware virtualization. The Akuma kernel adds minor overhead from eager NEON save/restore and syscall instrumentation, but these are negligible on real hardware. The path to fast inference is either fixing the QEMU HVF ISV assertion (a QEMU-side fix) or deploying to real AArch64 hardware.
