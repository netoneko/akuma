# Self-host build slowness on amd64 — investigation and first fix

*Investigation of 2026-09-16/17, worktree `profiling/amd64-build-slow`.*
*Status: first fix landed (CR3-skip on same-root switches); measured A/B on
real hardware still pending — the FC box was unreachable for the final arm.*

## Question

In-guest `cargo build` (self-host, Firecracker, 1 vCPU) was reported ~50x
slower than the aarch64 self-host. Where does the time go, and what can be
taken out?

## Measurements

| what | where | time |
|---|---|---|
| clean workspace build `-p akuma-amd64`, `x86_64-unknown-none` | host (M-series, 4 cores) | 15.9 s wall / 57.6 s CPU |
| clean workspace build `-p akuma`, `aarch64-unknown-none`, devbox-smoltcp,no-tests | host, same machine | 15.7 s wall / 59.3 s CPU |
| **akuma-exec single-crate rebuild, x86_64 target** | **host** | **3.05 s** (7.4 s CPU, 2.8 cores avg) |
| akuma-exec single-crate rebuild, aarch64 target | host | 2.0 s |
| akuma-exec single-crate rebuild, `-j1` | **in-guest amd64 (FC, 1 old vCPU)** | **102 s** |
| akuma-primitives single-crate rebuild | in-guest amd64 | 2.0 s |
| akuma-bkl single-crate rebuild | in-guest amd64 | 0.74 s |

Reading:

- **Building *for* amd64 is not the problem.** Same host, same tree, both
  targets: 15.9 vs 15.7 s. The toolchain cost is target-independent.
- **The 50x is runtime.** The same crate (akuma-exec), the same target
  (x86_64 codegen), takes 102 s in-guest vs 3 s on the host.
- Accounting: the FC box's core is ~3.5x slower per-core and the guest runs
  1 vCPU vs the host's 2.8-core average parallelism → ~10x expected. The
  observed 33x wall / 14x CPU leaves a **~3x residual that is kernel work in
  the guest**, not emulation and not hardware.
- Small crates (bkl 0.74 s, primitives 2 s) are fine: the cost concentrates
  in big single-crate compiles, i.e. where rustc's *runtime behaviour*
  (mmap/fault churn, futex/condvar jobserver churn, many switches) is heaviest.

A samply flamegraph of the host rustc compile is at
`/tmp/akuma-exec-profile.json` (session artifact) — the phase shape to compare
the in-guest `-Z self-profile` run against.

## Kernel audit — where the residual lives

Ranked, with the top one fixed here:

1. **Full TLB flush on every same-root context switch** — *fixed, see below.*
   `hook_switch_to` rewrote `CR3` for every switch into a process, even when
   the root was already live (`amd64/src/sched.rs`). `mov cr3` flushes the
   whole non-global TLB; with no PCID, rustc's working set (tens of MB of
   rlibs) re-page-walks after every switch. rustc's jobserver/condvar churn
   switches thousands of times per second, mostly between sibling threads of
   one process — every one of those paid a full flush.
2. Two `wrmsr` per switch (`IA32_FS_BASE`/`IA32_KERNEL_GS_BASE`) — suspected
   VM exits under KVM, but KVM passes FS/GS-base MSRs through by default, so
   likely **not** a real cost. Low priority; skip GS restore when unchanged is
   the cheap half-measure.
3. No shared zero page: every anonymous *read* fault allocates and zeroes a
   fresh 4 KiB frame (`amd64/src/mm.rs` `populate_page`). Linux maps a global
   zero page read-only and CoWs on write. Real win for rustc's sparse arena
   mmaps; medium-size change through the CoW path.
4. virtio-blk `read_bytes` allocates a sector-aligned temp `Vec` and
   double-copies every read (`crates/akuma-virtio/src/block.rs`); ext2
   re-derives block mappings per page (kernel-measured 2.2 µs/page,
   `amd64/src/mm.rs`). Amortized by the 16-page file-fault readahead, so
   mostly a constant tax.
5. BKL held across fault servicing (`amd64/src/idt.rs`); the
   `fault_bkl_drop_enabled` toggle exists in `akuma-bkl::policy` but is not
   wired on amd64. Small at SMP=1.

Already fine (checked, do not re-litigate): munmap/mprotect TLB strategy
(per-page invlpg + one ranged flush, zero IPIs at SMP=1), fpcache wiring +
readahead, futex park/wake protocol, 10 ms LAPIC tick handler, console
silence on hot paths.

## The fix (this worktree)

`amd64/src/sched.rs` `hook_switch_to`: activate only when
`want != paging::active_root()`.

Why this is sound and not the bug the old comment feared: the old comment
worried that "same CR3 value" could be a *recycled* frame — a freed root
reissued to the next process. That hazard is closed upstream: `akuma-mmu`'s
`free_or_defer_as_frames` refuses to free an L0 that any core's live CR3
(`any_core_on_l0`, fed by `publish_l0_begin`/`publish_l0_end`, which amd64's
`paging::activate` calls on **every** write) or any saved context still
references — it parks the frames for a later drain instead. So equality
between `want` and this core's live CR3 proves the root was never freed under
us: it is the same live address space and there is nothing to flush. The
`[SWITCH FREED-CR3]` tripwire still runs on every switch-in.

Verified: `cargo build -p akuma-amd64` clean; clippy clean; **boot suite
707 passed / 0 failed** under local QEMU TCG, zero `FREED-CR3` /
`SWITCH NO-BKL` tripwires, suite exercising 107 scheduler parks.

**A/B on real hardware (2026-09-17, FC guest, kernel rebuilt from this
worktree, md5 `f5797fe3`)**: the anchor `cargo build -p akuma-exec
--release --offline -j1` went **102 s → 83.5 s** wall (two trials 83.88 /
83.54 s, ±0.3 s; prior-kernel figure was a single trial on the stale 0.0.7
boot, so treat the delta as ~18% ± a few). Same-root switches no longer
flush. **A ~2.5x kernel residual remains** (83.5 s vs 3 s host; ~10x is
hardware). Candidates 3–5 below are the remaining levers, and the first
at-home action is a *non-invasive* utime/wall attribution: this session's
`/proc` scan sampler cost ~30% of the CPU by itself (60 pids × per-cat cost
every 2 s) and its utime reads came back 0 — sample one known rustc pid on
a long interval, or get per-pid CPU out of the kernel instead.

## What the profiling sessions turned up along the way

- **aarch64 devbox thread-spawn cap + temporary fork exhaustion.** A process
  spawning ~1000+ threads (in a loop with joins) dies with `EAGAIN` on
  `clone`, and *afterwards the whole guest cannot fork* for ~45 s — some
  slow reaper eventually recovers it. Reproduced with a 15-line
  `thread::spawn` loop on kernel 0.0.8, SMP=4. rustc at `-j1` doesn't get
  near this, but any `-jN`/threadpool-heavy workload can. Also caps the
  `jobserver_stress` probe: set `JS_SPAWN_ITERS=150`.
- **/proc bloat on long-lived amd64 boots**: the stale 0.0.7 boot answered
  `ls /proc | wc -l` with **16088** entries (60 on a fresh boot), nearly all
  of them stat-less. Anything scanning `/proc` (busybox glob, top, ps) crawls.
  Worth a look at slot recycling on the amd64 threading side.
- **An FC guest wedged hard mid-build** (console silent, ARP dead, no panic)
  during a `-Z time-passes` rebuild — the Defect-A family, unreproduced.
  Console evidence saved on the FC host as `akuma-fc.log.wedged-*`.
- **Stale `aktap0` route on the FC host** blackholes the guest after a VM
  restart (`10.0.2.15 dev aktap0 scope link linkdown`); fix is
  `ip route del 10.0.2.15 dev aktap0`. Cost an hour of "No route to host".
- `top`/`ps`/`free` on amd64 **already work** (real procfs mounted
  2026-09-08, `/proc/stat` rendered from live `akuma-threading` CPU-time
  counters; `mkdisk.sh`'s "still broken" comment is stale). Caveats: stime is
  always 0 (no user/kernel split), idle ticks derived, 10 ms quantization.

## Iterate (the at-home plan)

1. ~~Rebuild + reboot the FC guest from this worktree's kernel~~ **done**
   (kernel staged over the old one; pre-fix copy kept on the FC host as
   `akuma-amd64.pre-c3skip` for instant rollback).
2. ~~Re-time the anchor~~ **done — 83.5 s, see above.**
3. **Probe A/B** — `scripts/benchmarks/selfhost_probe_ab.sh` works on both
   arms. Same binary, same knobs (`userspace/forktest/selfhost_repro/
   jobserver_stress.rs`, binaries in `userspace/forktest/c_stress/`):
   aarch64 devbox (4 cores HVF) = all phases 1 s, condvar-5M 4 s, park-3M
   1 s; amd64 FC (1 old vCPU) = all phases 307 s (spawn-join dominates),
   condvar-5M 3 s, park-3M <1 s. The condvar/park phases are
   userspace-atomic-bound (uncontended std Mutex/Condvar never enters the
   kernel), so they measure the *cores*, not the kernel — the kernel-relevant
   phases are spawn-join and the barrier. Two portability traps hit on the
   way, both now handled in the script: rust's x86_64 musl target defaults
   to **static-pie**, which the amd64 loader SIGSEGVs on — build probes with
   `-C relocation-model=static`; and the amd64 rootfs has busybox without a
   `base64` applet link, so binary pushes go through `busybox base64 -d`.
   Also: a nested ssh (laptop→HP box→guest) re-splits multi-word remote
   commands at each shell layer — guest scripts must travel as staged files
   (`cat > /_probe_ab.sh`), never as inline text.
4. **Attribute the rest in-guest**: `-Z self-profile` on akuma-exec (writes
   `.events`, pull out and open in profiler.firefox.com), and a *non-invasive*
   CPU-time/wall split — the `/proc` scan sampler in this session cost ~30%
   of the guest's CPU and its utime reads came back 0 (fields 14/15 read 0
   even for a process mid-compile; worth checking `akuma-procfs`'s amd64
   stat rendering before trusting it). Sample one known rustc pid on a long
   interval instead, or read the counters kernel-side.
5. If the wedge recurs: Firecracker has no gdbstub, so the console tripwires
   (`[SWITCH FREED-CR3]`, `[BKL] stuck`, PSTATS) and the saved console log
   are the evidence path.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the self-host bring-up stages.
- `docs/runbooks/selfhost-kernel-build.md` — aarch64 self-host procedure,
  detach/poll mechanics, and the aarch64 baseline numbers (44 s clean build).
- `docs/archive/COW_PILE_AUDIT.md` §10 — the freed-L0 hazard this fix leans
  on the liveness gate for.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — procfs on amd64.
