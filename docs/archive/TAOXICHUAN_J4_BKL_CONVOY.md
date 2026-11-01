# The in-guest `-j4` build stalls intermittently: a BKL convoy of `poll` laps

**Status: OPEN, mechanism located, trigger and fix not yet established.** Written 2026-11-01. Stability grade C - nothing here is a `reference/` rule yet.

## Symptom
The in-guest `cargo build --release -p taoxichuan -j4` (a Taoxichuan kernel run as a 4-vCPU KVM guest on the ryzen under Pop!_OS, `scripts/kvm/taoxichuan/build_run.py`)
sometimes never finishes. Observed shapes, all on the same pinned source tree and the same toolchain root:

* **Spin at `execve rust-lld`.** Console freezes right after `[execve] ... rust-lld` and `[HEAP-R] 163MB used`; `[BKL] cpu N stuck: owner cpu M` climbs
  (84 at ~1290 s, 186 at ~2350 s); the guest burns ~394% host CPU; sshd stops answering; **no `[Fault]`**. Seen on 4 of the wedged runs.
* **`No child process (os error 10)`** from cargo's spawn of rustc at ~200 s (`EXIT=101`), no console fault. Seen once.
* **Ring-3 `#GP`** in a rustc process: `rip=0x300469d8 rsp=0x6fffffff8278 cr2=0x600000b9873d code=0`, `slot 4 killed (SIGSEGV)`, cargo `EXIT=139`. Seen once.

Only the first shape is understood. The other two are recorded because they appeared in the same series and may be the same family or separate bugs; nothing here
links them yet.

## History of the (wrong) first diagnosis
After two host watchdog resets (`HOST_KVM_BKL_WEDGE_UNDER_GUEST_LOAD.md`) every in-guest build stalled on four different kernels. `e2fsck -fn` showed p3 (the ext2
base disk) corrupted, and the stall was attributed to the disk. A run on an `e2fsck -fy`-repaired qcow2 overlay then **passed** (10m29s), which looked like confirmation.
It was not: after p3 itself was repaired (`-fp` stopped at a corrupt directory, `-fy` finished; clean on re-check) the same kernel failed again. The disk was genuinely
corrupt and is now clean, but it is **not** what stalls the build. One passing run does not confirm a cause for an intermittent failure.

## Evidence
All runs: 4 vCPUs, 3072 MiB, `microvm,accel=kvm`, fresh qcow2 overlay on the repaired p3 unless noted, artifact md5 when it finishes is always
`38852d540d8af420ac894544916cc2a1`. "ctl" = the kernel at `360fa708` (before X5, 1591736 bytes); "X5" = `5cb7c69b` (1591224 bytes).

| kernel | vCPUs | other guest running | result |
|---|---|---|---|
| ctl | 4 | no (kept, fsck'd overlay) | pass, 10m29s, 12 stuck lines |
| ctl | 4 | no | `ECHILD`, `EXIT=101` at ~200 s |
| X5 | 4 | no | rustc `#GP`, `EXIT=139` |
| ctl | 4 | yes (X5) | **wedge** at `execve rust-lld` |
| X5 | 4 | yes (ctl) | **wedge** at `execve rust-lld` |
| ctl | 4 | yes (1-vCPU ctl) | pass, 13m45s, 30 fault-ish console lines (same as `kb-fix`'s 4-vCPU row in `TAOXICHUAN_SMP.md`) |
| ctl | 1 | yes (4-vCPU ctl) | pass, 8m22s, 0 faults |
| X5 | 4 | yes (X5) | **wedge** |
| X5 | 4 | yes (X5) | **wedge** |

Control kernel: 5 runs, 3 pass (one of them 1 vCPU). X5 kernel: 4 runs, 0 pass. At 4 vCPUs that is 2/4 against 0/4 - **too few runs to say X5 is innocent**, and
four of the X5 runs were two-at-a-time on a 16-thread host with ~5 GB free, which may itself change timing. X5's changes (an eventfd slot table and `sigaltstack`'s
flag rule) are behaviour-identical on paper and host-tested, but the 0/4 is recorded as is. A clean answer needs the same kernels run alone, many times, or a
direct A/B on one rig slot.

## What the wedged guest is doing
Caught with the QEMU monitor (`build_run.py` opens `/root/mon<N>`; `scripts/kvm/taoxichuan/wedge_sample.py` does the sampling), two wedged X5 guests:

* **Three cores sit in `smp::bkl_enter+0x9c`**, the ticket spin. The fourth, the lock owner, runs: its RIP over successive samples is `Interface::poll`
  (`seq_to_transmit`, `dispatch`), `switch_to`, `exec_twin::can_run`, `memcpy`.
* **It is a convoy, not a deadlock.** `BKL_SERVING` advanced from `0x1cb5c1` to `0x1cb5c9` - 8 hand-overs in ~43 s - with one hold of ~30 s in the middle. The
  owner changes (cpu 1, 2, 3 at different samples).
* **The owner's stack** (stale words included, so read the order, not every frame): `dispatch -> sock::poll_arm -> net::sleep_until -> netpoll() / x86_yield ->
  smoltcp_net::poll -> Interface::poll -> Socket::dispatch -> seq_to_transmit`. Another sample shows `sigprocmask_arm -> TaskWalk::read_page/write_page`.

The "stuck" print is not a precise clock: `bkl_enter` prints once when a single wait passes 400,000,000 spins (seconds), so 186 lines means 186 waits that long.

## Mechanism (as far as it is established)
`poll`/`ppoll` (`sock::poll_arm`) loops: check readiness, then `net::sleep_until(now + 1 ms)`. Each lap of `sleep_until` runs **`netpoll()` - a full smoltcp poll,
loopback frames included - with the BKL held**, then yields (`x86_yield`) or `wait_lap()`s. The lock's depth is per *thread*, handed over at a switch
(`smp::transfer_depth`), so a thread that parks in a kernel `poll` keeps the lock with its core until another thread with depth 0 is switched in. With several such
pollers (sshd's serve loop, cargo and rustc waiting on pipes) the lock is occupied by network laps while the cores that need it for `execve`/`mmap`/`write` spin.
That is the "four CPUs are 1.8x slower than one" of `TAOXICHUAN_SMP.md`, pushed from slow to apparently stopped.

## Not established
1. **Why only sometimes.** Same kernel, same tree: wedge, `ECHILD` and pass all occur. Timing of when a poller and the `rust-lld` `execve` coincide is the obvious suspect; nothing measured.
2. **Which loopback TCP socket makes `seq_to_transmit`/`dispatch` dominate a lap**, and why a lap costs milliseconds-to-seconds rather than microseconds.
3. **Whether the `ECHILD` and the `#GP` are this bug.** A convoy does not explain a wrong answer from `wait4` or a general-protection fault. They may be a separate fork/exec race.
4. **Whether 1 vCPU can fail.** One 1-vCPU run, passed.

## Candidate fixes (none tried)
* Run `netpoll()` only for the lap itself and release the BKL around the yield/`wait_lap` in `sleep_until` (`smp::bkl_unlocked`), so a waiting poller does not hold it.
* Make `poll_arm` wait on a deadline with the lock released instead of 1 ms laps.
* Bound a lap's drain budget so one chatty loopback socket cannot extend a hold.

Each touches the SMP lock path; each needs an A/B of the in-guest build on parallel rig slots (`BR_N=5` and `BR_N=6` run side by side), several runs per arm, plus
`gate2.py` and `cow_leak_gate.py`.

## How to reproduce and look
1. `python3 scripts/kvm/taoxichuan/build_run.py KERNEL 4` (a second one with `BR_N=6` runs beside it; each takes ~14 min when it passes). It prints `log tail`, `artifact`
   and a console fault count; `EXIT=255` is the ssh client, not cargo.
2. If the console stops growing and `grep -ac 'BKL.*stuck' /root/tx-run<N>/console.log` keeps rising: copy `llvm-nm -C -n KERNEL` to the ryzen and run
   `python3 wedge_sample.py N SYMS` there. Rising `serving` = convoy; a frozen `serving` with a fixed owner would be a different bug.
3. Kill the guest by its pidfile (`/root/tx-run<N>/qemu.pid`), never by pattern - and never `pkill -f` a name that your own ssh command line contains.

## Verification status
Mechanism located by sampling two wedged guests; no fix attempted, no A/B. Host-side fixes in this series (`p3` repair, `build_run.py` parallel slots, monitor) are
rig changes, not kernel changes. Related: [`TAOXICHUAN_SMP.md`](TAOXICHUAN_SMP.md) (the 1-vs-4 vCPU timings), [`HOST_KVM_BKL_WEDGE_UNDER_GUEST_LOAD.md`](HOST_KVM_BKL_WEDGE_UNDER_GUEST_LOAD.md)
(the *host* wedge, a different bug that also ends in `[bkls>]` lines and which corrupted p3).

## Background
`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § box D (the same `[BKL] stuck` signature from `mmap` on bare metal); the Firecracker ticket-lock livelock in memory
`project_bkl_ticket_livelock_firecracker` (a lock-ordering cycle, which this is not).
