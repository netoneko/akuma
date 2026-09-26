# A silent, uninstrumented wedge caught live in a Firecracker microVM

**Date:** 2026-09-25. **Branch:** `even-more-cats` (host repo; kernel binary itself
predates the branch's HEAD by ~3 days — see caveat in §1).
**Kernel image:** `/tmp/akuma-fc.bin` inside the Lima `fc` VM, 3167 KB, `smp-shared`
2-core build, Firecracker v1.16.1, `machine-config.vcpu_count=2`.
**Architecture: AArch64.** The Lima `fc` VM runs on an arm64 Mac under `vz`, so
the Firecracker guest is the AArch64 kernel (`docs/runbooks/run-on-firecracker.md`),
not the amd64 one. That matters when comparing against amd64 sightings: the
scheduler, switch path and BKL call sites differ (see §6).
**Status:** **open, not root-caused.** Found by accident while diagnosing unrelated
high host CPU; the wedge itself was not reproduced on demand, only caught mid-flight
and killed.

**One line:** both vCPUs of a Firecracker-hosted akuma kernel sat at ~91% CPU each for
**19.5+ hours** with **zero new console output**, and the evidence rules out the
obvious explanation (a permanent spin in the instrumented `KernelLock::acquire` wait
loop) — something *else*, with no diagnostic on its path at all, is what's actually
spinning.

Current-state doc to read first: [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
→ "The `POOL` gate (shared-SMP)", which documents a *different*, already-open
"alive but unscheduled, no crash" wedge in the same kernel family. This doc is a
second, distinct sighting of that general shape and does **not** establish that the
two share a mechanism.

## 1. How this was found

Not a deliberate stress run. `limactl` (the macOS Lima VM host process, `vz` backend)
was reported at ~400% host CPU during unrelated work. Host-side triage found:

```
$ limactl shell fc -- ps -T -p <firecracker-pid> -o tid,pcpu,time,comm
    TID %CPU     TIME COMMAND
 442206  0.0 00:00:22 firecracker
 442208  0.0 00:00:00 fc_api
 442211 91.4 20:18:36 fc_vcpu 0
 442212 91.4 20:17:48 fc_vcpu 1
```

confirmed live and steady across a re-sample a few seconds apart — not a one-off
sampling artifact. Firecracker's own API agreed the VM thought it was healthy:

```
$ curl --unix-socket /tmp/fc.sock http://localhost/
{"id":"anonymous-instance","state":"Running","vmm_version":"1.16.1", ...}
```

The kernel binary was already 3 days stale relative to the branch's current HEAD at
capture time (built 2026-09-22 23:03, branch head at capture 2026-09-25 20:50), so
**this is evidence from a slightly older build, not necessarily the current one** —
flagged rather than silently treated as current-HEAD.

## 2. The last thing it said

`/tmp/akuma-fc-boot.log`'s mtime had frozen at **04:03**, ~2.5 hours after the VM
booted (01:26) — i.e. it went silent very early and then burned two full cores doing
*something* for the next ~19.5 hours with nothing reaching the console. The final
lines, from a periodic diagnostic dump (`[T7740.NN]` thread/pipe/futex dump, `[bkls>]`
BKL spin samples, `[EXCC]`/`[IRQS]` counters — unrelated to the wedge itself, just the
last scheduled heartbeat that got to finish):

```
[bkls>] core=1 ticket=34160057 serving=34160056 owner=2 spins=1048576
[bkls>] core=1 ticket=34160057 serving=34160056 owner=2 spins=2097152
[bkls>] core=1 ticket=34160057 serving=34160056 owner=2 spins=4194304
[herd] Reloading config...
```

Read literally: **both numbers are `aff0 + 1`** — `acquire` prints `me = core_id + 1`
(`crates/akuma-bkl/src/sync.rs:621`) and `owner` stores the same encoding. So
`core=1` is **core 0**, holding ticket `serving + 1`, waiting on `owner=2` =
**core 1** to release the BKL, and its spin counter is climbing by doubling. (A
first draft of this doc called the waiter "core 1" and the owner "core index 1",
which reads as a core waiting on itself; it is not. Corrected 2026-09-26.)

**Read these lines against the source the binary was built from, not HEAD.** The
image was built 2026-09-22 23:03, i.e. from `66416bc9` or earlier. The
"only the next waiter in line samples" gate (`sync.rs:757` today) and the
`[BKL] stuck` episode de-duplication (`STUCK_REPORTED`/`claim_stuck_report`,
`sync.rs:116-162` today) both landed in `a33988d5` on 2026-09-25 20:50 — after the
build. In the binary that ran, every waiter printed `[bkls>]` at every power of
two past 2^20, and every waiter printed a bare `[BKL] stuck: owner= waiter= tag=`
line every `SPIN_WARN_THRESHOLD` spins, with no folding
(`git show a33988d5^:crates/akuma-bkl/src/sync.rs`, lines ~704 and ~713). With two
vCPUs there is only one possible waiter, so the first difference changes nothing
here; the second makes §3's argument stronger, not weaker.

No panic, no assertion, no `TTBR SAVE-MISMATCH`/`LOAD-MISMATCH`/`SGI-S POISON` line
anywhere in the log — the anomaly-conditional prints scheduler.md and the PMM/TALC
wedge doc both rely on as tripwires never fired here either.

## 3. Why "it's still spinning in that exact loop" doesn't fit the evidence

This is the useful part of the sighting, and it took reading
`crates/akuma-bkl/src/sync.rs` to see: **the `[bkls>]` print is not the only
tripwire on this wait loop, and the other one is inconsistent with a simple
continuation of what's shown above.**

- `[bkls>]` only samples at power-of-two `total_spins` starting at `1_048_576`
  (`sync.rs:755` today; the power-of-two test is unchanged from the built source) —
  so if this exact core were still spinning in this exact loop, the next line due is
  `spins=8388608` (2^23), then 2^24, and so on. The code's own calibration is that
  2^20 spins is about **one second**, so 2^23 was due within seconds of the 2^22 line
  — yet **zero** further `[bkls>]` lines exist anywhere later in the log.
- Separately, `spins` (a *different*, resettable counter) trips `log_kernel_lock_stuck`
  — the `[BKL] stuck` line — every `SPIN_WARN_THRESHOLD = 10_000_000` spins
  (`sync.rs:767`, `986`). In the built source that line had **no de-duplication at
  all** (see §2), so every 10M spins printed one. 10M is about ten seconds of
  spinning; 19.5 hours would have printed thousands.
- **Zero `[BKL] stuck` lines exist anywhere in this boot's entire log**, not just after
  04:03. That is the load-bearing fact: core 0 never reached `SPIN_WARN_THRESHOLD`
  spins in this loop at any point in the session.
- The two thresholds together bound it tightly: the last sample was 2^22 and 2^23
  never printed, so **this wait ended between ~4.2M and ~8.4M spins — within a few
  seconds of the last line**, well before the 10M stuck threshold was even in play.
  The three escalating samples are not a snapshot of an eternal spin; whatever they
  were waiting on resolved, or the thread left the loop some other way.

**Conclusion the log supports:** the final state — two vCPUs at ~91% CPU for 19.5
hours with no output — is **not** a continuation of the printed BKL ticket wait. The
kernel left that instrumented path shortly after the last line above and entered a
different busy loop that has **no diagnostic on it at all**. What that loop is remains
unknown; candidates not ruled out:
- the `POOL` gate wedge from [`scheduler.md`](../reference/subsystems/scheduler.md)
  (a `try_lock` failure loop that, per that doc, prints
  `[SGI] POOL contended, skipped N ticks` — but only from the *interrupted* thread on a
  still-ticking core; a POOL-gated wedge stops timer IRQs from landing on the holder's
  own core, so if both vCPUs ended up wedged this way the print could plausibly go
  silent too, unlike the single-core cases scheduler.md documents),
  or
- an uninstrumented tight loop elsewhere entirely (device poll, an IRQ handler retry,
  userspace busy-loop with preemption already lost) that this session did not
  identify.

Both are speculative; nothing here proves either. This is flagged as the open
question, not resolved.

## 4. What's missing to actually root-cause it

- **No debugger was attached.** Unlike the QEMU-hosted wedges in
  [`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md), this
  was a **Firecracker** microVM, not QEMU — `GDB=1` / `scripts/lockprobe.py`
  (`../reference/scripts/multi-vm.md`) target QEMU's gdbstub and do not apply
  as-is. Firecracker exposes no gdbstub by default; confirming the actual stuck PC on
  a future sighting needs either a KVM-side debug attach or an in-kernel capture (see
  next point) rather than the existing tooling.
- **No owner-attribution capture exists for the BKL**, same gap scheduler.md notes for
  `POOL`: nothing in `KernelLock` records *where* the holder last entered the kernel
  beyond the `HOLDER_TAG`/profiler path, which is only populated when
  `PROFILE_ENABLED` is set. Whether that was on for this run is unknown.
- The instance was killed (to reclaim the two host cores) before any of the above
  could be tried. A boot log was archived from inside the Lima VM at
  `/tmp/akuma-fc-boot.2026-09-25-bkl-livelock.log` (host-local, not in this repo) in
  case a future sighting benefits from comparing against it.

## 5. How to catch the next one

- Watch for `limactl` (or any Lima-hosted Firecracker instance) sitting at high,
  *steady* CPU with no console growth — `stat` the boot log's mtime against wall
  clock, and `ps -T -p <firecracker-pid>` for `fc_vcpu N` threads pegged with no
  `%CPU` variance across repeated samples a few seconds apart. That combination (hot
  + silent + steady) is the signature; a merely busy/legitimate workload keeps writing
  to console or shows CPU% moving as it processes things.
- If caught again: **before killing it**, try to get `PROFILE_ENABLED` state,
  `STUCK_SUPPRESSED`/`kernel_lock_stuck_suppressed()`, and
  `kernel_lock_lost_ticket_recoveries()` off the running instance (e.g. via a debug
  syscall or console command, if one exists) to see whether the BKL's own counters
  moved after the log went silent — that alone would settle whether §3's "it left the
  loop" reading is right.

## 6. Not the same sighting as the amd64 `[SWITCH NO-BKL]` hangs

Two amd64 bare-metal hangs photographed 2026-09-26 also end in silence shortly
after `[herd] Reloading config...`, with `[BKL] stuck`/`[bkls>]` lines just before.
Their last line is `[SWITCH NO-BKL] from=4 to=2 core=2 via=yield_now` — a context
switch taken without the BKL through `amd64/src/sched.rs`'s `yield_now`, which that
kernel has since stopped allowing (`AKUMA_AMD64_BKL_NETWORKING.md`, 2026-09-26
section). That mechanism is x86-only (`akuma-threading`'s `x86_yield_now`); this
sighting is AArch64 and printed no switch report. The `herd` reload preceding both
is a coincidence worth one look, not evidence of a shared cause.

## Background

- [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md) —
  "The `POOL` gate (shared-SMP)", the other open "alive but unscheduled" wedge.
- [`PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md`](PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md) — a
  *resolved* silent wedge with the same outward shape (~400% CPU, no console, no
  panic), for contrast in method: that one was caught under QEMU with a live debugger
  attach, which is exactly the tooling gap this sighting hit under Firecracker.
- `crates/akuma-bkl/src/sync.rs` — the ticket lock, its `[bkls>]` sampling gate
  (`:744-766`), the `[BKL] stuck` dedup/reprint logic (`:116-162`), and the two spin
  thresholds (`:986`, `:993`). Line numbers are HEAD as of 2026-09-26; the image in
  this sighting predates the gate's next-in-line check and the dedup
  (`a33988d5`) — see §2.
