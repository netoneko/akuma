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

## Open: SMP=4 build load kills the guest — one cause found and fixed, one reproduced (2026-09-17)

With the BKL fixes in, a `cargo build -j4` at 4 vCPUs ran ~10 minutes and
then died: a **ring-3 `#UD`** (rip=0x1009266a0, cs=0x23 — a userspace
process fetched an invalid opcode, i.e. executed a page that should not hold
that) followed by the guest leaving the network (`10.0.2.15` ARPs dead,
console shows the exception dump and nothing after). This is the
"reads serving zeros under memory pressure" family from the aarch64
self-host history (§5.1a-era rustc ICEs, Defect B). Console evidence saved
on the FC host as **`/root/akuma-fc2.crash-smp4-ud.log`**.

### Cause 1 — FIXED: the file-page cache leaked one CoW reference per freshly filled page

`amd64/src/mm.rs`'s `fill_file_pages` miss arm took a third `cow_ref_inc`
("this mapping's reference") before `akuma_fpcache::insert`, which takes the
cache's own reference itself, and `populate_file_page_by_inode`'s
allocation already carries the mapper's reference
(`map_and_track_pte`/teardown are the inc/dec pair). Three refs in, two out
(munmap and the next write's `invalidate_inode`) — **one reference stranded
per freshly filled shared file page**, so the frame could never be freed.
The AArch64 original never had the extra increment (its fill carries the
allocation reference into `adopt_user_frame(frame, owns_ref=true)` — one
allocation ref + one cache ref).

Every `write(2)` to a file invalidates that inode's cache entries
(`invalidate_inode`, via ext2's inode-freed hook), so a workload that
rewrites files re-filled and re-stranded on every rewrite: **exactly one
leaked frame per page per rewrite**, measured at 256 KB per 256 KB rewrite.
`cargo build -j4` rewrites thousands of files over ten minutes → ~2.5 GiB
stranded → `pmm_free=0` → every downstream symptom: fork/report EAGAIN, the
ring-3 `#UD`, the dead network.

Evidence trail (all on the FC box, KVM, vcpu_count=4):

```
execleak2 mode=file (rewrite churn, cache on):  freeram 2553 → 1553 MB, −256 KB/rewrite, linear
execleak2 mode=file, SHARED_FILE_PAGES_ENABLED=false: 16000 cycles, flat 2553 MB  ← A/B
execleak2 mode=remap (cache hits, munmap):      flat — mapper refs are balanced
execleak2 modes mt/mtexec/mtfork/mtforkexec/madv: all flat — fork/exec/madvise leak nothing
kill-line forensics at pmm_free=0: fpcache_len=19 (cache nearly empty!), cow_ref_frames=653587
per-site counters: fill-inc=653587, insert-inc=653587, invalidate-dec=653568, munmap-dec=28632
```

`fpcache_len` pinned near zero rules out cap/eviction; `cow_ref_frames` at
~653k with hits=0/misses≈cycles×64 pins the stranded third reference. The
fix is the deletion of that one `cow_ref_inc`; post-fix, 16,000 rewrites run
flat at 2552 MB and the full smpstress file churn no longer drains.

### Cause 2 — REPRODUCED, still open: fork's share pass races a sibling thread's faults

With the leak fixed, the full smpstress still drained PMM to its floor —
and bisecting the worker shape (`execleak2` mode `w`/`w4`, below) isolated
the trigger: **fork executing on one thread while a sibling thread is
faulting in / madvising its own region concurrently**. At 4 vCPUs with four
such workers, in about a minute — no memory pressure required
(`pmm_free=587281`):

```
[Fault] #PF write to not-present page, cr2=0x30, rip=0x403460, pid=796
[Fault] #PF instruction fetch, rip=0x0,  cr2=0x0,        pid=60
  [memwatch-at-kill] pmm_free=587264 fpcache_len=0 cow_ref_frames=66347
```

The first is the aarch64 Defect-B signature (a pointer field in a live page
read back as null → write through `NULL+0x30`); the second is a process
that **jumped to NULL** — the same terminal as the cargo `#UD` (execute a
page that cannot hold code). 66k frames sit stuck in the CoW ledger
afterwards. So the SMP=4 crash is a **fork-share-pass vs concurrent
sibling-fault race** — lost PTE demotes or lost refcounts — and the OOM
floor from cause 1 was almost certainly what made it rare enough to look
like a 1-in-N, ten-minute event.

### The probes (`userspace/amd64/{smpstress,execleak,execleak2}/`)

Static musl C probes, injected with `debugfs` into `/probes/`, booted as
`INIT=` (mechanism: `scripts/utils/amd64_mem_trials.py::inject_local`,
FC: `scripts/utils/hpbox.py::firecracker`). This loop — probe, not cargo —
is what turned a 1-in-N ten-minute crash into a one-minute repro.

- `smpstress` — the original fast-iteration probe: 4 workers × (2 churn
  threads + exec churner + file checker), per-thread pattern-verified
  ballast, madvise/mmap churn, CoW grandchildren, shared file rewritten
  under mapping. Found cause 1; post-fix it is the end-to-end regression
  gate.
- `execleak` — freeram (sysinfo) across N fork+exec+waitpid of /bin/hello.
- `execleak2` — the bisect tree, one mode per shape: `file` (rewrite under
  mapping — caught cause 1), `remap` (cache hits + munmap), `mt`/`mtexec`
  (fork from main), `mtfork`/`mtforkexec` (fork from a thread), `madv`,
  and `w`/`w4` — one faithful smpstress worker (or four concurrent), knobs
  `f`=file-checker `x`=exec-churner `g`=grandchild-CoW-write. **`w4g` is
  the current minimal reproducer of cause 2**; the mode that finally caught
  it runs the churn+fork on the holder threads themselves, so the fork
  share pass races a sibling's fault-in/madvise — every earlier shape
  (paused holders, churn on main) was flat.

Probe-side lessons (each false positive looked exactly like the bug):
rotating-window checks must shift the seed with the window; a page
re-verified against a recently read tag can be legitimately rewritten in
between; if the version tag lives in the page the fill must write it there;
`/bin/hello` is the self-check ELF (healthy exit **0x7F**, argv[0] must be
`"hello"`); guest `time()` stretches ~4× under full TCG saturation — bound
loops by iteration count too; and a boot with no NIC never runs
`mem_watch_tick`, so OOM/fpcache forensics ride the ring-3 kill line
(`[memwatch-at-kill]`, permanent since this investigation).

### Instrumentation kept

The ring-3 kill path now prints `[memwatch-at-kill] pmm_free= fpcache_len=
fpcache_cap= cow_ref_frames=` plus the `[FPCACHE]` hit/miss/evict/inval
line — the two numbers that split "the cache ate the RAM" from "a mapper
leaked refs" at the moment of death, which no other surface on this target
exposes (a NIC-less boot never reaches `mem_watch_tick`, and a saturated
guest never idles into the `[PSTATS]` sweep).

### Status and next steps

- **Cause 1 fixed** (one-line reference fix in `amd64/src/mm.rs`), verified:
  16k rewrites flat, boot suite 707/707 SMP=1 and 717/717 SMP=4, clippy
  clean, full smpstress on FC no longer OOMs.
- **Cause 2 reproduced, not yet fixed.** Next: instrument the fork share
  pass (`usermode::fork_share_memory`) and the demote's ranged shootdown
  against concurrent `fault_in`/`madvise` on sibling threads — the `w4g`
  mode reproduces in ~1 minute at SMP=4 on the FC box, which is fast
  enough to A/B candidate fixes. The stuck `cow_ref_frames=66347` says at
  least one path loses a decrement or an inc lands untracked; the
  NULL-write/NULL-exec faults say PTE content is also getting lost, so
  this is likely more than a refcount bug.
- `[TRAMP-MISMATCH]` fires ~500–800×/run under exec churn (aarch64
  ancestor: `KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`) — map-first resolution
  defends the trampoline, but treat a non-zero steady-state rate as a
  counter to drive to zero.
- The FC box kernel: the fixed build (with the kill-line forensics) is
  staged at `/root/akuma/target/x86_64-unknown-none/release/akuma-amd64`;
  the pre-investigation binary is backed up beside it as
  `akuma-amd64.pre-smpstress`.

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
