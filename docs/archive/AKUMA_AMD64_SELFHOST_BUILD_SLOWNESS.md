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

## The fixes (this worktree)

Three landed, each verified by the 707-test boot suite under local QEMU TCG
and then by anchor re-timing on the FC box:

1. **CR3-skip on same-root context switches** (`amd64/src/sched.rs`
   `hook_switch_to`): activate only when `want != paging::active_root()`.
   Sound because `akuma-mmu`'s L0-free liveness gate refuses to free a root
   any core's live CR3 references (see the in-file comment). **102 s → 83.5 s
   (−18%).**
2. **Untimed futex waits no longer `hlt` before their wake check**
   (`amd64/src/futex.rs` `wait`): the loop's `allow_tick()` ran before the
   membership test, costing every untimed wait 1–2 LAPIC ticks (10 ms each)
   of pure latency on a core whose waker was runnable. Timed waits keep the
   window — their deadline reads `uptime_us`, which does not advance while
   `IF` is clear. **83.5 s → 77.9 s (−7%).**
3. **`allow_tick` releases the BKL across the `hlt`** (`amd64/src/sched.rs`),
   and **the netpoll drain runs BKL-free** (`amd64/src/net.rs`
   `netpoll_daemon`, scoped exactly like `kernel-glue`'s
   `netpoll_drain_step`). Together these fix the SMP>1 boot: at
   `vcpu_count=4` the netpoll daemon's near-continuous BKL ownership put all
   peer cores into a `[BKL] stuck` storm (owner=1 tag=503) and sshd never
   answered; with the fix the 4-vCPU guest boots and serves ssh with **zero**
   stuck lines. Two dead ends documented for the next person: parking
   *inside* a dropped window breaks the resume protocol at SMP
   (`[SWITCH BADFRAME]` → `#PF` fetch from 0x0), which is why the window is
   scoped to the drain alone and the park is made harmless instead.

Along the way, aarch64-parity instrumentation this target lacked: per-process
syscall counters + coarse per-syscall times + page-fault counts
(`ProcessSyscallStats`, bumped in `syscall_handler` and
`page_fault_dispatch`, dumped by a 30 s sweep in `idle_loop` — no PSTATS
existed on amd64 before), and the `[mmap-t]` per-mapping timing print is now
behind `mm::MMAP_TRACE` (it fired on every ≥16-page mmap; an in-guest build
flooded the UART with it while compiling).

**Anchor is now 77.9 s (−24% total) at 1 vCPU.** Host A/B says building *for*
amd64 costs the same as aarch64 (15.9 vs 15.7 s), so the remaining gap is
runtime; rustc's own `-Z time-passes` in-guest puts it in codegen/LLVM
(60 s + 45 s of the total).

## Probe A/B (`scripts/benchmarks/selfhost_probe_ab.sh`)

Same binary, same knobs, both guests
(`userspace/forktest/c_stress/jobserver_stress.{aarch64,x86_64}`):

| phase | aarch64 devbox (4 cores HVF) | amd64 FC (1 old vCPU) |
|---|---|---|
| all phases | 1 s | 307 s (spawn-join dominates) |
| condvar, 5M requests | 4 s | 3 s |
| park, 3M iters | 1 s | <1 s |

The condvar/park phases are userspace-atomic-bound (uncontended std
Mutex/Condvar never enters the kernel) — they measure the cores, not the
kernel; the kernel-relevant phases are spawn-join and the barrier. Portability
traps the script now handles: rust's x86_64 musl target defaults to
**static-pie**, which the amd64 loader SIGSEGVs on (build with
`-C relocation-model=static`); the amd64 rootfs's busybox has no `base64`
applet link; and a nested ssh re-splits multi-word remote commands at every
shell layer, so guest scripts must travel as staged files.

## Open: SMP=4 build load kills the guest (probe bring-up, 2026-09-17)

With the BKL fixes in, a `cargo build -j4` at 4 vCPUs ran ~10 minutes and
then died: a **ring-3 `#UD`** (rip=0x1009266a0, cs=0x23 — a userspace
process fetched an invalid opcode, i.e. executed a page that should not hold
that) followed by the guest leaving the network (`10.0.2.15` ARPs dead,
console shows the exception dump and nothing after). This is the
"reads serving zeros under memory pressure" family from the aarch64
self-host history (§5.1a-era rustc ICEs, Defect B). Console evidence saved
on the FC host as **`/root/akuma-fc2.crash-smp4-ud.log`**. Until root-caused,
treat `vcpu_count>1` + parallel builds on amd64 as unstable; the FC box is
restored to its original `vcpu_count=1` with the fixed kernel
(md5 `5b394360…`).

### The fast-iteration probe (`userspace/amd64/smpstress/`)

A cargo build is a ten-minute repro; the probe below catches the same family
in seconds and boots as `INIT=/probes/smpstress` (build: `x86_64-linux-musl-gcc
-static -O2 -pthread -o smpstress smpstress.c`; inject with `debugfs` into
`/probes/`, the same mechanism `scripts/benchmarks/amd64_fault_cost.py` uses;
boot `SMP=4 SSH_PORT=2444 HTTP_PORT=8484 MEMORY=3072 DISK=<img>
INIT=/probes/smpstress sh amd64/run.sh` from the worktree — ports 2222/8080
are taken by other VMs).

Shape, chosen to mirror what a `-j4` build actually does: 4 forked workers ×
(2 churn threads + file checker) — per-thread private anon region and a
**160 MiB ballast** filled with a per-thread pattern and re-verified on a
rotating window (8 threads live ≈ 1.3 GiB of the 3 GiB guest, so frames are
recycled under live mappings, which is the pressure the crash happened
under); per-iteration `madvise(MADV_DONTNEED)` + mmap/munmap churn; a fork
grandchild every 64 iterations writing a CoW copy; every process maps the
shared file `PROT_READ` and re-verifies it; a writer thread rewrites the file
in place (2789 versions in 5 min) while the mappings exist; and an
`execv("/bin/hello")` churn loop per worker (in flight — see below) to stress
loader/teardown of file-backed text at SMP, the #UD-shaped surface.

### Result so far: clean ≥5 min ×2 at SMP=4 under TCG — negative

Two full 5-minute runs (run5: anon+CoW+madvise+ballast+file-read; run10:
+ file-rewrite-under-mapping) **passed with zero pattern mismatches** at
SMP=4, boot suite 717/717 ahead of the probe, and the kernel tripwires the
crash should have left — `[SWITCH FREED-CR3]`, `[BKL] stuck`, `[SWITCH
NO-BKL]`, `[FUTEX-DUMP]`, `#UD` — never fired. The audit's two named
suspects are *not* cleared by this, but the obvious races in both are not
this shallow:

- **CoW window + shootdown under the BKL** (`amd64/src/idt.rs` ~919): the
  fault path takes the BKL for the servicing window, and the shootdown's
  ack-wait assumes every sender holds it — a peer resuming inside that window
  is what the design already argues away, and the probe did not break it.
- **`madvise(MADV_DONTNEED)` shared-frame memset** (the exact Defect B
  analogue): `dontneed_range` (`amd64/src/mm.rs`) already consults the CoW
  share count and breaks sharing with a fresh frame rather than zeroing a
  peer-visible frame; fpcache frames carry `cow_ref_inc`, so the ZeroInPlace
  arm does not wipe another process's cached page. The aarch64 defect's
  mechanism (`MADV_DONTNEED_SHARED_FRAME.md`) is not present in the amd64
  code in that form.

New instrumentation signal worth keeping an eye on (new under SMP=4 load,
self-correcting, fired ~6×/run during fork churn):

```
[TRAMP-MISMATCH] tid=23 THREAD_PID_MAP=353 but table scan found 352 — using 353
[unregister] pid=352 stale tid=23 now owned by pid=353
```

That is tid-recycling racing the trampoline's pid map during heavy
fork/exit — a benign-looking recovery path, but exactly the kind of window a
`#UD`-class bug hides behind if the recovery ever loses. Not implicated yet.
The aarch64 self-host history has the closest ancestor for this signal:
`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md` — a stale-tid window that only opened
at `-j4` (its guard fired 63× in one build) and whose fix preceded the first
clean `-j4` completion. Treat a non-zero steady-state rate of
`[TRAMP-MISMATCH]` during the next real build the same way the `[KTG-STALE-CH]`
guard was treated there: a counter to drive to zero, not noise.

### Probe-side lessons (read before touching the checker)

Three false positives, all mine, all worth remembering because each looks
*exactly* like the corruption family being hunted:

1. **Rotating-window verification must shift the seed with the pointer.**
   Checking `b + off` against `mix(seed + rel)` with an unshifted seed fails
   from the second window on, and the failing qword then legitimately reads
   as "the content from 512 KiB earlier" — a perfect wrong-frame-mapped
   impostor. (It consumed an hour; the "512 KiB shift = REGION_BYTES"
   numerology was seductive and wrong.)
2. **A page re-verified against a tag read moments ago can be rewritten in
   between** by a concurrent writer. A mismatch is only real if the tag is
   unchanged *and* the qword still mismatches on re-read.
3. **If the version tag lives in the page, the fill must write it there.**
   `mix(seed + i)` at every offset makes the tag qword read back as pattern
   data, and every check downstream mis-keys. Torn *pages* (different
   versions in different pages of one read pass) are legal under a
   concurrent writer; verify per page against that page's own tag, and
   give a first-seen version one pass before verifying it.

### Status and next steps

- **The premise held**: SMP=4 amd64 QEMU boots, serves the boot suite
  (717/717 — the SMP-only tests; the SMP=1 figure is the usual 707/707,
  re-verified 2026-09-17), and runs heavy memory churn cleanly on this
  branch — the `cargo build -j4` crash is not reachable by the generic
  probe under TCG.
- **Exec churn joined the probe and is clean.** The per-worker
  `fork`+`execv("/bin/hello")`+`waitpid` loop survived ~2600 cycles under
  full 4-vCPU saturation (run13), which also exercises the loader's
  file-backed text mappings and process teardown at SMP — the #UD-shaped
  surface. Two probe accommodations worth knowing: `/bin/hello` is the
  tree's self-check ELF, so a healthy run exits **0x7F**, not 0, and its
  argv check wants `argv[0] == "hello"` — exec'ing it as `"/bin/hello"`
  costs one probe bit (status 0x6F) and looks like a kernel failure.
- **Guest time stretches under full TCG saturation**: a `time()`-bounded
  churn loop ran ~4x its wall bound before noticing (the guest clock lags
  when all vCPUs are pegged). Bound long loops by iteration count too.
- **TCG may be hiding the crash** — TCG serializes vCPU timing in ways KVM
  does not. Next: run `smpstress` on the FC box at `vcpu_count=4` (config
  backup `/root/akuma-fc.json.vcpu1`, crash log
  `/root/akuma-fc2.crash-smp4-ud.log`) — the probe keeps FC iterations
  short, no cargo needed.
- If the FC probe run is clean too, the remaining divergence is *scale and
  shape*: the real build runs rustc (huge address spaces, thousands of
  mmap/futex ops per second, constant process churn) for ten minutes. Raise
  probe pressure toward near-OOM and add futex/condvar churn, and only then
  fall back to a real `-j4` cargo build on the FC box as the last resort.
- PSTATS (this branch's addition) works on this target and is the
  instrument for the next real build: `pgfault=` counts and per-syscall
  times appear in the 30 s `[PSTATS]` sweep (absent while all cores are
  saturated — the dump lives in `idle_loop`, and a saturated guest never
  idles).
- Verification state 2026-09-17: clippy clean (workspace minus
  `akuma-amd64` for the host target, `akuma-amd64` under
  `--target x86_64-unknown-none --release`), SMP=1 boot 707/707, SMP=4 boot
  717/717 twice, `smpstress` ≥5 min clean at SMP=4 twice and one
  ~30-minute saturated run with exec churn, zero CHECK-FAIL. Everything
  uncommitted.

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

## Iterate (what is left)

1. ~~CR3-skip~~, ~~futex hlt~~, ~~netpoll/allow_tick BKL~~ — **landed, see
   above**. The FC guest runs the fixed kernel; the pre-fix binary is kept as
   `akuma-amd64.pre-c3skip`.
2. **Root-cause the SMP=4 ring-3 `#UD`** (open item above) — it gates the
   biggest remaining lever, which is not a kernel change at all: rustc is
   single-threaded at `vcpu_count=1`, and the aarch64 self-host baseline
   (44 s clean build) had four cores. Once SMP builds are stable, re-time the
   anchor at `-j4`/4 vCPUs.
3. **Attribute the rest in-guest** with the new PSTATS counters: run a build,
   then read the 30 s `[PSTATS]` block (per-syscall counts, coarse times —
   blocking syscalls are the ones that surface — and `pf=` fault counts).
   rustc's `/proc/<pid>/stat` utime/minflt are stubs on this target (both
   read 0 mid-compile); PSTATS is the working instrument now.
4. Remaining audit levers if PSTATS shows fault/read domination: shared zero
   page for anonymous read faults; virtio-blk `read_bytes` temp-Vec; the
   syscall-entry opt-out bitmap (aarch64 `SYSCALL_BKL_OPTOUT_SEED`) — the
   *mechanism* ports as-is, but each seed needs its amd64 handler audited
   (e.g. `futex` cannot be seeded until `WAITERS` stops naming the BKL as its
   safety argument), and at 1 vCPU the uncontended BKL is cheap, so this pays
   only at SMP>1 — do it after item 2.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the self-host bring-up stages.
- `docs/runbooks/selfhost-kernel-build.md` — aarch64 self-host procedure,
  detach/poll mechanics, and the aarch64 baseline numbers (44 s clean build).
- `docs/archive/COW_PILE_AUDIT.md` §10 — the freed-L0 hazard this fix leans
  on the liveness gate for.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — procfs on amd64.
