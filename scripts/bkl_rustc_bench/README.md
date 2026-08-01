# rustc scaling benchmark (BKL Phase 7 baseline)

Measures in-VM `rustc` wall-clock at SMP=1/2/4 and against Docker/Linux on the same host.
Results and full analysis: [`../../docs/archive/BKL_RUSTC_SCALING_BASELINE.md`](../../docs/archive/BKL_RUSTC_SCALING_BASELINE.md).

| file | role |
|---|---|
| `hello_nostd.rs` | `--crate-type=lib --emit=metadata`; no linker, no `cc` exec — isolates startup |
| `hello_std.rs` | full link (forks `cc`) — startup + link |
| `big.rs` | 4,408 generated lines — codegen-bound; regenerate with `gen_big.py` |
| `pbench.py` | Akuma driver: host-side parallel `ssh` execs, timed + **verified** |
| `dbench.py` | Docker counterpart, identical methodology |
| `results_*_2026-08-01.txt` | the baseline run |

## Read the scaling columns, not the absolute seconds

Akuma is 3× slower than Linux on codegen and 58× on startup; BKL work moves neither much.
What it should move is **parallel efficiency**: Akuma extracts 1.18× from 4 cores where
Linux extracts 2.95×, and going 2→4 cores currently returns 0–12%. That gap is the metric.

## Two traps this harness exists to avoid

- **Never use the guest shell's `&` + `wait`** — Akuma's busybox `sh` does not reliably
  return from `wait` after a backgrounded child exits (reproduced at concurrency 1). All
  concurrency is host-side.
- **Verify artifacts, never trust wall-clock.** An unverified pass recorded a cell at 15s
  that really takes ~100s — it had failed silently. `pbench.py`/`dbench.py` check every
  output file's size before counting the timing.

Also: `awk`/`grep` over serial logs need `LC_ALL=C` (SMP interleaving emits invalid
multibyte sequences, which silently breaks readiness-wait loops).

## Run

Sequence the Akuma and Docker halves — never concurrently. `--cpus` bounds Docker's share
but reserves nothing for QEMU's vCPUs, and a host-descheduled vCPU holding the BKL is
indistinguishable from BKL contention.

```bash
( cd scripts/bkl_rustc_bench && python3 -m http.server 8899 --bind 127.0.0.1 & )
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests
SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4 scripts/cargo_runner.sh \
    target/aarch64-unknown-none/release-smp-shared/akuma > boot.log 2>&1 &
until LC_ALL=C awk '/Starting service: sshd/{f=1} END{exit !f}' boot.log; do sleep 5; done
./venv/bin/python scripts/bkl_rustc_bench/pbench.py 4      # repeat for 2, 1
docker run -d --name dbench --platform linux/arm64 --cpus=4 -m 4g \
    -v "$PWD/scripts/bkl_rustc_bench:/work" alpine:edge sleep 7200
docker exec dbench apk add -q rust
./venv/bin/python scripts/bkl_rustc_bench/dbench.py
```
