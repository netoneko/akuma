# Runbooks

Runbooks are action-first: do these steps, expect to see this output. No
narrative, no investigation story (that lives in `../archive/`).

## Triage matrix

Start from the symptom or task on the left.

| You are... | Read |
|---|---|
| Booting a VM and connecting via SSH | [`boot-and-connect.md`](boot-and-connect.md) |
| Building the devbox image | [`build-devbox.md`](build-devbox.md) |
| Building the `extreme-size` (4 MB floor) image | [`build-extreme-size.md`](build-extreme-size.md) |
| Debugging the devbox (SSH down, cargo crash, 100% CPU) | [`debug-devbox.md`](debug-devbox.md) |
| Recovering a wedged / hung / 100%-CPU VM | [`recover-wedged-vm.md`](recover-wedged-vm.md) |
| Debugging networking (native smoltcp stack) | [`debug-network.md`](debug-network.md) |
| Changing `blocking_relax` / any shared-kernel-SMP wait primitive | [`../archive/BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md`](../archive/BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md) — a single-core suite cannot verify it (the regression test SKIPs at `SMP=1`); run `MEMORY=2048 SMP=4 cargo run --release` and require `smp_shared_blocking_wait_peer_progress PASSED` |
| Network **latency** — a round trip costs milliseconds, or you want to profile the NIC path | [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) — build `--features net-profile` for `[NICSTAT]`, drive it with [`scripts/benchmarks/bench_nic_rtt.py`](../../scripts/benchmarks/bench_nic_rtt.py). If `nic_irq=0` in the dump, the NIC SPI is not reaching the CPU and the stack is back to being tick-driven |
| Network latency regressed and the NIC path "looks fine" | Check that `src/main.rs` re-arms `NIC_WAKE_PENDING` **before** the `while poll()` drain, not after. Re-arming after swallows wakes and costs 65 % of throughput — [`debug-network.md`](debug-network.md), [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §9 |
| Tempted by `net-waker-park` (register a waker instead of `blocking_relax`) | **Off on purpose.** Sockets really are the only blocking path that parks without registering — and fixing it measured *worse* (1,071 → 944 req/s), because `blocking_relax` wakes on any IRQ and the NIC raises ~6,300 per 5 s — [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §8 |
| Tempted by `net-noalloc` (static NIC rings / async TX) for latency | **Off on purpose.** It halves the time the stack holds `NETWORK` and still regresses HTTP p90 2.9x, because the cost moves into the wake — [`debug-network.md`](debug-network.md), [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §7 |
| Connections reset under connection-per-request load (HTTP/1.0, `accept` fails) | Socket slots exhausted by `TimeWait` against a 128 budget. **FIXED 2026-08-19** by a pressure valve in `socket_create` — [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §3.4 |
| In-guest `cargo` says `Could not connect to index.crates.io:443` while `curl` gets 200 | [`cargo-cannot-reach-crates-io.md`](cargo-cannot-reach-crates-io.md) — **no cargo config fixes this**; the multiplexing advice was disproven. It is nightly cargo's vendored libcurl, not the net stack. Fetch with apk `/usr/bin/cargo`, then build `--offline` |
| A client hangs only when the server is slow to send its first byte | [`debug-delayed-first-byte.md`](debug-delayed-first-byte.md) |
| An async runtime's child process never completes — the child exits in milliseconds, the caller times out | [`debug-async-subprocess-hang.md`](debug-async-subprocess-hang.md) |
| Running Redis — the Alpine package, or the official `redis:alpine` image in a box | [`run-redis.md`](run-redis.md) |
| Debugging OOM / panics / allocation failures | [`debug-memory-oom.md`](debug-memory-oom.md) |
| Debugging an EL1 crash / data abort / unhandled exception | [`debug-exceptions.md`](debug-exceptions.md) |
| Debugging a boot hang | [`debug-boot-hang.md`](debug-boot-hang.md) |
| Debugging shared-kernel SMP (BKL deadlock/contention, profiler) | [`debug-smp.md`](debug-smp.md) |
| `[BKL] stuck` bursts under fork/thread churn — check `owner=` **first** | `owner=0` = the lock is *free*, so it is a lost FIFO ticket, not a stuck holder; `tag=511` means nothing without `bkl-profile`. **FIXED 2026-08-08** (barges no longer touch the queue): [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) -> "The FIFO ticket invariant". Reproduce with `c_stress/bssfork 20 3 1` at SMP=4 |
| `[SGI] POOL contended, skipped N ticks` climbing forever; console still printing but ssh dead | **OPEN.** The box is *unscheduled*, not hung — `POOL` gates all preemption. The preemption watchdog cannot see it, and the tid in the message is the interrupted thread, **not** the holder: [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md) -> "The `POOL` gate" |
| Debugging a thread parked forever in `futex` (lost wakeup) | [`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) |
| Debugging a brand-new `pthread_create`d thread that SIGSEGVs at birth | [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) |
| A process that forks while multi-threaded dies with `EXIT=139` / `[WPF] cow_ref=0 lazy_self=NONE` | **FIXED 2026-08-08** — [`../archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](../archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md) §12. Regression: `c_stress/bssfork 20 3` |
| A write fault SIGSEGVs a page whose `[WPF]` line says `ap_rw=true` | The write was legal and the fault was stale — some other thread had already repaired the page. §12 of the same audit; the repair is `stale_write_fault_absorbed` in `src/exceptions.rs` |
| A VM feels slow / unresponsive under parallel load, or an in-VM build crawls | The console may be the bottleneck — three per-event traces were unconditional until 2026-08-08 and cost 270 KB/s: [`../archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`](../archive/SERIAL_TRACE_TRAFFIC_AUDIT.md). Histogram the log before blaming the kernel |
| Debugging SSH latency / echo / terminal sizing | [`debug-ssh-latency.md`](debug-ssh-latency.md) |
| Self-hosting (compiling the kernel inside Akuma) | [`selfhost-kernel-build.md`](selfhost-kernel-build.md) |
| Running a Docker image with `box run` | [`run-docker-image.md`](run-docker-image.md) |
| Adding an apk package to the devbox | [`add-apk-package.md`](add-apk-package.md) |
| Adding a `sc-*` kernel feature | [`add-syscall-feature.md`](add-syscall-feature.md) |
| Landed a fix and need to update the bugfix audit | [`update-bug-fix-list.md`](update-bug-fix-list.md) |
| Looking for copy-pasted code before a refactor (PMD CPD) | [`find-duplicated-code.md`](find-duplicated-code.md) |
| Exporting an HTML deck under `bootstrap/public/` to PDF — or wondering why Cmd-P prints the mobile layout with huge type | [`print-deck-to-pdf.md`](print-deck-to-pdf.md) — Chrome lays print media out at a ~800px viewport whatever `@page` says; use [`scripts/render_deck_pdf.py`](../../scripts/render_deck_pdf.py) |
| Confirming a deduplication / extraction change caused no regression — [`scripts/verify_trim.py`](../../scripts/verify_trim.py) diffed against your parent commit, which log lines are known-benign, and the redis memtest for memory-path changes | [`verify-trim-fat-change.md`](verify-trim-fat-change.md) |

## Conventions

- Each runbook ends with a **Verify** section: the exact output that confirms
  success.
- Commands are copy-pasteable. Env knobs are called out explicitly.
- "Background" footers link to `../archive/` originals for the investigation
  story behind a procedure.

## Authoring a new runbook

1. Name it after the *task* or *symptom*, not the subsystem
   (`debug-devbox.md`, not `rump.md`).
2. Lead with the one-paragraph "when to use this".
3. Steps are numbered, present-tense, imperative.
4. End with **Verify** - the log lines / command output / SSH result that means
   it worked.
5. Add a row to the triage matrix above.
