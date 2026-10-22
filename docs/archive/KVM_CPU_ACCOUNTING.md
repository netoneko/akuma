# KVM: "cpu0 is 100% busy" was accounting, the relay was not slow, and vCPUs got their own cores

2026-10-23, ryzen metal (entry 30, `kvmctl kvmmem=4096`), branch `kvm-part-1`.

## Part 1 - what was measured

New instrumentation (stays): `sched::halted_tsc(core)` counts TSC cycles each core spends in a
`hlt` (`idle_loop` and `allow_tick`); the VM task prints `[KVM] cpu:` every 5 s into the guest log
(its own split: guest / sleep / yield / relay / other, every core's busy share, exit rates by kind),
and `echo status > /dev/kvm0` prints `[kvm] cores:` for the interval since the last status - which
works with no guest, so it is the A/B baseline.

**cpu0 "100%".** `/proc/stat` bills a halt inside `allow_tick` to the task that called it
(`idle_loop` credits its halts back with `credit_halted_time`; `allow_tick` did not). A core whose
only task halts that way reads 100% busy. True (TSC) busy share of cpu0:

| state | cpu0 busy | other cores |
|---|---|---|
| no guest (A, twice in one boot) | 4-8% | ~2% |
| idle 1-vCPU guest | 12-13% (sleep 91%, ~6.6k exits/s: lapic 3.5k, msr 2.4k, hlt 0.5k) | ~2% |
| idle 2-vCPU guest, one core | 16% | ~2% |
| ssh session, idle | 13-15% | ~2% |
| 32 MiB relay transfer | 74% (guest 27%, exit handling 30%) | 2-16% |

`/proc/stat` showed cpu0 ~100% in the no-guest row too. `allow_tick` now credits its halt; cpu0 in
`/proc/stat` then matches the TSC (12.8% against 12-13%). Per-core `/proc/stat` remains a
cumulative per-thread sum bucketed by last core and is not monotonic after credits: use the TSC lines.
cpu1's "67%" was the same artifact (TSC: 1.5%).

**Relay.** `relay_bulk.py` timed 4 MiB *including the outer ssh handshake to the host (~0.7 s)*; that
is where 2.6-4.3 MB/s came from. `scripts/kvm/relay_profile.py` subtracts it: steady state through
the relay is 14-17 MB/s (one vCPU). From the Mac, the host's own sshd and the guest behind the relay
give the same 18-19 KiB/s down / 42-44 KiB/s up: the wifi link is the limit, not the hypervisor.

**vCPUs.** Two vCPUs time-slicing one core: bulk 7.5 MB/s (half of one vCPU); late SYN-ACKs were not
reproduced as worse (2 retries against 3 in a similar stress run).

## Part 2 - one task per core

`amd64/src/kvm/linux/machine.rs` (see `docs/reference/subsystems/kvm.md`). Measured on the metal,
two-vCPU Akuma guest, CPU-bound shell loops:

| placement | 1 loop | 2 loops |
|---|---|---|
| 2 vCPUs, `vm_cores = 2-3` (SMT siblings) | 5.9 s | 9.3 s |
| 2 vCPUs, `vm_cores = 2,4` | 5.9 s | 6.1 s (1.95x) |

`relay_stress.py` passes (3 rounds) on both guests; bulk through the relay with two dedicated vCPU
cores is 11.2-11.5 MB/s (7.5 time-sliced, 14-17 for one vCPU: exit handling is serialised under the BKL).
