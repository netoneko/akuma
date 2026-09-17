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
2026-09-17" below. Re-timing hit a NEW, distinct wall: `mmap` placement was
O(n²) in the region count, so a process doing many small mmaps (rustc's own
allocator) crawled to a halt once it accumulated roughly four figures of
regions — §5 of that section. **Fixed 2026-09-17** (§5's "The fix"): the
placer sorts and scans once, and `/proc/<pid>/maps` binary-searches the same
extents instead of scanning them per page.*

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

### Cross-arch TCG comparison (2026-09-17) — the read path is the gap

Same probe binaries built from the same sources, both kernels under QEMU
TCG, SMP=4, same args. `ext2probe 25 4` (before-pass numbers):

| op | amd64 | aarch64 | ratio |
|---|---:|---:|---:|
| create | 279,984 µs | 134,123 µs | 2.1× |
| seq_write (2 MiB) | 88,639 µs | 51,962 µs | 1.7× |
| **seq_read (2 MiB)** | **5,661 µs** | **375 µs** | **15×** |
| list_dir | 2,432 µs | 382 µs | 6.4× |
| delete | 90,988 µs | 50,063 µs | 1.8× |
| mass delete | 3640 files/s | 6396 files/s | 1.8× |

`read_syscall_cost` (8 MiB warm `/tmp/rsc.bin`; medians, 100×100):

| arm | aarch64 | amd64 | ratio |
|---|---:|---:|---:|
| getpid (syscall floor) | 140 ns | 1,170 ns | 8.4× |
| null read (len=0) | 250 ns | 490 ns | 2.0× |
| zero 4 KiB | 450 ns | 8,610 ns | 19× |
| zero 64 KiB | 2,920 ns | 122,100 ns | 42× |
| file pread 4 KiB | 820 ns | 15,360 ns | 19× |
| file pread 64 KiB | 6,370 ns | 147,620 ns | 23× |

Reading: the syscall *entry* floor is 8× (TCG x86 entry/swapgs cost —
present on every call but small in absolute terms). The killer is the
**data-movement path**: `zero 64 KiB` moves no filesystem data at all —
syscall + fill of the user buffer — and amd64 pays 122 µs for what aarch64
does in 2.9 µs (~540 MB/s vs ~22 GB/s). The per-byte cost, not the syscall
count, is what made rustc spend 258 s in-kernel on 62 k reads. Whatever the
amd64 `copy_to_user`/user-buffer fill does (byte-wise loop? no ERMS
`rep movsb`? per-page fault churn?), it is ~40× off aarch64's, and fixing
it is worth more than every other lever combined. Note the amd64 numbers
are *per-call linear* in length (zero 8K ≈ 2× zero 4K), so this is a
throughput problem, not per-syscall overhead.

One correctness divergence rode along: `ext2probe` pinned-reclaim returned
**0 %** of deleted mapped-file bytes on amd64 vs **85 %** on aarch64 —
`ext2probe: SPACE LEAK (unlink of a MAPPED file did not return its blocks
— pin/deferral leak)`. Verdict otherwise `NO REGRESSION` on both.

Probe mechanics for amd64 INIT-runs: `read_syscall_cost` needs
`/tmp/rsc.bin` to already exist (`busybox dd if=/dev/zero of=/tmp/rsc.bin
bs=1M count=8` first — via the sshd image, since bare INIT boots have no
shell); disk built with `sh amd64/mkdisk.sh /tmp/probe-disk.img 128` plus
debugfs injection of `probes/{ext2probe,read_syscall_cost,execleak2}`;
ssh auth takes `-i target/x86_64-unknown-none/release/amd64-ssh-test-key`.
Beware: backgrounded QEMU processes are reaped between tool calls — run
boot+probe+teardown inside one shell invocation. Raw logs:
`logs/j4-wedge-20260917/` and `/tmp/probe-runs/` (session-local).

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
3. ~~**Attribute the rest in-guest** with the new PSTATS counters~~ — **done,
   §8.** Read that section's "`[PSTATS]` first had to stop being a sampler"
   before trusting any earlier `[PSTATS]` number in this doc: the timings were
   10 ms-tick samples until 2026-09-17, they only print at `SMP≥2`, and an
   `nrN` entry is a raw **x86_64** number while a *named* one is asm-generic.
   rustc's `/proc/<pid>/stat` utime/minflt are stubs on this target (both
   read 0 mid-compile); PSTATS is the working instrument.
4. Remaining audit levers. **The `-j1` ranking above no longer holds** (2026-09-17):
   with the §8 sort fixed, a `zerocopy` compile's whole measured syscall and
   fault budget is ~1 s of 18 s, and `read` is not in it — the 62 k-read
   attribution was from the `-j4` kernel build, a different workload that reads
   far more source. Re-attribute against the target workload before picking
   between **virtio-blk `read_bytes` temp-Vec double-copy**, **ext2 per-page
   block re-derivation** and the shared zero page for anonymous read faults; the
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

### 5. FIXED (2026-09-17): `find_free_va` was O(n²) per `mmap` call, and a process with ~1000+ regions crawled to a halt

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

#### The fix (2026-09-17, same day)

**It sorts.** `find_free_va` now calls `sort_unstable_by_key(|r| r.start_va)`
on the caller's list and scans it **once**: `cand` is the high-water mark of
every region seen so far, and because the list is sorted, the first region
starting at or past `cand + len` proves every byte below it is free. The
restart — `continue 'outer`, which re-entered the walk at the top of the list
— is gone.

Two things the paragraph above got wrong, and they are why this was a small
change rather than the dedicated session it was scoped as:

- **The objection to sorting does not hold.** The `mm.rs:198` comment ruled it
  out because "a gap scan would have to sort — which means a `Vec` per `mmap`,
  on the path a program allocating memory takes". `sort_unstable_by_key` is
  pattern-defeating quicksort: **in place, no allocation**. It is also
  adaptive, so the sorted list each call leaves behind costs a linear pass to
  re-confirm on the next one rather than a full sort — the steady state is
  O(n), not O(n log n).
- **`find_free_va` is not shared, and neither is the region list's use of it.**
  It is amd64-only. The AArch64 kernel does not place this way at all: it
  carries a per-process bump cursor plus a free list
  (`akuma_exec::process::ProcessMemory::next_mmap` / `free_regions`), and
  `MMAP_BASE`/`find_free_va` appear nowhere in `src/`. `akuma-mmap` — the
  shared crate — was not touched, so there was no cross-architecture exposure
  to test for.

Sorting the caller's list is sound because nothing reads it in order. Regions
never overlap: `MAP_FIXED` `unmap_range`s its target before it records
anything, and every other placement comes from `find_free_va` itself — so the
`find(|r| r.contains(va))` lookups in `fault_in` and `dontneed_range` have at
most one answer whatever order they walk in.

**`/proc/<pid>/maps` was a second, independent quadratic** in the same file,
and the reason the 64 KB read returned in under a second while the 2 MB read
never returned. `collect_leaf_runs` (`amd64/src/fd.rs`) asked
`skip.iter().any(...)` — a linear scan of every region extent — **once per
resident page**, so rendering the file cost O(regions × pages). `pid_map_rows`
now sorts the extents (they are non-overlapping, for the reason above) and the
lookup is a `partition_point` binary search.

**Verification.** Boot suite under local QEMU TCG (`-M microvm`, SMP=1):
**752 passed, 0 failed** — §1's 747 plus the five new checks below. Clippy
clean on the amd64 target with and without
`no-tests`; the full host test suite green (`akuma-mmap` 61/61 among them,
unchanged because the crate was not touched). `cat /proc/self/maps` under the
fixed kernel renders the expected four ascending rows (text, data, mmap arena,
stack).

Both fixes are pinned by self-tests that assert the *shape* rather than a
time, because a boot suite cannot time anything reliably:

- `find_free_va_scan` returns the number of regions the scan examined, and
  `va_placement_check` walks a **1000-region staircase** — single-page regions
  with a single-page hole between each pair, the shape `rustc` accumulates —
  handed in reversed, then checks the placement is right *and* that the scan
  touched each region at most once. The old walk cost 1000 passes of 1000
  regions on that input.
- `maps_skip_lookup_check` runs the binary search against the linear scan it
  replaced as an **oracle**, page by page, over an out-of-order extent list
  containing an extent at the first address, two abutting extents, a one-page
  hole and a wide one. A wrong answer here does not crash anything — it prints
  a page twice or drops a run, and renders a plausible-looking file — so the
  predecessor is the only thing that can catch it.

~~**Still to do: re-time the build.**~~ **Done — §8** (2026-09-17). The wall is
gone: `zerocopy` compiles in 63 s where it had never finished. Re-timing it is
also what turned up the *second*, larger cost in this same function — the
`sort_unstable_by_key` this fix left in, which the paragraph below calls
adaptive and which is not, because `munmap` re-orders the list between calls. The wedged
2026-09-17 guest is still up (kill it before reusing the rig — see "Status and
next steps"), and the `-j4` silent wedge above is a separate, still-open bug.

### 6. FIXED (2026-09-17): `munmap` never released the file mapping's `InodePin`, so `unlink` stopped returning blocks

*Found by a cross-architecture probe sweep, not by this investigation: amd64 was
slower than aarch64 on every filesystem op, and alongside those ratios sat a
**correctness** divergence — `ext2probe`'s `reclaim[pinned]` phase returned
**0 %** of deleted bytes on amd64 against **85 %** on aarch64.*

A file-backed mapping takes an `akuma_primitives::InodePin` on the inode, and
ext2 will not free a pinned inode's blocks on `unlink` — it defers them, which
is correct, because the mapping is still reading them. So the pin has to be
released when the mapping goes away.

On this target it never was. `pin_mapped_inode` (`amd64/src/mm.rs`'s
`pin_mapping_inode` → `akuma_mmu::UserAddressSpace`) had **no release half at
all**. Its own doc comment explains why the pins live in the address space:
"this struct dies exactly when the mappings do — on `exec` and on exit". That is
true of the *address space* and false of an individual mapping. `unmap_range`
released the pages and the region records and left the claim on the file behind,
so every distinct inode a process had ever mapped stayed pinned for the life of
the process.

**Why that is an outage and not a slow leak.** The pin table has 1024 slots.
Past saturation `is_pinned` answers `true` for *every* inode, so ext2 defers
every `unlink`'s block free onto a 256-slot list — and that list then never
drains, because the pins that would release it are held by a process that is
still running. A long-lived process that maps and unmaps files (which is every
build tool, and `ld.so` on every exec) walks the filesystem into "no `unlink`
frees anything". AArch64 does not have the bug: its pin rides inside
`LazySource::File` in the lazy region and dies with the region.

**Fix** — `akuma_mmu::UserAddressSpace::retain_mapped_inode_pins` (x86_64 half,
beside `pin_mapped_inode`), called from `unmap_range` inside the existing region
hold. It takes a *predicate*, not an inode, and that is what makes it correct
rather than approximately correct: a pin is per inode and per address space, one
mapping may go while three others still name the same file (`ld.so` maps a
shared object once per segment), and only the surviving region list can say
whether the last one has gone. Guarded by a cheap overlap pass — almost no
`munmap` touches a file mapping, and a region with no `file` never contributed a
pin. Lock order is unchanged: regions → address space, as
`with_current_address_space` documents.

**A/B, local QEMU TCG, same rootfs image copied per arm**
(`scripts/benchmarks/pin_reclaim_ab.sh`):

| phase | pre-fix | post-fix |
|---|---|---|
| `unpinned` (control — nothing maps them) | 16640 KB consumed, **100 % back** | 16640 KB, **100 % back** |
| `pinned` (unlink while mapped) | 19212 KB consumed, 12 KB back — **0 %** | 19212 KB, 16400 KB back — **85 %** |

The control is the half that makes the other half mean anything: it is green on
*both* arms, so the filesystem frees normally and the defect is specifically the
pin/deferral interaction. 85 % is the AArch64 number exactly; the residual is
directory and group metadata, which is why the probe's threshold is 80.

**The probe** is `userspace/ext2probe/c/pin_reclaim.c` — a C restatement of the
Rust `ext2probe`'s `reclaim_pinned` phase, written because that probe is a
`libakuma` binary and `libakuma` does not build for x86_64, so the one
measurement that distinguishes the two kernels could not be run on the kernel
that was failing it. Static musl, so the same binary runs on either
architecture and on real Linux as a reference arm; staged into the image by
`amd64/mkdisk.sh`, built by `userspace/ext2probe/c/build.sh`.

Three harness traps paid for here, all of which report a clean kernel that was
never tested:

- **`amd64/run.sh` runs `cargo build` before it boots**, so staging a kernel
  binary at the target path does not pin it — the build overwrites it from the
  working tree and both arms run the same kernel. The first baseline arm scored
  `OK` for exactly this reason. `pin_reclaim_ab.sh` drives QEMU directly.
- **`INITARGS` is argv[1..]**, not argv[0] — the kernel supplies the program
  name. `INITARGS=pin_reclaim,/tmp` made `/tmp` argv[2] and `pin_reclaim` the
  probe's root directory, so it created nothing and measured nothing.
- **A probe that measures nothing must not score a pass.** `0` bytes returned of
  `0` consumed is 100 % by arithmetic. The probe reports `INCONCLUSIVE` as a
  third verdict and exits non-zero on it, the same rule `scripts/mem_suite.py`
  applies. That is what caught the third trap: the control phase wrote
  `PLAIN_SIZE` bytes out of a buffer sized `PINNED_SIZE`, the kernel rejected
  the overread, every control file was empty — and a probe that scored
  INCONCLUSIVE as OK would have reported a working control that never ran.

### 7. The amd64-is-slower-at-filesystem-ops premise did NOT reproduce — and the one real defect behind it (2026-09-17)

*Prompted by a cross-architecture probe sweep reporting amd64 slower on every
ext2 op under TCG SMP=4, with `seq_read` **15x** and `list_dir` **6.4x** called
out as dramatic outliers. Neither reproduces.*

**Measure with one binary or do not measure.** That sweep compared two different
probes — the Rust `ext2probe` builds only for aarch64 — so the ratio included
whatever the two programs did differently. `userspace/ext2probe/c/fs_ops_cost.c`
is the same phases as one static musl binary that runs on both kernels and on
Linux. Same binary, both under QEMU TCG, SMP=1, 2048 MB, 2 MiB working set:

| op | amd64 | aarch64 | ratio |
|---|---|---|---|
| create | 14 328 us | 24 065 us | 0.60x |
| seq_write | 110 590 | 199 400 | 0.55x |
| seq_read_cold | 6 334 | 22 171 | 0.29x |
| **seq_read_warm** | **6 186** | **16 881** | **0.37x** |
| **list_dir** | **1 164** | **7 265** | **0.16x** |
| delete | 6 426 | 11 437 | 0.56x |

amd64 is faster on every op, including both claimed outliers. **That first run
is RETRACTED as a measurement — quote the clean rerun below instead.** Three
defects, any one of which is disqualifying:

1. **The arms were not run under the same conditions.** Another agent's QEMU —
   at times a 4-vCPU TCG guest on an 8-performance-core host — was running
   throughout, and the two arms ran about five minutes apart, so they saw
   *different* background load rather than the same one.
2. **The guest environments differ.** The aarch64 arm ran through `ssh` on a
   devbox with `herd` and `sshd` sharing its one TCG vCPU (that kernel has no
   `init=`); the amd64 arm ran as init with nothing else in the guest.
3. **One sample each**, on a workload whose fastest phase is a millisecond.

#### The clean rerun — quiet host, five passes, minimum per op

Re-run with the host free of other guests and `--repeat=5`, taking the
**minimum** across passes. `min` is the right statistic when the noise is other
load on the machine: contention can only ever *add* time, so the fastest pass is
the one least contaminated. (The probe repeats internally because a second
sample would otherwise cost a whole reboot — this kernel has no `init=` on the
AArch64 side and its amd64 `sshd` answers only its own staged key.)

| op | amd64 | aarch64 | ratio |
|---|---|---|---|
| create | 13 828 us | 16 668 us | 0.83x |
| seq_write | 110 992 | 124 550 | 0.89x |
| seq_read_cold | 6 118 | 16 190 | 0.38x |
| **seq_read_warm** | **6 228** | **10 689** | **0.58x** |
| **list_dir** | **305** | **423** | **0.72x** |
| delete | 6 297 | 7 548 | 0.83x |

**The 15x and the 6.4x do not reproduce.** amd64 is faster or comparable on
every op. One asymmetry remains and is stated rather than corrected — the
aarch64 arm still runs through `ssh` with `herd` and `sshd` on its one vCPU —
but it biases *against* aarch64, so the negative claim is safe even if the
margins are not: nothing here is a read-path defect to fix.

**`list_dir` has a warm-up swing that dwarfs the cross-architecture difference**,
and it is the reason a single sample cannot be trusted for this op at all:
aarch64 runs 7 006 us on pass 1 and 423 us by pass 3 — **16.6x** — while amd64
goes 1 189 -> 305 (3.9x). Any one-shot comparison of `list_dir` is measuring
which pass each side happened to be on.

#### Four execution modes, and what TCG was hiding

Same binary, `--repeat=5`, minimum per op. Two of these are emulated and two
are not, which turns out to matter far more than the architecture does.

| op | amd64 TCG | amd64 **KVM** (Firecracker, the box) | aarch64 TCG | aarch64 **HVF** (Mac) | aarch64 **KVM** (Lima) |
|---|---|---|---|---|---|
| create | 13 828 | **5 056** | 16 668 | 42 918 | 330 044 |
| seq_write | 110 992 | **35 388** | 124 550 | 549 587 | 3 578 553 |
| seq_read_cold | 6 118 | **874** | 16 190 | 1 209 | 6 444 |
| seq_read_warm | 6 228 | **867** | 10 689 | 4 907 | 6 368 |
| list_dir | 305 | **84** | 423 | 82 | 205 |
| delete | 6 297 | **2 266** | 7 548 | 35 233 | 166 341 |

Two findings, and the second is the one worth chasing:

1. **amd64 behaves correctly under acceleration.** Every op is faster on KVM
   than on its own TCG run — 2.7x (create), 3.1x (seq_write), 2.8x (delete),
   3.6x (list_dir), 7.2x (seq_read). That is what a healthy path looks like when
   the CPU stops being emulated. Firecracker's variance across five passes is
   under 10 %, the tightest of any arm here.

2. **aarch64 writes get SLOWER the less emulated the machine is, and reads do
   not.** `seq_write` goes 124 550 (TCG) -> 549 587 (HVF) -> 3 578 553 (KVM);
   `create` and `delete` move the same way, while `seq_read` and `list_dir`
   speed up normally. A path that gets *worse* when the CPU gets ~50x faster is
   not CPU-bound — it is waiting on wall-clock, and the HVF arm works out at
   **~1.07 ms per 4 KiB write and ~1.4 ms per unlink**, which is the shape of a
   synchronous device round-trip per operation. Under TCG the guest is slow
   enough that the completion has always already happened, so the wait never
   shows up. **This is an aarch64 write-path finding, not an amd64 one**, and it
   is invisible to every TCG-based measurement in this document.

Caveats that must travel with the table. The two accelerated arms are on
**different hardware** (an HP x86 box vs an Apple M-series), so the amd64-KVM
column and the aarch64-HVF/KVM columns cannot be compared to each other as
architectures — only each column against its own TCG arm, which is what both
findings above do. The Lima column is additionally confounded: `lima_aarch64_run.sh`
mounts the image `snapshot=on` over a read-only host mount, so its writes go to
an overlay and its absolute numbers are not a clean measure of anything. The HVF
arm has no such overlay and shows the same direction, which is why finding 2
rests on HVF rather than Lima. The HVF arm also runs a `no-tests` kernel —
HVF asserts partway through this kernel's boot suite (`QEMU_HVF_ISV_BUG.md`), so
there was no choice; the filesystem path is identical either way.

Two traps worth keeping, both of which produce a confident wrong table:

- **A TCG cross-architecture ratio has a floor well above 1.0.** An x86_64 guest
  on an ARM host is translated instruction-by-instruction; an aarch64 guest on
  the same host is nearly a pass-through. Only *relative* standouts within one
  run mean anything.
- **`gettimeofday` reads `CLOCK_REALTIME`, which is 0 on a NIC-less boot** (no
  SNTP, so "never synced"). The probe's first run reported every phase as `0 us`
  and a complete, plausible-looking table. It uses `CLOCK_MONOTONIC` now.

#### The real defect the sweep led to: amd64's ext2 block cache was 16 MB

Reading the read path to explain the (non-existent) gap found one anyway.
`akuma_ext2::set_cache_cap_bytes` is called from `akuma_vfs_glue::fs::init` —
the **AArch64** mount path. This target mounts ext2 itself, and `amd64/src/fs.rs`
had picked up that path's `fpcache_init` call and its inode-freed hook (both
with comments explaining exactly why they had to be restated here) but **not
this one**. So `CACHE_CAP_BYTES` kept `akuma-ext2`'s `DEFAULT_CACHE_CAP_BYTES`:
16 MB, a value whose own doc comment says it is sized for `cargo test`. AArch64
runs `min(RAM/8, akuma_config::FSCACHE_CEILING_MB)` — 384 MB where there is RAM
for it.

It is invisible to any benchmark with a small working set, which is why it
survived: 2 MiB fits in 16 MB, every read is a hit either way. It is a *build*
that pays — `rustc` reading rlibs has a working set in the hundreds of
megabytes, which is the shape `BKL_RUSTC_SCALING_BASELINE.md` sized the ceiling
against.

**A/B, 32 MB working set** — which is the point: it crosses the old cap.

Unlike the retracted table above, this comparison survives a noisy host, and
the reason is worth stating because it is the general rule. The two arms differ
in **one kernel constant** and nothing else, and they were run *both* ways:
concurrently — where they share the same background load at the same instant by
construction — and serially back to back. Concurrent gave 830 823 -> 205 262 us
(4.0x), serial gave the table below (3.8x). Two methodologies with opposite
contention properties agreeing to within 5 % is what makes the read result
trustworthy on a host that was not quiet.

| op | cap = 16 MB | cap = min(RAM/8, 384 MB) |
|---|---|---|
| seq_read_cold | 382 118 us | **98 421 us — 3.9x** |
| seq_read_warm | 373 252 us | **98 829 us — 3.8x** |
| seq_write | 2 052 456 | 2 180 738 (6 % slower) |
| create | 14 564 | 14 166 |
| delete | 7 139 | 8 539 (20 % slower) |
| list_dir | 1 576 | 1 281 |

The read win is ~3.9x and reproduced under both methodologies. The write and
delete regressions are **one sample each, on a contended host**, and 6 % / 20 %
is exactly the size that contention produces — they are not separable from
noise and must not be reported as a regression without repeats. A bigger cache
holding more dirty blocks is a plausible mechanism if they turn out to be real.
Boot suite 752/752, clippy clean.

`--seq-mb=N` on the probe is what makes this measurable at all: a working set
that fits the cap reports the hit path however big the cap is, so the default
2 MiB pass cannot tell a 16 MB cache from a 384 MB one.

### 8. FIXED (2026-09-17): `find_free_va` sorted 4 400 regions on **every** `mmap`, and that was 74 % of `rustc`

*This is the §5 follow-through: §5 removed the O(n²) restart and the `zerocopy`
wall went with it, but nobody had re-timed the build. Timing it turned up a
second, larger cost in the same function.*

**The §5 regression gate passes.** `cargo build -p zerocopy --target
x86_64-unknown-none --release --offline -j1`, in-guest, FC 1 vCPU / 6144 MB,
`/tmp/ktarget` on ext2: **63 s**, against "never completed in 15+ minutes"
before §5. The stall is gone.

#### Same binary on both sides — and here "both sides" means both *kernels*

63 s is not a number that means anything on its own, and the arm that gives it
one is cheap and had not been run: the guest image carries a complete
`x86_64-unknown-linux-musl` toolchain, so the **same `cargo` and `rustc`
binaries can build the same source on the box's own Ubuntu side**, in a chroot
on a copy of the guest's root image. Same CPU, same files, same toolchain; the
only variable left is the kernel underneath.

| | `zerocopy` `-j1`, min of 3 |
|---|---|
| Linux (bare, `taskset -c 0`) | **8.8 s** |
| Akuma/amd64 (FC, 1 vCPU) | **63 s** |
| Akuma/amd64, after this fix | **18.3 s** |

Pinning the Linux arm to one CPU changes nothing (8.80 s against 8.84 s), which
rules out the obvious objection that the host arm simply had four cores.

Three traps in setting that chroot up, all of which read as "the kernel is
broken" rather than "the harness is":

- **The image's binaries have no `x` bit.** Akuma's ext2 does not check it;
  Linux does, and `chroot` answers `Permission denied` for `/bin/sh`, which
  looks exactly like a mount option.
- **`/dev/null` does not exist in the image.** `cargo` redirects its `rustc -vV`
  probe there and fails with `could not execute process rustc -vV (never
  executed) / No such file or directory` — a message that names `rustc`, so the
  first four attempts went looking at `PATH`, at `RUSTC`, and at the musl
  loader, all of which were fine.
- The ssh key the harnesses expect (`target/…/amd64-ssh-test-key`) had been
  cleaned away; the image's `authorized_keys` is a file on the image, so a new
  key can simply be appended to it while nothing is booted.

#### `[PSTATS]` first had to stop being a sampler

`[PSTATS]` timed syscalls with `lapic::ticks()` — the **10 ms** LAPIC tick. The
note in `usermode.rs` argued that was the point: a syscall shorter than a tick
folds to 0, the sweep sorts by time, so what surfaces is where the wall clock
went. That is true of a *blocking* syscall and false of a frequent short one,
and the difference is not rounding — a 7 µs `mmap` is charged a full 10 ms
whenever a tick lands inside it, so the reported time is an unbiased estimator
with a standard error of one whole tick per sample. It read 8.27 s for `mmap`
against a per-call cost the probe put at 7.7 µs (0.45 s), and **nothing in the
number says which of the two is wrong**.

Switched to `lapic::tsc_uptime_us()`, the resolution this target already has
(`clock_gettime` moved onto it in §2). Cost is one `rdtsc` plus a 128-bit
mul/div at each end, ~40 cycles, against a syscall floor three orders up. It
returns `None` while the TSC is uncalibrated, and the epilogue then adds
nothing rather than a fabricated duration.

Two more things about `[PSTATS]` on this target that cost time to find:

- **It only prints from `idle_loop`, which is the *secondary core's* entry
  (`smp.rs:755`).** At `SMP=1` nothing ever enters it and the sweep never runs,
  however long a process lives. Attribution runs need `SMP≥2`; `-j1` with two
  vCPUs is the right shape, since the second core only has to idle. (The
  exit-time dump in `akuma-exec` is in `return_to_kernel`, which is the AArch64
  exit path — amd64 does not take it, so a process exiting prints nothing.)
- **The syscall numbers are counted at two sites under two numbering schemes.**
  `amd64/src/usermode.rs` counts the raw **x86_64** number; `akuma-syscalls-glue`
  counts the **asm-generic** one for everything that reaches glue. The name
  table is asm-generic only. So an entry printed **with a name is correct**, and
  an entry printed as **`nrN` is a raw x86_64 number** — `nr9`/`nr11` are
  `mmap`/`munmap`, `nr228` is `clock_gettime`. A reader who assumes one scheme
  will find `sshd` calling `ptrace` 16 352 times.

With TSC timing, one `rustc` over a 29.92 s window:

```
[PSTATS] PID 85 (rustc) 29.92s: 112599 syscalls in_kernel=22761ms pgfault=67465
  | nr9=58520(20362ms) nr11=53347(2309ms) fcntl=23(49ms) …
```

**20.4 s of a 29.9 s window inside `mmap` — 68 % of wall, 89 % of all
in-kernel time, at 348 µs per call.** 99.3 % of this process's syscalls are
`mmap`/`munmap`.

#### What it was not

Two candidates were measured and rejected before the real one was found, and
both are worth keeping because both are plausible and both are wrong:

- **Eager population.** `plan()` makes a private anonymous mapping eager unless
  it exceeds `MMAP_EAGER_MAX_PAGES` (16), and musl's mallocng maps groups at or
  under that, so *every* one of `rustc`'s 58 k mappings took the eager path —
  and `mem_fault_cost` prices one eager page at 14.0 µs against a lazy 0.64 µs
  (Linux's eager premium for the same arm is **32 ns**). It looked certain.
  Forcing `EAGER_MAX_PAGES = 0` so every private anonymous mapping is lazy
  changed the build time by **nothing** (1 m 02 s against 1 m 03 s), and
  `mmap`'s share stayed at 74 %. Reverted.
- **Page faults.** 67 k of them, and they are not in `in_kernel` at all. They
  are also fine: `mem_fault_cost` puts a demand fault at 1 532 ns against
  Linux's 1 291 ns, and a CoW fault at 2 401 ns against 1 823 ns.

#### The probe that under-measured by 60x, and why

`userspace/memprobe/c/mmap_scale.c` (new here) mmaps one page at a time and
reports the per-call cost bucketed by how many mappings the process already
holds. One static musl binary for both architectures and for Linux.

| regions held | Akuma `mmap` | Linux `mmap` |
|---|---|---|
| 0 | 1 656 ns | 937 ns |
| 3 750 | **16 720 ns** | 927 ns |

Exactly linear — `1 656 ns + 4.02 ns x regions` — against a Linux line that is
flat to within noise. A real defect, and **not this one**: at the ~2 000 regions
`/proc/<pid>/maps` showed for a live `rustc` it predicts 8 µs, not 348 µs.

The probe under-measures because its grow phase **never unmaps**, so the region
list it hands the placer is always already in address order — the best case for
an adaptive sort, and the case `rustc` is never in. *A probe that reproduces the
shape of the workload but not its history can be linear, correct, and off by a
factor of 60.*

#### Root cause: the sort, and the two appenders that made it necessary

Phase timing inside `sys_mmap` (temporary `[mmap-prof]`, 8 192-call averages)
put it beyond doubt — and note the growth, which is what a region-count theory
cannot explain and a *disorder* theory can:

```
[mmap-prof] calls=8192  avg_total_us=34  avg_lookup_us=0 avg_place_us=33  avg_sort_us=31  avg_regions=424  unsorted_pct=25
[mmap-prof] calls=81920 avg_total_us=483 avg_lookup_us=0 avg_place_us=483 avg_sort_us=468 avg_regions=4400 unsorted_pct=18
```

**468 µs of a 483 µs `mmap` was `sort_unstable_by_key`** — 97 % of the call. The
process-table lookup every entry pays is 0 µs; the scan §5 rewrote is ~15 µs.

§5 left the sort in with this justification:

> `sort_unstable_by_key` is pattern-defeating quicksort: in place, **no
> allocation** … and adaptive, so the sorted list this leaves behind costs a
> linear pass to re-confirm on the next call rather than a full sort.

The first half is true. The second is only true **if nothing re-orders the list
between two calls**, and two things did:

1. `sys_mmap` appended each new region with `regions.push` — out of order
   whenever first-fit placed it in a freed hole rather than at the top.
2. `akuma_mmap::detach_eager_regions_in_range` — `munmap`'s clip-and-split —
   `remove`d the region it clipped and `push`ed its survivors onto the **end**,
   so every partial unmap moved a low-address region to the back.

`rustc` interleaves ~53 k `munmap`s with ~58 k `mmap`s compiling one small
crate, and the list arriving at the placer averaged **4 400 regions, 18 % of
calls out of order**. The cost grows as the build runs because the disorder
does.

#### The fix

Three parts, and the first two are what make the third safe:

- **`detach_eager_regions_in_range` keeps its survivors in place.** The head
  reuses the slot the original occupied (`pages`/`frames` assigned in place,
  `start_va` is already right); a tail goes at `i + 1`, or into the slot itself
  when there is no head; a region wholly inside the range is removed. This is
  **strictly less shifting than the `remove`-and-`push` it replaces** — the old
  shape shifted the list tail once unconditionally, this one shifts only for a
  middle split or a whole-region unmap, and a clip at either edge now moves
  nothing. That matters because **the AArch64 kernel calls this function too**
  (`akuma-syscalls-glue`'s `munmap`) and gets no benefit from the ordering: it
  places from a bump cursor and a free list (`ProcessMemory::next_mmap` /
  `free_regions`) and never scans this list for a gap. Ordering is free for it
  rather than a tax.
- **`insert_region_sorted`** (amd64) puts a new region at its address via
  `partition_point` instead of appending. The `insert` shifts nothing in the
  common case, because first-fit returns a low gap only when one was freed and
  otherwise places at the end of the list.
- **The placer sorts only if the list is not already sorted** — a `windows(2)`
  pass with no moves. The sort is **kept rather than replaced by a debug
  assertion**, deliberately: the invariant is an optimisation, and a future
  writer who appends should get a slow placer, not a wrong one. An optimisation
  that quietly becomes a correctness requirement is how this cost arrived.

#### Result

| | `zerocopy` `-j1`, min of 3, FC 1 vCPU / 6 GB |
|---|---|
| before | 63 s |
| after | **18.3 s** — **3.4x** |
| Linux reference | 8.8 s (gap 7.2x -> 2.1x) |

Boot suite **730 passed / 0 failed at SMP=1** and **740 / 0 at SMP=4** (both +2
over the pre-fix count: the two new self-tests). Host tests 1 455 / 0, clippy
clean on the amd64 target and on `akuma-mmap`.

Pinned by tests that assert **shape, not time**, because a boot suite cannot
time anything and because a list that silently goes out of order is not slower
here — it is slower on the *next* `mmap`, in another process, minutes later:

- `akuma-mmap` host tests `split_keeps_the_list_in_address_order` (head+tail and
  tail-only splits) and `split_visits_every_region_once` (the loop has to step
  over what it inserted; getting that wrong silently skips a region).
- amd64 boot-suite checks `mmap va: insertion keeps the region list in address
  order` and `… keeps every region`, fed out-of-order inserts including one
  landing in a hole in the middle.

#### Open, and measured here

- **`munmap` costs 13.2 µs at `SMP=2` against 1.97 µs at `SMP=1`** (probe base,
  same kernel) — a TLB-shootdown IPI per unmap. Linux is 1.46 µs flat. At 53 k
  unmaps per `rustc` that is ~0.6 s per core beyond the first, and it is the
  next thing to look at for SMP builds.
- **`mmap` is still linear in region count** at 4.02 ns/region (the §5 scan,
  now the whole of placement). At 4 400 regions that is ~18 µs a call. A
  per-address-space cursor — what AArch64 already does — is the fix; it did not
  matter next to a 468 µs sort and it is the leading term now. The same
  `mmap_scale` binary on AArch64 (QEMU TCG) is **flat**: 1 248–1 584 ns from 0
  to 3 750 regions, slope −0.24 ns/region. That is the bump cursor plus free
  list, and it is also why none of §8 is an AArch64 speedup — only the shared
  `detach_eager_regions_in_range` change reaches that kernel, and there it is
  behaviour-neutral and slightly cheaper.
- The remaining 2.1x against Linux is **not** syscalls: with `mmap` fixed the
  measured syscall and fault budget accounts for ~1 s of the 18 s.


## Background

- `docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md` — the AArch64 original of §6's leak.
- `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` — where §7's 384 MB ceiling comes from.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the self-host bring-up stages.
- `docs/runbooks/selfhost-kernel-build.md` — aarch64 self-host procedure,
  detach/poll mechanics, and the aarch64 baseline numbers (44 s clean build).
- `docs/archive/COW_PILE_AUDIT.md` §10 — the freed-L0 hazard this fix leans
  on the liveness gate for.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — procfs on amd64.
