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

### Akuma now runs under HVF (since 2026-06-09)

The TCG numbers above are no longer the only option. `cargo run --release` uses
`-accel hvf` by default on Apple Silicon, and stories15M-q4_0 measures **~7000–7500
t/s prompt and ~1300–1700 t/s generation** under HVF (vs ~100 / ~13–14 t/s under
TCG with `-t 1`) — a ~70× / ~100× speedup. The `(isv)` assertion was **not** a
QEMU bug and **not** the NEON `STP`/`LDP` save/restore (that earlier theory was
wrong — NEON saves target the stack, which is RAM that HVF faults in without a
trap to QEMU). It was three Akuma assumptions that only hold under TCG:

1. **GICv2 MMIO programming model.** HVF presents GICv3; the GICv2 CPU-interface
   MMIO at `0x0801_0000` does not exist there, and distributor writes faulted with
   ISV=0 → assert. Fixed by a GICv3 driver (`src/gic_v3.rs`, default; GICv2 behind
   the `gic-v2` feature).
2. **Physical timer (`CNTP`).** Trapped under HVF; switched preemption to the
   virtual timer (`CNTV`/PPI 27).
3. **`IC IVAU` on a not-yet-mapped user VA** in the demand-pager; switched to the
   kernel alias (`kva`).

See `docs/QEMU_HVF_ISV_BUG.md` for the full writeup. Note `scripts/run.sh` is a
separate, older script; the canonical runner is `scripts/cargo_runner.sh`.

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

#### Futex verified correct + debug logging fixed (2026-06-09)

The ggml thread pool's `[futex-dbg] … result=ETIMEDOUT` ~1 s loops (visible during
inference) looked like a possible wake bug, so the futex wake path was
**measured**, not just assumed:

- New boot self-tests in `src/sync_tests.rs`: `test_futex_genuine_wake_no_value_change`
  (a genuine `FUTEX_WAKE`, with the futex word **never** changed, must return `0` —
  the EAGAIN value-changed path and the timeout path cannot mask a broken/slow wake)
  and `test_futex_wake_latency_prompt` (asserts wake latency ≪ timeout).
- Result on the `release` kernel: **both PASS, measured wake latency ~401 µs.**
  Genuine `FUTEX_WAKE` promptly unblocks a parked waiter. The ETIMEDOUT loops are
  **benign idle-worker waits**, not a kernel bug. **Futex is not the bottleneck.**

Separately, `config::FUTEX_DBG_ENABLED` was found set to `true` **globally** (not
profile-gated), so every futex op printed an `[futex-dbg]` line to the slow serial
UART, plus an *ungated* `[clear_child_tid]` print on every wake. Under inference
(thousands of futex ops) that is pure overhead and log spam. Fixed: default
`FUTEX_DBG_ENABLED = false` and gated the `[clear_child_tid]` print behind it.
Measured effect on generation t/s: **none** (confirming generation is TCG-bound,
not futex-bound) — but it removes the overhead and de-noises the logs. Flip the
flag to `true` only when actively debugging futex wait/wake pairing.

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
# HVF (fast) — default on Apple Silicon
cargo run --release
# Inside Akuma (single-core, so -t 1; -st for clean exit on repeated runs):
llama-cli -m /models/stories15M-q4_0.gguf -p "Once upon a time" -n 16 -t 1 -c 256 -st

# TCG (slow, portable) — force with HVF=0
HVF=0 cargo run --release
```

## Summary

The ~3000x performance gap was almost entirely QEMU's TCG software emulation versus
HVF hardware virtualization — and Akuma now runs under HVF by default on Apple
Silicon, closing it (~70× prompt / ~100× generation on stories15M). The remaining
kernel overhead (eager NEON save/restore, syscall instrumentation) is minor. The
fix was Akuma-side (GICv3 driver + virtual timer + correct I-cache maintenance),
not a QEMU patch; see `docs/QEMU_HVF_ISV_BUG.md`.
