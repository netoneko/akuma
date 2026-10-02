# amd64 self-host build: targeted TLB shootdown (2026-10-02)

*Continues `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §19 ("what remains is
whatever serialises"). Rig: ryzen Firecracker, 4 vCPU / 4 GiB,
`scripts/benchmarks/ryzen_fc/`, clean 96-crate `cargo build -p akuma-amd64`,
`CLEAN_ALL=1`, host-timed.*

**Status (2026-10-02): done.** Targeted shootdown is −22 % at `-j4` and −14 % at
`-j1`. It exposed a latent `execve` `EIO` (§4) — a heap-capacity bug, not a
device error — now fixed by making `read_image` wait; 10/10 clean `-j4` builds.

## 1. Baseline and attribution

Kernel `9791d5a02e2e` (`main` `e6d8657`), `-j4`, 4 vCPU: **100 s** (run 301),
90 s in later runs. 27 `[BKL] stuck … tag=11` lines per build, as the runbook
already records and calls unexplained.

`[PSTATS]` over the sampled `rustc` processes (6 of them; the sweep prints every
30 s, so this is a sample, not a census):

| item | calls | kernel time |
|---|---|---|
| `futex` (printed as `accept` — asm-generic 202; x86 202 is futex) | 516 k | 10.6 s — blocking wait, not work |
| **`munmap` (`nr11`)** | 98 k | **5.8 s, ~59 µs per call** |
| `mmap` (`nr9`) | 113 k | 0.3 s |
| page faults | 163 k | — |

`munmap` is the only per-call cost that is both large and kernel work, and its
holder is the `tag=11` of every `[BKL] stuck` line. The 59 µs is not the page
walk (§9/§10 of the slowness doc made that O(log n)); it is the shootdown.

## 2. Cause

`shootdown::broadcast` IPI'd **every** online peer and `wait_for_acks` waited for
every one, for every flush — whether or not that core was running the edited
address space. A `rustc` with a handful of threads on a 4-core guest paid a
full IPI round-trip, under the BKL, to cores that were running `cargo`, `sshd`
or idle. Every other core queued behind the sender for the BKL for the length of
that round-trip.

## 3. Fix

`akuma_mmu::cores_on_l0_mask(root)` — a bitmask over the existing per-core
`ACTIVE_L0`/`PREV_L0` registry (already maintained by `paging::activate` for the
page-table free gate). `broadcast` now addresses only cores whose published
root is the sender's `CR3`, stores that set in `TARGET[me]` and the generation in
`TARGET_GEN[me]`, and `wait_for_acks` waits for those cores only.

* **Ordering.** A full fence sits between the caller's PTE stores and the
  registry loads. The peer side publishes with a `SeqCst` store (`xchg`) before
  `mov cr3`: either the scan sees the publish (core targeted) or the core's first
  walk after the switch sees the edit (nothing stale). Cores mid-switch are
  covered by `PREV_L0`.
* **Wait on own generation.** `wait_for_acks` used the global `GEN`. With
  targeting, a later sender may skip a peer this sender addressed, and that
  peer's `ACKED` would never reach the newer number — a wedge. Hence
  `TARGET_GEN`.
* **Fallbacks to "everyone"**: the sender's own root is not in the registry
  (an `activate_unpublished` path, so the registry cannot vouch); the sender is
  on the kernel root; more online cores than `TTBR_TRACK_CORES` (8) tracks.
* **No target → no IPI**: a single-threaded process (the common `munmap`) now
  returns `false` from `broadcast` and skips the wait.
* `TARGETED` (`AtomicBool`) restores the old behaviour; there is no runtime
  switch yet, so the A/B is two kernel binaries.

Files: `amd64/src/shootdown.rs`, `crates/akuma-mmu/src/lib.rs`.

## 4. Results

A/B on one rig, alternating arms, fresh boot per run, `CLEAN_ALL=1`.

| arm | jobs | runs | wall | `[BKL] stuck` | verdicts |
|---|---|---|---|---|---|
| baseline `9791d5a02e2e` | 4 | 7 | 90 s every run (one 100 s) | 27 every run | 7 PASS |
| targeted `eee85bd5234c` | 4 | 7 | 70 s | 0-1 | **5 PASS, 2 FAIL** (`EIO`) |
| baseline | 1 | 1 | 140 s | 0 | PASS |
| targeted | 1 | 1 | 120 s | 0 | PASS |
| targeted + wait fix `7ccf53ab56c0` | 4 | 10 | 70-81 s | 0-2 | **10 PASS** |

Faster by 20 s at `-j4` (−22 %) and at `-j1` (−14 %): the saving is the IPI
round-trip itself, not only BKL contention (at `-j1` the BKL is uncontended).
The 27 `tag=11` lines are gone — they were `munmap` holding the BKL across a
shootdown to cores that did not need it.

### Re-attribution on the 70 s kernel

Same `[PSTATS]` aggregation over the sampled `rustc` processes (run 8001: only 2
sampled, so per-call figures, not totals):

| | baseline (run 301) | targeted (run 8001) |
|---|---|---|
| `munmap` per call | 5803 ms / 98 271 = **59 µs** | 130 ms / 19 791 = **6.6 µs** |
| `mmap` per call | 3.0 µs | 2.5 µs |
| in-kernel time left | futex waits (blocking) | futex waits (blocking) |

`munmap` is out of the top. What is left in the sampled kernel time is untimed
futex parking, which is a wait, not work. Page faults (~2 k/s, 51 k in 23 s) are
the next per-event kernel cost and the audit's item 3 (shared zero page for
anonymous read faults) is the lever the first doc ranked for them; their per-fault
cost has not been measured here, so measure it before building anything.

### The `EIO` — cause found

Two targeted runs (4021, 6051) died ~20 s in with `lld-wrapper: could not exec
rust-lld: I/O error (os error 5)` while linking a host unit. No `[Fault]`, no
`[TLB] stuck`. The runbook `amd64-j4-build-crash-hunt.md` records the same
signature on the metal (3 of 10) and tied it to USB stalls; here it is virtio-blk.

`FsError::Internal` and `FsError::IoError` both map to `EIO`, so the errno could
not say which. A console line on the refused-reservation arm of `read_image`
settled it, on the first diagnostic run:

```
[exec] read_image: heap refused 165777608 B for …/rust-lld (heap=536870912 allocated=483295102)
```

`execve` holds the **whole image** in the kernel heap until the loader has copied
it out (`mem.rs` records why the heap is sized for it). The guest's heap is
512 MiB (RAM < 8 GiB), 128 MB of it the ext2 block cache; three concurrent
`rust-lld` (158 MB each) fill it and a fourth `rustc`'s `try_reserve_exact` fails.
It is a **capacity** failure that presents as a device error. The change
exposed it rather than caused it: faster links overlap more, so the window the
baseline had (and the metal's 3 of 10 may well share — the metal's heap is
1 GiB there, but its links are slower and its failures were not examined for
this line) got wider.

**Fix:** `read_image` retries the reservation, yielding then `allow_tick` (10 ms)
between tries, up to 2000 ticks (~20 s), because the holders finish in
milliseconds and need nothing the waiter holds. After the wait it fails with the
same console line. A wait prints `[exec] read_image: waited N ticks for heap:
<path>`; the wait engaged in **7 of the 10** clean runs (1-2 times each), i.e. each
was an `EIO` before.

## 5. Harness bug found on the way

`jrun.sh` detected completion with `grep -q '^RC='`, but cargo's progress bar
ends without a newline, so `RC=101` lands mid-line. A failed build was never
noticed and the run idled to its 2400 s budget (and a `WEDGE` after 300 s of
silence would have mislabelled it). Fixed on the rig:
`grep -q "RC=[0-9]"`.

## 6. Next lever: page-fault service (measured, then fixed)

`[PSTATS]`'s `pgfault` is a *count*; nothing timed a fault. `idt.rs` now
accumulates TSC cycles (`FAULT_STATS`: faults, BKL-acquire wait, `fault_in`
service) and `mm.rs` the file-backed share (`FILE_FAULT_CYCLES`); the 30 s sweep
prints them as `[FAULTSTAT]`. First reading (4 runs, instrumented 70-80 s kernel):

| | value |
|---|---|
| demand faults per clean build | **~1.5 M** (the sampled PSTATS said 163 k — it covered 6 processes) |
| mean BKL wait / service | 13 µs / 13-14 µs |
| file-backed faults | 510 k (31 %), **38.5 µs each = 93 % of all service time** |
| anonymous faults | ~1.1 M, ~1.2 µs each |
| file pages filled / of which shared-cache hits | 4.9 M / 4.67 M (**95 %**) |

So the per-fault cost was not the zero-page lever the first slowness doc ranked
(anonymous faults are cheap). It was `fill_file_pages` allocating, zeroing and
`ext2`-reading a **64 KiB window buffer on every file fault before looking at the
shared cache** — a buffer the loop never touches for the 95 % of pages that hit.
The buffer is now loaded on the first page that misses (`mm.rs`, `buf_tried`).

| | before | after |
|---|---|---|
| file-fault service | 38.5 µs | **8.0 µs** |
| mean service, all faults | 12.9 µs | 3.4 µs |
| mean BKL wait per fault | 12.2 µs | 4.1 µs (less held time to wait for) |
| clean `-j4` wall | 70-80 s | **60-70 s** (4/4 PASS, runs 9101-9104) |

Cumulative against the 2026-10-02 morning baseline: **90 s → 60-70 s (−22 to −33 %)**.

Remaining in the fault path: ~8 µs per file fault is now per-page work
(`is_current_user_range_mapped` walk, `with_current_address_space` lock and
`adopt_user_frame` + `map_page_pte` per page, ~10 pages per fault) — batchable
under one address-space hold; and every fault still takes the BKL for the whole
window (`fault_bkl_drop_enabled` exists in `akuma-bkl::policy`, unwired on amd64).

## 6b. On the metal (trashcan, 2026-10-02)

Same loop as `amd64-j4-build-crash-hunt.md` §1a (`amd64_metal_j4_loop.sh`, whole
`/root/ktarget` wiped per run, `kbuild -c -j 4`, SMP=4, 16 GiB, USB root), `kot`
switched off by the owner. Both kernels built on the Mac, **plain** config (no
`no-tests`, so both run the boot suite — a boot-time difference only), installed
by `cp` over `/boot/akuma-amd64`, one reboot each.

| arm | runs | wall | `[BKL] stuck` | verdict |
|---|---|---|---|---|
| baseline `67e87042` | 21, 22 | 594 s, 571 s | 55, 49 | PASS, PASS |
| new `5152981c` | 31, 32 | **177 s, 114 s** | 45, 61 | PASS, PASS |
| new, `no-tests` (quiet boot) | 33 | 173 s | 42 | PASS |

**~3-5x on the metal** (571-594 s → 114-177 s; the earlier 2026-09-30 batch was
836-905 s with `kot` running). 0 `[Fault]`, 0 SIGSEGV, 0 `[TLB] stuck`, no `EIO`,
and no `[exec] read_image: waited` line (the metal's 1 GiB heap never ran short).
The ryzen guest gained 22-33 %; the metal gained far more because its per-fault
cost was higher — 510 k file faults each doing a 64 KiB `ext2` read off a USB
disk that the shared cache made unnecessary. `[BKL] stuck` did **not** fall here
(42-61 per build against 27 on ryzen before the fix), so metal has a different
BKL holder still worth naming.

Caveats: two runs per arm; run 21 started with `kot` on and 22 with it off (the
two differ by 4 %); the faster pair, 114 s vs 177 s, shows cold-vs-warm variance
the sample cannot size. The kernel left installed is the `no-tests` build
(6,736,840 B, md5 `189dad3f8117`): **a plain `cargo build` drops quiet boot**
(`splash` is on by default only for `no-tests`).

## 7. Still open

* The structural fix is not to hold the whole image: `execve` could map the ELF
  from the file (as the AArch64 loader does) and the wait would go. The wait
  bounds the failure; it does not remove the 158 MB-per-exec heap demand.
* The metal's `execve` `EIO` (`amd64-j4-build-crash-hunt.md` §1a) should be
  re-run with the console line before its USB explanation is trusted.
* The remaining `tag=11` line in 0-2 runs: who still holds the BKL long in
  `munmap`.
* A runtime `TARGETED` switch so the A/B can be same-binary.
* Next lever by the same method: re-run `[PSTATS]` on the 70 s kernel; `munmap`
  should have dropped out of the top.

## Background

- `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §8-§10, §19 — the per-call work this
  builds on and the scaling table it answers.
- `docs/runbooks/selfhost-kernel-build-amd64.md` § "Second rig: ryzen" — the
  27 unexplained `tag=11` lines.
- `AMD64_TLB_STUCK_AS_LOCK_2026-10-01.md` — the shootdown wait's termination
  argument this change must preserve.
