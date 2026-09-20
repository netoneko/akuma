# Runbooks

Runbooks are action-first: do these steps, expect to see this output. No
narrative, no investigation story (that lives in `../archive/`).

## Triage matrix

Start from the symptom or task on the left.

| You are... | Read |
|---|---|
| Booting a VM and connecting via SSH | [`boot-and-connect.md`](boot-and-connect.md) |
| Changing Akuma/amd64 and testing it on the **real HP box**, hands-off | [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the two-personality ssh trap, `reboot -f`, the three KVM rigs, and the known-broken list (`date` 1970 → "certificate not trusted", pipes, `ps`, `wget https`) |
| The HP box boots but answers **nothing** on the network — no ssh, no ping | [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "Boot it with `netprobe`" — every `[rtl]` line is gated behind a stall that may never arm, so their absence proves nothing; `[probe]` is unconditional and gives `rx=`/`dry=`/`kicks=`/`polls=`/`laps=` plus the `rx_desc pa=` to check `rdsar` against. Also: with no DHCP lease the box sits on the static **`192.168.1.220`**, not `.123` |
| ...and `kicks=0` next to a frozen `rx=` | [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "The `Silent` arm" — a *detection* failure, not a failed repair. A receiver that delivered a few frames and then stopped used to disarm both stall arms on the way past and stay deaf for the boot. Fixed 2026-09-20; `kicks=` climbing with `rx=` still flat is the other, more interesting answer |
| Not sure whether a devbox userspace process is actually hung or just slow | [`diagnose-hung-userspace-process.md`](diagnose-hung-userspace-process.md) — start here before kernel tracing or gdb |
| Working **inside** Akuma on the HP box — its own checkout, toolchain, cargo cache and `kbuild`/`ubuild`/`mbuild`/`kinstall` | [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "Working **on** the box" — a session has no environment, cargo reads its config from the *cwd*, proc macros need musl's `libc.so`/`libgcc_s.so` on lld's search path, and `execve` cannot run a `#!` script |
| Adding **Intel HDA audio** to Akuma/amd64 (the on-box agent's brief) | [`add-intel-hda-audio.md`](add-intel-hda-audio.md) — the existing `/dev/dsp` seam to reuse, why it iterates **on the metal** (the reboot is the loop), and the bring-up order |
| Putting a nightly Rust toolchain inside the Akuma/amd64 guest | [`stage-rust-toolchain-amd64.md`](stage-rust-toolchain-amd64.md) — musl host, `--force-non-host`, loop-mount the image; and the `LD_LIBRARY_PATH`/`$ORIGIN` trap that makes `rustc` unable to find its own `.so` |
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
| In-guest `cargo` says `Could not connect to index.crates.io:443` while `curl` gets 200 | [`cargo-cannot-reach-crates-io.md`](cargo-cannot-reach-crates-io.md) — **fixed 2026-08-20**: `sys_pselect6` was not writing `exceptfds`, and the nightly toolchain's libcurl uses `select(2)`. Was never a net-stack fault. |
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
| A write fault SIGSEGVs a page whose `[WPF]` line says `ap_rw=true` | The write was legal and the fault was stale — some other thread had already repaired the page. **Re-fixed 2026-08-30**: the absorb now re-checks after the fault-slot wait and at the SIGSEGV delivery point too (the entry-time check alone lost races to the repair). §12 of the same audit; the repair is `stale_write_fault_absorbed` in `akuma-exceptions`; storm repro `c_stress/cowstale hammer` |
| A VM feels slow / unresponsive under parallel load, or an in-VM build crawls | The console may be the bottleneck — three per-event traces were unconditional until 2026-08-08 and cost 270 KB/s: [`../archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`](../archive/SERIAL_TRACE_TRAFFIC_AUDIT.md). Histogram the log before blaming the kernel |
| Debugging SSH latency / echo / terminal sizing | [`debug-ssh-latency.md`](debug-ssh-latency.md) |
| Self-hosting (compiling the kernel inside Akuma) — **AArch64** | [`selfhost-kernel-build.md`](selfhost-kernel-build.md) |
| Self-hosting on **amd64** (Firecracker guest on the HP box) | [`selfhost-kernel-build-amd64.md`](selfhost-kernel-build-amd64.md) — the one-command gate, the image-drift step that fakes a compile error, timings (`-j8` ~164 s, the fastest cell), and the fixed-point check |
| Self-hosting on **amd64 bare metal** (the HP box itself, no hypervisor) | [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "Self-hosting on the metal" — GRUB reads the kernel off Akuma's **own ext2 root**, so installing one is a `cp`; plus why `reboot -f` no longer returns you to Ubuntu |
| Swapping the running kernel for a freshly built one without touching the host (`KERNEL_DROPOFF` + raw block fd + `reboot(2)`) | [`selfhost-kernel-build.md`](selfhost-kernel-build.md) § "Swap the running kernel in place" — **AArch64 only**; the amd64 equivalent needs none of that machinery (row above) |
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
| Akuma on Firecracker / `/dev/kvm` on a Mac / microVM boot | [run-on-firecracker.md](run-on-firecracker.md) |
| The device tree a Firecracker microVM actually gets / GIC and virtio addresses per vCPU count | [dump-firecracker-fdt.md](dump-firecracker-fdt.md) |
