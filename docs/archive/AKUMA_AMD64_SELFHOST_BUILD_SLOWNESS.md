# Self-host build slowness on amd64 — investigation and first fix

*Investigation of 2026-09-16/17, worktree `profiling/amd64-build-slow`,
folded to the main tree 2026-09-17 (merge e5ba8298).*
*Status: the 1-vCPU anchor landed (77.9 s, −24 %) and both SMP=4 crash
causes are fixed and FC-verified; the `-j4` run survives ~50 min / 31
crates with flat memory but still ends in a silent wedge behind a residual
ring-3 kill pair, and the build remains slow (rustc ≈44 % of wall
in-kernel, dominated by `read`) — see "The `-j4` verification run" and
"Iterate" below.*
*Continued 2026-09-17: four more fixes landed (PIT calibration off the
port-`0x61` glue `microvm` lacks, TSC-resolution `clock_gettime`, the
virtio-blk allocate-and-copy on every aligned read/write, and per-process
CPU time that was unconditionally zero on this target) — see "Continued
2026-09-17" below. Re-timing hit a NEW, distinct, root-caused-but-unfixed
wall: `mmap` placement is O(n²) in the region count, so a process doing many
small mmaps (rustc's own allocator) crawls to a halt once it accumulates
roughly four figures of regions — §5 of that section.*

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

## Open: SMP=4 build load — two crash causes found and fixed, one wedge left (2026-09-17)

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

### Cause 2 — FIXED (2026-09-17): fork's share pass raced a sibling thread's faults

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
page that cannot hold code). 66k frames sat stuck in the CoW ledger
afterwards.

**Root cause.** The `no-bkl-process` carve-out — whose design record is
`crates/akuma-exec/src/process/bkl_guard.rs` — drops the BKL around
`fork_process` step 4, so the share pass's safety does not come from the
BKL. It comes from the **owner's address-space lock**, held across a walk
whose per-page transition is `read PTE → cow_ref_inc → demote` as one atom,
and it is sound only because the CoW fault handler takes *the same lock*
for its break (aarch64's handler does: `owner.address_space.lock()` in
`src/exceptions.rs`, bkl_guard.rs constraint 3). The amd64 port skipped
both halves of that contract:

- `idt.rs`'s `cow_write_fault` serviced the break through a raw-CR3
  `new_shared` view with **no address-space lock at all**, so it interleaved
  with the BKL-free fork walk: fork reads leaf=X → the sibling's fault breaks
  X (`cow_ref_dec` → freed or remapped to Y) → fork incs the stale X and maps
  it into the child. Refcounts stranded (the 66k ledger), frames freed while
  still mapped — which surfaces as the freed-then-recycled-page corruption
  class: `cr2=0x30`, `rip=0x0`, and the ten-minute cargo `#UD`.
- `usermode.rs`'s `share_parent_memory_into` locked `parent.address_space` —
  the **forking thread's** `Process`. For a `CLONE_THREAD` sibling fork that
  is a shared-L0 view under a **fresh lock** nothing else in the system takes
  (bkl_guard.rs constraint 1), so the hold excluded nothing.

**Fix** (three edits, no locking added anywhere else):

- `amd64/src/idt.rs` `cow_write_fault`: resolve the owner via
  `address_space_owner_pid_for_fault()` → `lookup_process_shared()` and take
  the owner's AS lock for the whole servicing window — read → decide →
  rewrite → refcount → ledger swap is one critical section against the share
  pass. The Copy arm's ledger swap moved inside that hold, replacing
  `cow_swap_frame` (which additionally had the own-vs-tgid bug: it edited a
  sibling thread's throwaway view ledger, not the leader's).
- `amd64/src/usermode.rs` `share_parent_memory_into`: lock
  `lookup_process_shared(parent.tgid)`'s address space — the thread-group
  leader owns the live L0, and `tgid` is the leader's pid — so fork pass, CoW
  fault, and `madvise`/`munmap` (via `with_current_address_space`) all
  serialize on one lock object.
- `cow_swap_frame` deleted (single caller folded in).

No lock-order inversion: fork's hold never waits for the BKL (that is the
carve-out's point), and every other AS-lock taker holds BKL→AS. A `None`
owner (kernel root, ring-0 self-test) proceeds unlocked exactly as before.

Verification so far: boot suite + clippy clean on the amd64 target, host
tests green (identical pre/post warning counts on the host clippy — the 183
warnings are pre-existing and unrelated). FC-verified 2026-09-17 (KVM,
vcpu_count=4, 6 GB): **`execleak2 w4g` green** — 4 replicas × 100 churn
cycles, freeram flat at 2034 MB, zero `[Fault]` lines, `DONE w4`; pre-fix
the same mode died in ~1 minute. The `cargo -j4` run under it is §"The
-j4 verification run" below.

### The `-j4` verification run (2026-09-17, FC SMP=4, 6 GB) — big win, not done

`cargo build --release -p akuma-amd64 -j4 --offline` in the FC guest
(`/root/akuma-fc-rust.img`'s `/src/akuma`, `/usr/local/rust/bin` nightly,
booted by `/root/akuma-fc-run.sh` against `/root/akuma-fc.json`; guest sshd
listens on **2222**, not 22). Against the pre-fix kernel this workload died
at ~10 minutes with `pmm_free=0` → fork EAGAIN → ring-3 `#UD` → dead
network. On the fixed kernel:

- **~50 minutes and 31 crates compiled with memory flat** (`pmm_free`
  1.2–1.3 M frames throughout, fpcache healthy, `cow_ref_frames` ≈83 k —
  plausible live sharing, not the stranded-663 k signature). No OOM, no
  fork EAGAIN, no NULL-write/NULL-exec faults. Cause 1 + cause 2 held.
- **Two residual ring-3 kills, ~50 min in**: `#GP(0) rip=0x30046b96` killing
  pid 957, and a `pid=1215 killed by signal 11` one line later — both right
  after a `[TRAMP-MISMATCH]` pair (tid 18/19 resolving against stale
  `THREAD_PID_MAP` entries). PID-recycling ambiguity: the long-lived rustc
  that *also* carried pid 957 was alive 30 minutes later (PSTATS elapsed
  2675 s), so the victim was a recycled slot, not that job — no cargo job
  was lost and the build kept advancing. Console evidence:
  `/root/akuma-fc.log` lines ~2700-2710 of the 2026-09-17 boot.
- **The run then ended in a SILENT WEDGE** (the Defect-A family,
  selfhost-kernel-build.md §5.3a row 3): `virtio-drivers` sat "Compiling"
  30+ minutes, and a 40-second PSTATS delta showed **zero syscalls across
  every remaining rustc** while cargo stayed alive — cargo waiting on a
  child that is gone, i.e. `wait4` never returned or the child died without
  waking it. Prime suspect: the `#GP`/SIGSEGV kill pair killed a build
  process without cargo being woken. This wedge, plus its kill-class
  precursor, is the top open item — not cause 2, which is fixed.

Speed (the original question, still open): it is slow, and the counters
say where. rustc pid 1417 burned **258 s of 590 s in-kernel on 62 k `read`
syscalls (246 s)**; cargo sat **716 s blocked in `recvfrom`** on the
jobserver pipe; individual mid-size crates (`akuma-dmesg`,
`virtio-drivers`) took 10–30 min of wall. That is the file-read/fault path,
not codegen and not scheduling — matching the audit's remaining levers
below (virtio-blk double-copy, ext2 per-page block re-derivation, no shared
zero page). The wedge masks honest re-timing until it is fixed.

### Status and next steps

- **Cause 1 fixed** (one-line reference fix in `amd64/src/mm.rs`), verified:
  16k rewrites flat, boot suite 707/707 SMP=1 and 717/717 SMP=4, clippy
  clean, full smpstress on FC no longer OOMs.
- **Cause 2 fixed** (`idt.rs` owner-locked CoW break + owner-resolved share
  pass, above) — `w4g` green, `-j4` memory flat for ~50 min. Fix folded to
  the main tree (merge e5ba8298).
- **Open: the `-j4` wedge + its kill-class precursor** (§ above). Next:
  catch the `#GP`/SIGSEGV kills with a rip/cr3 dump plus the victim's
  `/proc/<pid>/stat`-equivalent from PSTATS before the kill, and check
  whether every `[TRAMP-MISMATCH]` burst precedes a kill; then decide
  whether the trampoline first-mismatch defense is losing once per N
  thousand execs. A cargo-side mitigation (retry a job whose rustc died
  without a signal-carrying exit) would unblock re-timing while the kernel
  hunt runs — but is a mask, not a fix.
- **Then: the read path** for speed — `[PSTATS]` now gives the attribution
  for free (per-syscall counts and coarse times per process). The three
  audit levers (virtio-blk `read_bytes` temp-Vec double-copy, ext2 per-page
  block re-derivation, shared zero page for anon reads) are where the 258 s
  of rustc in-kernel time lives.
- `[TRAMP-MISMATCH]` fires ~500–800×/run under exec churn (aarch64
  ancestor: `KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`) — map-first resolution
  defends the trampoline, but the 2026-09-17 `-j4` run ties it (in time) to
  the residual kill pair; treat "burst then kill" as one bug, not two.
- The FC box kernel: the fixed build (with the kill-line forensics) is
  staged at `/root/akuma/target/x86_64-unknown-none/release/akuma-amd64`;
  the pre-investigation binary is backed up beside it as
  `akuma-amd64.pre-smpstress`. The wedged 2026-09-17 `-j4` guest was still
  up when this was written — kill it (`pgrep -x firecracker` on the box's
  Ubuntu side, port-22) before reusing the rig.

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
2. **Root-cause the `-j4` silent wedge and its kill-class precursor**
   (open item above; the original `#UD`/OOM shape is fixed — causes 1 and 2).
   It still gates re-timing: the aarch64 self-host baseline (44 s clean
   build) had four cores, and this run shows SMP builds are survivable but
   not yet dependable.
3. **Attribute the rest in-guest** with the new PSTATS counters: run a build,
   then read the 30 s `[PSTATS]` block (per-syscall counts, coarse times —
   blocking syscalls are the ones that surface — and `pf=` fault counts).
   rustc's `/proc/<pid>/stat` utime/minflt are stubs on this target (both
   read 0 mid-compile); PSTATS is the working instrument now. **First
   attribution is in (§ the -j4 run): rustc spends ~44 % of its wall
   in-kernel, almost all of it in `read`** — the read path, not codegen.
4. Remaining audit levers, now ranked by that attribution: **virtio-blk
   `read_bytes` temp-Vec double-copy** and **ext2 per-page block
   re-derivation** first (they tax every one of those 62 k reads), then
   shared zero page for anonymous read faults; the
   syscall-entry opt-out bitmap (aarch64 `SYSCALL_BKL_OPTOUT_SEED`) — the
   *mechanism* ports as-is, but each seed needs its amd64 handler audited
   (e.g. `futex` cannot be seeded until `WAITERS` stops naming the BKL as its
   safety argument) — only pays at SMP>1, after item 2.

## Continued 2026-09-17 (later the same day): the clock, the block double-copy, and CPU-time accounting

*Investigation continued on `vaporwave` (the Firecracker host) after the fold
above. Four fixes landed, all in the shared crates both kernels build from
(`crates/akuma-virtio`, `crates/akuma-threading`) or amd64-only
(`amd64/src/lapic.rs`, `amd64/src/net.rs`). A fifth item — a new wedge class,
distinct from the `-j4` one above — was found and is OPEN, not fixed.*

### 1. The clock was never calibrated on `microvm`, and the guess could be off by up to 6x

`lapic::calibrate` measured PIT channel 2's gate/output through legacy port
`0x61` — board glue, not part of the 8254 itself. QEMU's `microvm` machine
instantiates an `isa-pit` (`info qtree`: ports 0x40/0x43) but nothing answers
0x61, so `pit_present()` always failed there and `calibrate` never ran. Every
timeout on that machine — and every `clock_gettime` — ran on the hardcoded
`UNCALIBRATED_COUNT` guess, which `start_timer`'s own doc already flagged as
"~1.6 ms on a KVM guest whose APIC is nominally 1 GHz and ~16 ms on this
machine's 100 MHz bus. One clock ran 6x fast, the other 1.6x slow, and both
called it 10 ms."

Fixed by switching calibration to PIT **channel 0**: it is hardwired always-on
(no gate needed), and the 8254's own read-back command (`0xE2` on the command
port, decoded in `lapic.rs`) latches a channel's status — OUTPUT bit included
— onto the data port. That is standard 8254 behaviour, not board glue, so it
works on `microvm`, on real hardware, and needs no port-0x61 equivalent at
all. One bug on the way: the first version of the read-back command byte had
bits 5/4 (count-latch / status-latch) inverted, which *looked* calibrated
(`CALIBRATED=true`, a plausible-sounding "10 MHz" APIC bus) but was polling a
live decrementing count register, not a status bit — caught immediately by
the kernel's own `clock_rate_check` self-test (`lapic: ticks per 50ms (expect
5) 0` — the exact class of bug that self-test exists to catch). Fixed byte:
`0b1110_0010`. Verified: LAPIC and TSC both calibrate to **997 MHz** (matches
the "~1 GHz on a KVM-class guest" the docs already expected), the 50 ms
cross-check reports exactly 5 ticks, all 747 boot self-tests pass.

### 2. `clock_gettime` moved from 10 ms ticks to TSC resolution

`net::uptime_us()` — what `clock_gettime`, `nanosleep`, futex deadlines and
every network timeout derive from — was `lapic::ticks() * 10_000`, one LAPIC
timer IRQ per reading. Now that the TSC is genuinely calibrated (above), it
reads `lapic::tsc_uptime_us()` first (an `rdtsc` delta scaled by the measured
Hz, in `u128` to avoid overflow) and falls back to the old tick counter only
when no PIT was found — the target `usermode.rs`'s own self-test named as the
`60 s + 45 s` reason a fault-cost probe "is not run, it is not *runnable*":
"TSC has the resolution the tick clock lacks, and the kernel is where the TSC
is reachable." `userspace/ext2probe/c/read_syscall_cost` on amd64 went from
tick-quantized noise (100 µs steps, scaled ~6x wrong per #1) to real numbers:
`getpid` ~1.1 µs, a 0-byte `read` ~470 ns (the syscall floor), a 4 KiB `read`
~12-14 µs. All 747 self-tests still pass, including the one that checks
`clock_gettime` agrees with the kernel's own clock within one tick.

### 3. `read_bytes`/`write_bytes` did an unconditional allocate-and-copy — this doc's own item 4, lever 1

`crates/akuma-virtio/src/block.rs`'s `read_bytes`/`write_bytes` always
allocated a temp sector-aligned buffer, read/wrote through it, then copied
into/out of the caller's buffer — even when the caller's `offset` and `len`
were already sector-aligned, which is **every** ext2 call site: `block_size`
is 4096, a multiple of `SECTOR_SIZE` (512), so `ext2.rs`'s hot read-fill path
(`disk_offset`/`run_bytes`, both `block_size` multiples) always qualified.
`write_bytes` had the worse version of the same bug: a full read-modify-write
— including the **read** — for a write that replaces the sector wholesale.
Fixed with a fast path (`offset`/`len` both sector-multiples → straight
`read_sectors`/`write_sectors` on the caller's buffer, no allocation, no
extra copy, and for writes no read at all) ahead of the existing slow path,
which stays for genuinely misaligned callers. Shared code — `akuma-vfs-glue`'s
`KernelBlockDevice` (both kernels' `BlockDevice` impl for ext2) calls straight
into it, and `akuma-vfs-glue` is a dependency of the root `akuma` (aarch64)
package too, confirmed via `Cargo.toml`.

Caveat on verification: `read_syscall_cost`'s own before/after numbers showed
no difference, and that is expected, not a failed fix — the probe warms the
file with four full passes before timing anything, so every measured read is
an **ext2 block-cache hit** and never reaches `read_bytes` at all. The fix
pays off on the *cold* path (first touch of each block), which is most of
what a build's "open a source file, open an rlib" pattern is. A clean A/B on
the **write** side (no warm-cache confound — every `dd`-written block is new)
did show it: `dd bs=4096 count=2000` (2000 aligned 4 KiB writes) went from
9.1-9.6 MB/s (3 runs, pre-fix) to 16.1 MB/s (post-fix) on the local `microvm`
rig.

### 4. `ps`/`top`'s CPU time was not "10 ms-quantized" as a prior doc said — it was exactly, permanently zero

`docs/archive/AKUMA_AMD64_STREAMLINING.md` records amd64 `/proc` CPU-time
figures as "quantized"; that is no longer what's true, and may never have
been on the thread-heavy path this build exercises. Direct read of
`/proc/<pid>/stat` field 14 (`utime` — what `ps`'s TIME column shows) on a
process alive 9+ minutes read **0**, for every process, always.

Root cause: `crates/akuma-threading/src/lib.rs`'s `x86_yield_now` — the
**only** context-switch path on this target — never touched
`TOTAL_CPU_TIMES` or a slot's `start_time_us`. Both are written by
`commit_switch`, the generic (aarch64) switch path's equivalent step; the
x86-64 arch-hooks path (`x86_claim_slot`/`x86_publish`/`x86_yield`/
`x86_adopt_running_thread`, added for this target's cooperative scheduler)
duplicates everything else `commit_switch` does — `ON_CPU`, `THREAD_STATES`,
the picked-next dance — but was never given this half. So `TOTAL_CPU_TIMES`
never accumulated, and `get_thread_cpu_time`'s "still running, add time since
`start_time_us`" branch never fired because `start_time_us` was never set
either — the `if start_time > 0` guard skipped it, silently, forever.

Fixed by adding the same two steps `commit_switch` does, at the same two
points: `x86_yield_now` bills the outgoing thread's elapsed slice into
`TOTAL_CPU_TIMES` and stamps the incoming thread's `start_time_us`, under the
same `POOL` lock `get_thread_cpu_time`'s read side already takes;
`x86_adopt_running_thread` (the boot/idle-thread bootstrap path) stamps its
own `start_time_us` too, so a thread that never gets switched out before
something reads its CPU time doesn't undercount its first slice. Verified on
the local `microvm` rig: `/proc/1/stat` utime went from `0` to `1` (one
jiffy) after boot; a `busybox yes` loop run for 3 real seconds showed **300**
jiffies (exactly 3.00 s) via direct `/proc` read, and `ps`'s own TIME column
correctly rendered `0:03` — both were unconditionally `0:00` before. This is
architecture-neutral code in a shared crate; the aarch64 build was rebuilt
clean afterward and is unaffected (the new code is
`#[cfg(target_arch = "x86_64")]`-gated).

### 5. OPEN: `find_free_va` is O(n²) per `mmap` call, and a process with ~1000+ regions crawls to a halt

Attempting to re-time the in-guest self-host build (Firecracker, `vcpu_count:
1`, `mem_size_mib: 6144`, deliberately matching this doc's own "1-vCPU
anchor" methodology to avoid the `-j4` SMP wedge) with fixes #1-#4 applied:
`cargo build --release -p akuma-amd64 --target x86_64-unknown-none -j1
--offline` compiled 19 of 90 crates in ~9 minutes, then stopped making
forward progress entirely while compiling `zerocopy` — a small crate that
compiles in seconds on every other target. The three `ps`-visible rows for
that one `rustc --crate-name zerocopy` invocation (pids 277/279, plus 278
tagged `{ctrl-c}` — rustc's own internal Ctrl-C-watcher thread, reported as
a **separate `Tgid`** rather than a thread inside 277's, which may itself be
a real divergence worth checking against how this target's `clone()` maps
`CLONE_THREAD`) sat at the same three PIDs, `State: R (running)`, zero
`build.log` growth across every check spanning 15+ minutes. No panic, no
`[Fault]`, no `memwatch-at-kill` anywhere near the stall point.

**Root cause, found by reading `/proc/277/maps` and the placement code it
came from.** `/proc/277/maps` itself is the tell: a bounded 64 KB read came
back in under a second with **1,524 lines** (mostly individual 4 KiB `rw-p`
regions — one `mmap` per small allocation from rustc's own arena/bump
allocator), but a 2 MB read (~46k lines, ~30x more data) never returned at
all inside a 30 s budget — not proportionally slower, catastrophically
slower. `find_free_va` (`amd64/src/mm.rs:207`, first-fit placement for every
`mmap`) explains why: its own doc comment says the outer retry loop
"terminates in at most one pass per region," which is true and also
incomplete — each of those up-to-`n` outer passes re-scans the **entire**
`regions` slice from the top (`for r in regions` inside `'outer: loop`), so a
single `mmap` call against `n` scattered regions costs **O(n²)**, not O(n).
`/proc/<pid>/maps` rendering walks the same flat, unsorted
`Vec<MmapRegion>` (`crates/akuma-mmap/src/region.rs`) to format each line,
which is why reading the file inherits the same blowup. Build progress
matches exactly: fast and steady while every process's region count was
small (19 crates in ~9 minutes), then a hard wall the moment one process
(zerocopy's `rustc`, doing enough small individual allocations to reach
four-digit region counts within its own address space) crossed into "every
subsequent `mmap` costs proportional to everything already accumulated."
Nothing is deadlocked; every call is still completing, just at cost growing
quadratically with a count that only ever grows for a process like `rustc`.

**Not fixed this session, and deliberately not attempted under time
pressure.** The real fix is a data-structure change — `akuma-mmap`'s region
list is a flat, intentionally-unsorted `Vec` everywhere (placement,
`/proc/maps` rendering, almost certainly `munmap`'s clip-and-split too; see
the "why no sort" comment at `mm.rs:198` for why it was chosen that way), and
real Linux uses a red-black/interval tree for exactly this reason (O(log n)
placement). This crate is shared between both kernels, so the fix needs
testing on both architectures before it can be trusted — a dedicated session,
not a tail end of this one. The FC guest was left running rather than killed,
in case a live low-level trace becomes useful before it is un-wedged. A fresh
boot with fixes #1-#4 and a normal `cargo clean && cargo build` trial is the
fallback to get a real timing number without waiting on this fix.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the self-host bring-up stages.
- `docs/runbooks/selfhost-kernel-build.md` — aarch64 self-host procedure,
  detach/poll mechanics, and the aarch64 baseline numbers (44 s clean build).
- `docs/archive/COW_PILE_AUDIT.md` §10 — the freed-L0 hazard this fix leans
  on the liveness gate for.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — procfs on amd64.
