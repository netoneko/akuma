# rustc scaling baseline before BKL Phase 7 — 2026-08-01

Pre-Phase-7 throughput baseline, and the first Akuma-vs-Linux comparison on a
**process-lifecycle-heavy** workload. Companion to
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md); the metric defined here is Phase 7's
success criterion per
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.4.

## 1. Why this workload

The campaign had three kinds of evidence and all three had gaps:
`contention_spins`/`[BKLPROF]` is a *proxy* (spins, not seconds); digests prove
correctness but say nothing about speed; and the one throughput number that existed
(`BKL_MM_CARVE_OUT.md` §4, llama.cpp tok/s) is compute- and mmap-bound and barely touches
process lifecycle. `rustc` exercises exactly the holders that are still un-carved —
`execve` ~22%, `clone` ~10–13%, `openat` ~10% (audit §1.2).

`hello.rs` rather than a cargo crate, deliberately: no dependency graph, no `build.rs`, no
proc-macro2 exposure (the in-VM kernel build deadlocks there), and low enough variance to
afford reps. `big.rs` (4,408 generated lines, 400 structs + impls, `HashMap`/`format!`) adds
a codegen-bound contrast — still a single `rustc` invocation, no cargo.

## 2. Method

- Guest: `devbox.img` (same disk as the contention regimen), apk `rustc 1.96.1`
  (`librustc_driver-209dcae0deb659d4.so` is 63 MB — the dominant startup mmap).
- Kernel: **one binary** across all SMP settings,
  `release-smp-shared --features devbox-smoltcp,no-tests` (no profiler — it perturbs
  timing). Only QEMU `-smp` varies. `SNAPSHOT=1`, `MEMORY=4096`, fresh boot per setting.
- Docker: `alpine:edge`, `rustc 1.97.0`, `--platform linux/arm64` (native on Apple
  Silicon), `--cpus=4 -m 4g` to match. Same three source files, byte-identical.
- **Timing and verification are host-side and identical for both**, so only the guest
  differs: N parallel `ssh`/`docker exec` invocations, wall-clock of the batch measured on
  the host, and **every compile's output artifact size checked** before the timing counts.
- **Akuma and Docker runs were sequenced, never concurrent** — `--cpus=4` bounds Docker's
  share but reserves nothing for QEMU's vCPU threads, and a host-descheduled vCPU holding
  the BKL is indistinguishable from BKL contention (`KernelLock`'s lost-ticket recovery
  cites host descheduling for this reason). Concurrent runs would bias *toward* the
  hypothesis.
- 2 reps/cell; rep-to-rep spread was <1% on every cell that passed verification.

## 3. Results

Wall-clock seconds, median of 2. `c` = concurrent `rustc` invocations.

| mode | c | SMP=1 | SMP=2 | SMP=4 | docker | akuma4/docker | 1→4 speedup | 2→4 speedup |
|---|---|---|---|---|---|---|---|---|
| `nostd` | 1 | 12.25 | 5.83 | 5.72 | 0.22 | **26×** | 2.14× | 1.02× |
| `nostd` | 4 | 22.47 | 14.75 | 13.15 | 0.17 | **77×** | 1.71× | 1.12× |
| `std` | 1 | 39.68 | 13.72 | 13.36 | 0.23 | **58×** | 2.97× | 1.03× |
| `std` | 4 | 85.60 | 44.30 | 40.78 | 0.22 | **185×** | 2.10× | 1.09× |
| `big` | 1 | 99.07 | 30.12 | 29.12 | 9.69 | **3.0×** | 3.40× | 1.03× |
| `big` | 4 | FAIL | 95.25 | 99.04 | 13.14 | **7.5×** | — | 0.96× |

Parallel efficiency — `conc=4 / conc=1` on 4 cores (1.00× = perfect scaling, 4.00× = no
parallelism at all):

| mode | Akuma SMP=4 | Docker | effective speedup: Akuma vs Linux |
|---|---|---|---|
| `nostd` | 2.30× | 0.77× | 1.74× vs 5.18× |
| `std` | 3.05× | 0.96× | 1.31× vs 4.18× |
| `big` | 3.40× | 1.36× | 1.18× vs **2.95×** |

## 4. What the numbers say

**4.1 Beyond 2 cores, extra cores buy essentially nothing.** The 2→4 column is 0.96–1.12×
across every cell — and `big conc=4` *regresses*. Doubling cores on a compile workload
returns 0–12%. This is the Phase 7 justification, stated as a number: **`big conc=4` on 4
cores is no faster than on 2.**

**4.2 Akuma's parallel efficiency is ~1.2–1.7× where Linux gets ~3–5× on the same 4
cores.** For `big` — the cell least distorted by startup cost — Akuma extracts **1.18×**
from four cores and Linux extracts **2.95×**. That gap, not the absolute times, is what
Phase 7 should move.

**4.3 The SMP=1 → SMP=2 jump is not parallelism, and it is the most surprising result.**
A *single-threaded* compile goes 39.68 → 13.72s (2.97×) from 1 to 2 cores; `big` goes
99.07 → 30.12s (3.29×). Superlinear speedup for a one-process workload means the SMP=1
case is pathological: with one core, rustc must interleave with the kernel's own
long-lived threads — the async-main smoltcp poll loop, `netpoll_maint` housekeeping
(`reclaim_terminated_slots` every 100 ms), herd, sshd, and the timer tick. Give it a
second core and those move off the compile's core. So **~⅔ of a single core is consumed by
kernel background work**, which is consistent with `idle` + `irq/sched` + `netpoll_maint`
being 98% of the *idle-boot* BKL attribution (audit §1.2).

**4.4 The Akuma penalty is startup, not codegen — a 60× spread between the two.** Against
Docker: `hello std` is **58×** slower, but `big` is only **3.0×**. `hello` measures process
startup (ELF load + the 63 MB `librustc_driver` mmap out of ext2) plus a `cc` exec; `big`
adds ~9.5s of real codegen that dominates. Decomposing: Docker `big` − Docker `std` ≈ 9.5s
of codegen; Akuma `big` − Akuma `std` ≈ 15.8s. So **codegen itself is only ~1.6× slower on
Akuma; startup is 58×.** That points at ext2 read + eager dylib mmap (the effect
`rustc_compile_ext2_mmap` already recorded: lazy mmap alone was ~6.9× on startup), not at
the BKL.

**These two findings pull in different directions and both are real.** BKL work should
improve 4.1/4.2 (scaling) and will barely touch 4.3/4.4 (single-stream latency). Anyone
using this baseline to judge Phase 7 must read the *scaling* columns, not the absolute
seconds — otherwise a successful Phase 7 will look like a failure.

## 5. Caveats

- **`rustc` versions differ**: Akuma 1.96.1 (apk stable on `devbox.img`) vs Docker 1.97.0
  (`alpine:edge`, the closest available). One minor version; the gaps here are 3–185×, far
  outside any plausible version effect. Matching exactly would need an apk pin.
- **`[FORK-DBG]` prints are in the measurement path.** `replace_image` emits five
  unconditional UART writes per `execve`, three inside the BKL-held destructive window, on
  an unlocked `static UART` (audit §4.2). Confirmed live in every boot log here. They
  inflate Akuma's exec-heavy cells by an unmeasured amount. **Re-baseline after they are
  removed**, or measure both ways.
- **`big conc=4` at SMP=1 failed both reps** — artifact absent, rustc silent. Most likely
  memory pressure (4 concurrent 63 MB-dylib compiles, 1 core, 4 GB). Not investigated; it
  is the one missing cell.
- **Docker is not bare metal** — Docker Desktop runs a Linux VM on the same host, so this
  is VM-vs-VM, which is the fairer comparison. Host: 12 logical / 8 performance cores.
- No `[BKLPROF]` attribution was captured on *this* workload. That is the obvious
  follow-up and the thing that would separate "BKL serialization" from "host
  oversubscription" in §4.2 — see §6.

## 6. Follow-ups this baseline implies

1. **Run the matrix on a `bkl-profile` build** to get attribution and throughput on the
   *same* workload — the pairing the campaign has never had. Until then, §4.2's gap is
   "poor scaling," not specifically "BKL."
2. **Re-baseline without `[FORK-DBG]`.**
3. **Startup cost is a bigger lever than the BKL for single-stream rustc** (58× vs 3×).
   That is `rustc_compile_ext2_mmap` territory (lazy/file-backed mmap of the 63 MB dylib),
   not Phase 7 — but it is where the wall-clock actually is, and it should not be
   mis-attributed to locking.

## 7. Reproducing

```bash
# payload (hello_std.rs, hello_nostd.rs, big.rs) served to the guest as 10.0.2.2:8899
( cd /tmp/bklpay && python3 -m http.server 8899 --bind 127.0.0.1 & )
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests
# per SMP setting: fresh boot, then host-driven parallel compiles with verification
SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4 scripts/cargo_runner.sh \
    target/aarch64-unknown-none/release-smp-shared/akuma > boot.log 2>&1 &
./venv/bin/python scripts/bkl_rustc_bench/pbench.py 4
# docker side, sequenced AFTER all VM runs
docker run -d --name dbench --platform linux/arm64 --cpus=4 -m 4g -v /tmp/dbench:/work \
    alpine:edge sleep 7200 && docker exec dbench apk add -q rust
./venv/bin/python scripts/bkl_rustc_bench/dbench.py
```

**Two harness traps that cost real time here, both worth inheriting:**

- **Never use the guest shell's `&` + `wait`.** Akuma's busybox `sh` does not reliably
  return from `wait` after a backgrounded child exits — reproduced at concurrency **1**,
  leaving `batch.sh` hung with its children already gone from `ps`, and mangling `busybox
  time`'s output. All concurrency here is therefore host-side (N parallel
  `ssh`/`docker exec`). This is a real kernel-side finding, adjacent to the fixed
  `waitid`-on-non-children family, and it deserves its own investigation.
- **Verify artifacts, never trust wall-clock.** An unverified first pass recorded
  `big conc=4` at 15.04s against 119.29s for the identical cell — the fast one had failed
  silently. Every cell here checks the output file's size before the timing counts
  (`locking.md` playbook rule 6, and it caught a real error within minutes).
- Minor: `awk`/`grep` on serial logs need `LC_ALL=C` — SMP log interleaving emits invalid
  multibyte sequences and `awk` otherwise errors out, which silently broke a boot-readiness
  wait loop into an infinite spin.

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) — what Phase 7 is blocked on; §1.2 is the
  attribution table this baseline complements.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.4 — success
  criteria, which now reference this curve.
- [`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md) — the agent
  prompt this executes.
- [`BKL_MM_CARVE_OUT.md`](BKL_MM_CARVE_OUT.md) §4 — the llama.cpp Akuma-vs-Linux
  comparison, the other throughput datapoint.
