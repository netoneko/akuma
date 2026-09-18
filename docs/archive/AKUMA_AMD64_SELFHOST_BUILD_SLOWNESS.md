# Self-host build slowness on amd64 — investigation and first fix

*Investigation of 2026-09-16/17, worktree `profiling/amd64-build-slow`,
folded to the main tree 2026-09-17 (merge e5ba8298).*
*Status (2026-09-18, §15-§17): **`-j4` at `SMP=4` is green and the self-host
has a fixed point.** A clean 137-crate `cargo build -p akuma-amd64` in the
Firecracker guest takes ~185 s, the kernel it produces boots (768 passed at
4 vCPU), and the kernel *it* builds is byte-identical to itself. The sections
below are the road there, in order; each one's "still open" is answered by a
later one, so read §14-§17 before acting on §10-§13's next-steps.*
*Earlier status: the 1-vCPU anchor landed (77.9 s, −24 %) and both SMP=4 crash
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
2. **Root-cause the `-j4` silent wedge** — **the top open item, and now the
   only thing between here and a fast in-guest kernel build** (§10). The
   "kill-class precursor" framing is retired: the 2026-09-18 capture has no kill
   at all, an idle vCPU, and two `rustc` processes present at `0:00` CPU that
   were never scheduled. Read §10 before the older account here.
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
- **The syscall numbers are counted at two sites under two numbering schemes,
  and the name is looked up by index regardless of which site wrote it.**
  `amd64/src/usermode.rs` counts the raw **x86_64** number (`usermode.rs`'s
  `syscall_stats.inc(nr)` at syscall entry); `akuma-syscalls-glue` counts the
  **asm-generic** one for everything that reaches glue
  (`akuma-syscalls-glue/src/lib.rs`). Both write the same per-process array, and
  `syscall_name` maps **asm-generic** numbers only.

  So on this target **one syscall appears twice**, once at each number, and the
  printed name is always the asm-generic name of whichever index it landed on.
  Three consequences, and the third is the trap:

  1. An entry printed as **`nrN` is a raw x86_64 number** whose value has no
     asm-generic name — `nr9`/`nr11` are `mmap`/`munmap`, `nr228` is
     `clock_gettime`.
  2. An entry at an asm-generic index is named correctly.
  3. **An x86_64 number that happens to be a valid asm-generic number is printed
     under the wrong name, and looks perfectly plausible.** `sshd`'s
     `unlinkat=16352(55650ms)` beside `nanosleep=16352(56403ms)` is one syscall:
     `nanosleep` is x86_64 35, and asm-generic 35 is `unlinkat`. The equal counts
     are the tell. Likewise a `cargo` blocked in `futex` (x86_64 202) reads as
     **`accept`**, because asm-generic 202 is `accept`.

  **Decode before concluding.** An earlier reading of this doc's own `-j4` data
  put `rustc` "almost all in `read`"; asm-generic 63 is `read` and x86_64 63 is
  `uname`, and the two readings are not remotely the same finding.

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
- ~~**`mmap` is still linear in region count** at 4.02 ns/region~~ — **§9**:
  the per-address-space cursor landed, slope 4.02 -> 1.65 ns/region. What is
  left of it is the residual scan past the cursor. The same
  `mmap_scale` binary on AArch64 (QEMU TCG) is **flat**: 1 248–1 584 ns from 0
  to 3 750 regions, slope −0.24 ns/region. That is the bump cursor plus free
  list, and it is also why none of §8 is an AArch64 speedup — only the shared
  `detach_eager_regions_in_range` change reaches that kernel, and there it is
  behaviour-neutral and slightly cheaper.
- The remaining 2.1x against Linux is **not** syscalls: with `mmap` fixed the
  measured syscall and fault budget accounts for ~1 s of the 18 s.


### 9. FIXED (2026-09-17): four O(n) walks over the region list, and a linear `find` per page fault

*§8's follow-through. With the sort gone, `[PSTATS]` — now a stopwatch — put
`mmap` at 27 us and `munmap` at 42 us against a 4 400-region list, together **98 %
of `rustc`'s in-kernel time**. Both are scans of the same list, and so is a
lookup on the fault path that `[PSTATS]` cannot see at all.*

#### What was walking the list

| walk | when | what it cost |
|---|---|---|
| `find_free_va`'s first-fit scan | every `mmap` | ~18 us of a 26 us call |
| `find_free_va`'s `windows(2)` sorted-check | every `mmap` | ~7 us (1.65 ns/region) |
| `detach_eager_regions_in_range` | every `munmap` | O(regions), **both kernels** |
| `unmap_range`'s "does this range name a file?" | every `munmap` | O(regions) |
| `regions.iter().find(\|r\| r.contains(page))` | every **page fault** | O(regions), 86 k times |

The last one is the one to notice: page faults are *counted* in `[PSTATS]` and
never *timed*, so this walk was invisible to the instrument that found everything
else. It was found by reading the fault path after the syscall numbers stopped
explaining the wall clock.

#### The invariant, promoted

§8 made the region list sorted on amd64 to kill the per-`mmap` sort. That is
enough to make every walk above a binary search, so the invariant moved into
`akuma-mmap` as `insert_region_sorted`, documented there, with
`region_index_containing` (fault lookup) and `regions_overlapping` (range
queries) as the two searches built on it. **Both kernels now maintain it**:
AArch64's `mmap`/`mremap` sites in `akuma-syscalls-glue` and
`record_mmap_region` in `akuma-exec` insert in order rather than appending.

Everything else in `akuma-mmap` already preserved order and needed no change —
`inherit_mmap_regions_for_cow_child` maps over its input, `mprotect_eager_regions_in_range`
drains in order and emits each region's pieces ascending, and fork copies the
parent's snapshot in order.

#### The placement cursor

The first-fit scan is O(regions before the first adequate gap), which for a
process growing a dense arena is all of them. `find_free_va_from` starts at a
per-process cursor and **wraps once** to `MMAP_BASE` if that finds nothing.

It reuses `ProcessMemory::next_mmap` — the AArch64 kernel's own per-process mmap
cursor, which this target had never used. Reusing it rather than adding a field
is what makes `fork` correct for free: that kernel already copies it to the
child, and a child inheriting an empty cursor would re-walk its parent's whole
address space on its first `mmap`. Values outside this target's window are
clamped, which is what makes sharing a field initialised for the other kernel's
VA layout sound.

**What it gives up** is exact lowest-address first-fit: a hole below the cursor
waits for the wrap. That is weaker than the scan and *stronger* than the global
bump allocator this file replaced in the first place — that one was global
across every process and never reused anything. It also recycles a freed address
later than first-fit would, so a stale pointer keeps faulting for longer instead
of landing in a live mapping.

#### The wedge this caused, and the guard that came out of it

Making the searches depend on sortedness turns a missed writer from *slow* into
*wrong*. Two were missed — `mremap`'s `push` on amd64 and `record_mmap_region`
on AArch64 — and the result was an in-guest `rustc` that stopped making progress
with **no panic, no fault and no log line**: the §5 wall, reintroduced by a
different mechanism. It took a build to find because nothing else looks at a
4 000-region list.

So `detach_eager_regions_in_range` carries two things:

- a **`debug_assert`** that the list is sorted — host tests only;
- an **O(1) guard** that notices the last pair out of order and re-sorts. That
  is exactly the shape a writer that appends leaves behind, so it converts the
  commonest mistake from a wedge into one slower call. It is **a guard, not a
  proof**: two appends in ascending order leave the last pair sorted and the
  list still broken. Real enforcement is that every writer goes through
  `insert_region_sorted`.

The same O(1) guard replaced the placer's `windows(2)` pass, which was itself
O(regions) on the path the sort had just been removed from.

#### Result

`cargo build -p zerocopy --target x86_64-unknown-none --release --offline -j1`,
in-guest, FC 6144 MB, min of 3:

| | amd64 |
|---|---|
| §8 (sorted list, sort skipped) | 18.3 s |
| + binary-searched split, fault lookup and file-overlap pass | 16.4 s |
| + placement cursor, O(1) sorted-check | **15.1 s** |
| (start of the session, post-§5) | 63 s |
| Linux, same `rustc` binary, same box | 8.8 s |

**4.2x cumulative; the gap to Linux goes 7.2x -> 1.7x.**

`mmap_scale` on amd64, per-call placement cost against regions held: the slope
goes **4.02 -> 1.65 ns/region** with the cursor alone, and the 3 750-region
bucket **16 720 -> 7 600 ns**.

#### AArch64, measured

AArch64 never paid the sort (it places from a bump cursor and a free list), but
it walks the same list on `munmap`. A/B against a worktree at the pre-§9 commit,
same binary, same host, same probe:

| regions held | `munmap` before | `munmap` after |
|---|---|---|
| 0 | 3 000 ns | 2 600 ns |
| 750 | 8 300 ns | 2 400 ns |
| 2 500 | 8 400 ns | 2 900 ns |
| 3 750 | **8 400 ns** | **2 400 ns** |

**3.5x at 3 750 regions, and flat instead of climbing.** `mmap` is unchanged
(1 200-1 600 ns, already flat). Boot suite 311 PASSED / 0 FAILED.

#### The probe had to be fixed first

`mmap_scale` originally timed a 250-call bucket as one bracket and took the best
of `repeat` passes. That assumes a pass exists in which the whole bucket ran
undisturbed — false under QEMU TCG, where it reported buckets alternating
between 1.5 us and 25 us with no relation to region count and a `growth=16.9x`
that was purely which bucket got unlucky last. It now times **groups of 10**:
short enough that most brackets escape preemption, long enough to amortise the
two clock reads. That is what turned the AArch64 numbers above from noise into a
flat line. *A benchmark that cannot resolve the effect is not a null result.*

#### Verification

amd64 boot suite **735 passed / 0 failed at SMP=1**, **745 / 0 at SMP=4** (+5
over §8: the placement-cursor self-tests, which cover the hint being honoured,
a hole below the hint being skipped, an out-of-window hint being clamped, the
wrap finding that hole when there is no room above, and a full address space
answering `ENOMEM` rather than a bad address). AArch64 boot suite 311 / 0. Host
tests 1 460 / 0 (`akuma-mmap` 68, including an oracle test for
`regions_overlapping` against the filter it replaced, and one for the
append-healing guard). Clippy clean on both kernels and on `akuma-mmap`.

#### Open

- The remaining 1.7x against Linux is **not syscalls**: at 15 s the whole
  measured syscall and fault budget is ~2 s.
- `munmap` still carries a TLB-shootdown IPI per call at SMP>1 — 13.2 us against
  1.97 us at SMP=1, measured on the same kernel.


### 10. The full kernel build: two blockers found, one fixed (2026-09-18)

*§9 got `zerocopy` to 13.2 s. The goal behind it is the whole in-guest kernel
build, and pointing the rig at that turned up three things — one of them a rig
defect that had been quietly making the measurement impossible.*

#### The guest's own source tree did not compile

`cargo build -p akuma-amd64` in the guest failed with two `E0425`s:
`lapic.rs:174` calling `crate::sched::note_tick_sample` and `net.rs:828` calling
`crate::sched::tick_profile`, neither of which exists in the guest's `sched.rs`.

Not a code bug — a **partially synced tree**. `/src/akuma/amd64/src/lapic.rs`
was dated 2026-09-17 and `sched.rs` 2026-09-12: an earlier session copied some
amd64 sources into the image and missed one. Every `-j4` run in this doc stopped
short of `akuma-amd64` itself (the wedge, or a crash, came first), so nothing
had ever compiled the kernel crate in there and the breakage stayed invisible.

Repaired by mounting the image with the guest down and `rsync -a --exclude
vendor` of `amd64/` and `crates/` from the box's tree (~12 MB). **`rsync -a`
preserves mtimes, which is what you want here**: cargo then rebuilds only the
crates whose contents actually changed, instead of the whole graph.

The lesson is the one `docs/` already carries about partial copies, in a new
place: *the image is a build input, and it drifts*. If an in-guest build fails
with a missing symbol, suspect the image before the code.

#### FIXED: the ext2 block cache was sized against RAM, inside a fixed heap

With the tree buildable, `-j1` died:

```
[HEAP] 505MB used (alloc=165777608 bytes)
[ALLOC FAIL] requested=65536 heap_total=512MB heap_used=510MB (99%) peak=510MB
[OOM] allocation of 65536 bytes failed
```

`165 777 608` is exactly the size of
`/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/rust-lld` — the
linker the kernel's own build finishes with. **`execve` on this target holds the
whole image in one kernel-heap `Vec`.** It reads it in 64 KiB chunks and
reserves it fallibly (`try_reserve_exact`, see `fs.rs`'s "exec-side image read"),
so the 158 MB reservation itself succeeded; what failed was the next 64 KB
allocation after it.

It had room to fail because of §7's cache change. `akuma-ext2`'s cap was set to
`min(RAM/8, FSCACHE_CEILING_MB)` — correct on AArch64, where the heap grows, and
wrong here, where the heap is a fixed `mem::HEAP_SIZE` of 512 MB. At 6 GB of
guest RAM that formula gives **384 MB of cache inside a 512 MB heap**, leaving
128 MB for everything else including a 158 MB exec.

The cap now also takes `HEAP_SIZE / 4`. A fraction rather than "leave N bytes
free", because the thing being protected against is a single request the size of
a linker, and a constant would go stale the first time `HEAP_SIZE` moved.

**The underlying defect is still open**: `execve` should stream `PT_LOAD`
segments into user pages rather than hold the whole file. The cap makes a 158 MB
binary fit; a 400 MB one would not, and `librustc_driver.so` in this very image
is 311 MB (it is `mmap`ped lazily, not exec'd, which is why it does not blow up
today).

#### Result: the kernel builds itself again

`cargo build -p akuma-amd64 --target x86_64-unknown-none --release --offline
-j1`, FC 1 vCPU / 6144 MB, **incremental** (3 crates — the ones the re-sync
actually changed): **1 m 18 s, rc=0**. That is the first time this doc records
the amd64 kernel crate compiling in-guest at all.

#### Still open, and it is the thing between here and a fast build: the `-j4` wedge

Reproduced on the current kernel at SMP=4 `-j4`: all progress stops after ~27
crates, ~4 minutes in. The new capture **contradicts the one in § "The `-j4`
verification run"**, which blamed a `#GP`/SIGSEGV kill pair and cargo waiting on
a child that had died:

- **The guest vCPU is 7 % busy** over a 21 s host-side sample. Nothing is
  running; this is not a livelock or a slow crate.
- **The children are still there.** `ps` shows `rustc --crate-name
  akuma_primitives` (pid 69) and a `zerocopy` build script (pid 71), both at
  **`0:00` CPU** — created and never scheduled. `cargo` itself has 0:14 and is
  waiting.
- **No kill of any kind**: no `#GP`, no `#UD`, no SIGSEGV, no "killed by
  signal", no `[PANIC]`, no `[BKL] stuck`, no OOM.
- The only console output is a `[TRAMP-MISMATCH]` burst, every line naming
  `tid=14` and `table scan found 69` — the stuck `rustc` — while the
  `THREAD_PID_MAP=` value climbs with each newly created process.

So the shape is **a process that exists but is never scheduled**, not a process
that died unnoticed. `SMP=1`/`-j1` does not reproduce it.

This is what gates a fast in-guest kernel build: `-j1` is reliable and slow,
`-j4` is the only way to a ten-minute build, and it wedges.

#### SMP costs money when the jobs do not use it

`zerocopy` `-j1` on the fixed kernel, min of 3, same image and kernel, only
`vcpu_count` changed:

| SMP=1 | SMP=2 | SMP=4 |
|---|---|---|
| **13.24 s** | 13.80 s | 16.62 s |

**+26 % at SMP=4 for a single-job build.** The extra cores do no work and cost
real time: a TLB-shootdown IPI per `munmap` (`mmap_scale` on the same kernel
puts the `munmap` floor at 1.97 µs at SMP=1 against 13.2 µs at SMP=2) plus BKL
contention. Worth knowing before reading any SMP>1 number in this doc as a
regression.


### 11. Bare metal: the rig now self-hosts (2026-09-18)

*The Firecracker number wanted a bare-metal counterpart — same kernel, same
source, no hypervisor. Two rig defects stood in the way; both are fixed, and the
recipe below is what the partition now holds.*

**`E0463`: no `core` for `x86_64-unknown-none`.** The bare-metal root
(`/dev/sdb1` on the Ubuntu side, `/dev/sda1` to Akuma — a 64 GB ext2 partition
on the USB disk) carried a rust toolchain at `/usr/local/rust` whose
`lib/rustlib/` held **only `x86_64-unknown-linux-musl`**. That builds host
binaries and proc macros; it cannot build the kernel. 949 MB against the metal's
778 MB, and this was most of the difference.

It cannot be papered over from a laptop: the rlibs must come from the *same*
rustc build, and the box is `1.100.0-nightly (0fc141305 2026-09-11)`.

**Vendor from git, not from the image.** The first attempt copied the whole tree
out of the Firecracker image. That is wrong twice over: `vendor/` and the
`[source.crates-io] replace-with = "vendored-sources"` block are **not in git**
— they are rig state — so copying the image's tree also imports the image's
*Cargo.lock*, and the two drift. Measured here: the image's `vendor/` carried
syn 2.0.114 / quote 1.0.44 / proc-macro2 1.0.106 where the checkout's lock wants
2.0.111 / 1.0.42 / 1.0.103, so the pair would not resolve offline at all.

The box has network, so it vendors against its own checkout and the two cannot
disagree:

```sh
mount /dev/sdb1 /mnt/ak
mount -o loop /root/akuma-fc-rust.img /mnt/fcrust
# 1. source from git, protecting the two rig-state paths
rsync -a --delete --exclude target --exclude .git --exclude vendor \
      /root/akuma/ /mnt/ak/root/akuma/
# 2. vendor against THIS checkout's Cargo.lock (19 MB, not the image's 44 MB)
cd /root/akuma && cargo vendor --versioned-dirs /mnt/ak/root/akuma/vendor
# 3. the offline block, appended to .cargo/config.toml (not in git)
# 4. the toolchain, for the missing bare-metal target
rsync -a /mnt/fcrust/usr/local/rust/ /mnt/ak/usr/local/rust/
umount /mnt/fcrust /mnt/ak
```

**Verified without rebooting.** `chroot` into an executable copy of the
Firecracker image with the bare-metal tree bind-mounted over it, and build the
crate that had failed:

```
Compiling akuma-cpu v0.1.0 (/bm/crates/akuma-cpu)
Finished `release` profile [optimized] target(s) in 2.84s     rc=0
```

That is the same trick §8 used for the Linux reference arm, and it is the cheap
way to test a rootfs change: a reboot of this box costs a round trip through
GRUB and, if the ssh key is wrong, a walk to the machine.

State of the partition afterwards: `/root/akuma` at `33577243`, `vendor/` 19 MB
matching its `Cargo.lock` byte for byte, offline config, `/root/.cargo`,
toolchain with both targets, 57 GB free.

#### Then the link died: **LLD's threading, not its size**

With the toolchain fixed, the build ran all the way to the end and stopped
there, three times out of three:

```
error: linking with `rust-lld` failed: signal: 11 (SIGSEGV)
```

`rust-lld` is 158 MB and this target's `execve` holds a whole binary in the
kernel heap (§10), so "the linker is too big" is the obvious guess and it is
wrong. The same build **links fine under Firecracker at SMP=1**; the metal was
booted SMP=4.

Settled without rebuilding anything, by replaying the exact invocation. LLD
prints its own argv in the crash dump, so the command is in the build log:

```sh
ARGS=$(sed 's/.*Program arguments: rust-lld //' /root/lld_cmd.txt)
$LLD $ARGS                 # rc=142, stack dump
$LLD $ARGS --threads=1     # rc=0, 3 364 832-byte kernel
```

**LLD is multi-threaded by default**, and its thread pool is what this kernel
cannot survive at SMP>1 — the same family as the `-j4` wedge above, reached by a
different route. It is also why the failure looked like two different bugs: the
first run reported `rc=139` (cargo relaying SIGSEGV) and the second `rc=101`
(cargo reporting a link failure); both are the same linker crash.

The rig carries `-C link-arg=--threads=1` in `/root/akuma/.cargo/config.toml`'s
`x86_64-unknown-none` rustflags. **Rig state, not source** — the same argument as
`vendor/` and the offline block: it is a workaround for a kernel defect, and
putting it in `build.rs` would bake that defect into the repo. Note the cost of
that placement: rustflags are part of every crate's fingerprint, so adding it
rebuilds the whole graph once. `build.rs`'s `rustc-link-arg-bins` would have
invalidated only the kernel binary, and is the better home if this ever becomes
permanent.

This is the lever for the `-j4` wedge too. The wedge needed four concurrent
`rustc` processes; this needs one multi-threaded process. A single linker
reproducing it in ~20 minutes with a deterministic, replayable command line is a
far cheaper harness than a 95-crate build.

#### And with the linker fixed, `rustc` corrupts instead — `cr2` is ASCII

With `--threads=1` in the rustflags the build no longer dies at the link. It
dies earlier, at **24 crates**, and the kernel names the victim:

```
#PF: not-present read from ring 3
[Fault] #PF page fault in ring 3 on cpu 3 err=0x0000000000000004
        rip=0x00000000300469c6 rsp=0x00007fffffff72c8
        cr2=0x00004d5f4e4f4964 cr3=0x0000000040707000
        task=10 pid=14304 — killing the process
```

**`cr2 = 0x00004d5f4e4f4964` is not a pointer, it is text**: little-endian, those
bytes are `d I O N _ M` — the tail of something like `…SION_M…`. A pointer in
`rustc`'s address space was overwritten with string data and then dereferenced.
That is memory *corruption*, not a wild read, and it is the kernel's to answer
for: the same binaries build cleanly at SMP=1.

Alongside it in the same ring buffer: **245 `[BKL] stuck` lines** and a
continuous `[TRAMP-MISMATCH]` storm with pids in the 16 000s (the build spawns
that many processes), each naming a tid whose `THREAD_PID_MAP` owner has moved
on while a table scan still finds the old one.

So SMP>1 on this target has at least three faces, and they are probably one bug:

| symptom | where seen |
|---|---|
| processes created, never scheduled, build wedges | Firecracker SMP=4 `-j4` (§10) |
| `rust-lld` SIGSEGV at the final link | bare metal SMP=4, deterministic |
| `rustc` dereferences a pointer overwritten with ASCII | bare metal SMP=4, 24 crates in |

All three involve **multi-threaded user processes**, which is the thing SMP=1
never exercises concurrently. That is the next investigation, and the linker is
the cheapest entry point into it.

#### `nosmp` settles it: the metal builds its own kernel

Same disk, same toolchain, same tree, same kernel binary — only `nosmp` added to
the command line:

```
Finished `release` profile [optimized] target(s) in 22m 52s
-rw-r--r-- 1 0 0 3364832 /root/ktarget/x86_64-unknown-none/release/akuma-amd64
```

**rc=0, 95 crates, 22 m 54 s.** Every SMP=4 failure above disappears at one core,
which is what makes SMP the variable rather than the USB disk, the linker's size
or the toolchain.

| | clean 95-crate `-j1` build |
|---|---|
| Firecracker, SMP=1, image on the internal SSD | **3 m 43 s** |
| bare metal, `nosmp`, root on the USB disk | **22 m 54 s** |

**6.2x** — on the same physical machine. (**Corrected 2026-09-18:** "almost
certainly storage" was the guess here and it measured wrong — the device is only
1.4–2.3x; the rest is the 128 MB ext2 block cache. The last sentence of this
paragraph anticipated the right experiment; `AKUMA_AMD64_BARE_METAL_SELFHOST.md`
§2 ran it.) The Firecracker guest reads its root through a file on
Ubuntu's ext4/SSD, so the *host's* page cache sits in front of every block; the
metal reads a USB disk with nothing in front of it but this kernel's own 128 MB
ext2 block cache. Worth confirming before optimising anything on the metal: an
in-guest build is the wrong place to look for a 6x that a `dd` would show.

**Untested, and honestly so:** whether `rust-lld`'s default threading also
crashes at `nosmp`. It could not be checked cheaply — `rustc` deletes its
`.rcgu.o` files after a successful link, so replaying the link needs them
regenerated, and regenerating them with default threading means taking
`--threads=1` out of the rustflags, which invalidates every crate's fingerprint
and costs the whole 23 minutes again. The prior is that it is SMP-only (one core
cannot run LLD's pool concurrently), but it is a prior, not a measurement.

#### The trap that cost a power cycle: stage a key you actually hold

`amd64/mkdisk.sh` stages `target/x86_64-unknown-none/release/amd64-ssh-test-key.pub`
into the RAM image, and with `root=/dev/sda1` **sshd reads the persistent root's**
`/etc/sshd/authorized_keys` instead. Those are two different files, and the
laptop's `~/.ssh/config` `akuma` alias points at a *third* thing — the key file
in the local checkout, which `target/` cleaning removes and rebuilds.

All three drifted apart here. A key was generated **on the box** to get into the
Firecracker guest and appended to the persistent root; its private half stayed on
the box's Ubuntu disk. Booting bare metal then took Ubuntu down and with it the
only copy of the private key — port 2222 answered `SSH-2.0-Akuma_0.1` and nothing
could authenticate. Recovery was a power cycle (harmless: `grub-reboot` arms a
**one-shot** entry, so the next boot returns to Ubuntu by itself).

**The rule this earns:** before rebooting the box out of Ubuntu, verify that the
key on the laptop is in the persistent root's `authorized_keys` — by string, not
by assumption:

```sh
K=/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key
mount /dev/sdb1 /mnt/ak
grep -qF "$(ssh-keygen -y -f $K | awk '{print $2}')" /mnt/ak/etc/sshd/authorized_keys && echo OK
```

and make that same key the one `mkdisk.sh` stages, so the RAM-image fallback
authorises it too. Both are true on the box now, and `ssh akuma` works from the
laptop.


### 12. The wedge, cornered: both variables are necessary, and it is not the free path (2026-09-18)

> **Corrected 2026-09-18 by §13.** The matrix, the cheap repro and the
> free-path elimination below all stand. The *characterisation* does not: this
> section calls the primary failure "runnable tasks that no core ever picks",
> and the instrument it asks for in next-step 1 was built and says the
> opposite — every wedged thread is `WAITING` in `futex`, scheduled about once
> a second, with no leaked `ON_CPU` gate anywhere. Read §13 before acting on
> the next-steps list here; items 1 and 2 are answered and item 2's hypothesis
> is retired.

*§11 left three symptoms and a guess that they were one bug in "multi-threaded
user processes at SMP>1". This session measured the variables separately,
reproduced the wedge twice on a **single-crate** build, and eliminated the
mechanism the AArch64 ancestor turned out to be.*

#### The matrix: neither variable alone is enough

`scripts/benchmarks/amd64_fc_build_matrix.py` builds one crate in the box's
Firecracker guest across an SMP x jobs matrix. `zerocopy`,
`x86_64-unknown-none`, `--release --offline`, `cargo clean -p` before each
cell, kernel at `bb65c927`, guest 6144 MiB:

| vcpu | jobs | outcome | wall | cargo's own |
|---|---|---|---|---|
| 1 | 1 | PASS | 32.1 s | 31.29 s |
| 1 | 4 | PASS | 13.6 s | 12.95 s |
| 4 | 1 | PASS | 17.4 s | 16.79 s |
| **4** | **4** | **WEDGE** | 600 s (budget) | — |

**Several processes alone do not wedge it; several cores alone do not wedge
it; together they do.** That is the first time the two have been separated,
and it retires "is it just SMP?" and "is it just job concurrency?" in one
table.

Two readings to be careful about. The 1x1 cell ran first after a boot and
paid a cold block cache, so its 32.1 s is not evidence that `-j4` is faster
than `-j1` at one core — do not quote that pair. The 4x1-against-1x4 gap
(17.4 s against 13.6 s) is the same +26 %-ish SMP tax §10 measured, and that
one is real.

**This is now the cheap repro.** §10 needed 95 crates and ~27 of them to get
here; `zerocopy` alone reaches it in about ten minutes, and the wedging unit
is `zerocopy`'s **build script**, which is two `rustc` invocations. Both
attempts wedged, so it is deterministic rather than a 1-in-N.

#### What a wedged guest looks like, from two runs

Run 1 (default kernel). The console carries a kill:

```
[Fault] #PF page fault in ring 3 on cpu 0 err=0x0000000000000004
        rip=0x00000001028d0b20 rsp=0x000000011d3b7e40
        cr2=0x0000000000000034 cr3=0x0000000023862000 task=16 pid=64
        — killing the process
```

`cr2=0x34` is a **near-NULL read** — a null struct pointer plus a field
offset — and `pid=64` is the `rustc` running the build script. Alongside it:
8 `[TRAMP-MISMATCH]` lines and, notably, **zero `[BKL] stuck`**. The storm
that has accompanied every previous SMP=4 incident on this target is simply
absent, so the `[BKL] stuck` family and this wedge are not the same thing.

Run 2 (the forensics kernel below). Same wedge, vCPU 7.0 %, 7
`[TRAMP-MISMATCH]` — and **no kill of any kind**. What `ps` shows is the
better evidence:

```
    1 0     0:08 /bin/sshd
   60 0     0:24 cargo build -p zerocopy ... -j4
   61 0     0:00 {futures-timer} cargo build ...
   62 0     0:00 cargo build ...
   63 0     0:00 cargo build ...
   64 0     0:00 rustc --crate-name build_script_build ...
   65 0     0:00 {ctrl-c} rustc --crate-name build_script_build ...
   67 0     0:00 rustc --crate-name build_script_build ...
   68 0     0:00 {coordinator} rustc --crate-name build_script_build ...
```

**Two `rustc` processes and both their threads, every one at 0:00 CPU**, with
`cargo` the only thing that ever ran. Not one of them has been scheduled
once. That is §10's "created and never scheduled" shape, now with the thread
names visible, and it is the dominant shape: run 1's kill is the exception,
not the rule, and may well be downstream.

So the ordering of the two open symptoms should be inverted from §11's. The
primary failure is **runnable tasks that no core ever picks**. The near-NULL
kill is a second thing that sometimes happens on the way.

#### Eliminated: the free path. `[PMM-UAF]` and friends stay silent

`amd64/src/mem.rs` passed `PmmConfig { cow_ref_ledger: false,
pmm_uaf_quarantine: false, pmm_premature_free_check: false }` with a comment
justifying it: *"this kernel has no page cache, no retired-process list and no
CoW"*. **All three of those are now untrue** — CoW fork is `akuma_cow` in
`idt.rs`, the file-page cache is `akuma_fpcache` in `fs.rs`, and the
`drain_retired` hook three lines below the config is live. The flags had
quietly become a cost decision wearing a capability comment, and what they
switch off is the instrument that cracked the analogous AArch64 bug:
`SELFHOST_ZERO_PAGE_HUNT.md` §8's `sys_munmap` freeing the frame its *region
record* named instead of the one the live PTE held, ~11,000 times per build,
found by `[PMM-UAF]` in one boot after six mechanisms had been eliminated by
guessing.

They are now behind `--features pmm-forensics` on `akuma-amd64`, off by
default (every free poisons a page and parks it in a 512-entry ring; the
premature-free check walks for a surviving mapper on every free). One call
site, reached by both entry points, so it cannot go the way of the
`exec_runtime::init` divergence. The build prints
`pmm: forensics ON (quarantine + premature-free + CoW ledger)`, which is what
lets a run prove which kernel it was — the boot log of the run below carries
that line.

**Result: zero `PMM-` reports across a full reproduced wedge.** No
`[PMM-UAF]`, no `[PMM-PREMATURE]`, no `[PMM-RESURRECT]`. So the amd64 wedge
is **not** the AArch64 free-path family.

The honest caveat: the wedge arrives early — during a build script, a few
seconds of real work — so the number of frees before it is small, and this
clears the free path *up to the wedge point* rather than in general. It does
not clear the bare-metal `cr2=0x00004d5f4e4f4964` corruption at 24 crates,
which happens far later and has never been run under this instrument. Run
the metal with `pmm-forensics` before treating that one as cleared too.

#### `mtstress`: a calibrated probe that does **not** reproduce it

`userspace/amd64/mtstress/mtstress.c` is the LLD-shaped probe §11 asked for —
one process, many threads, one address space — with five arms: pointer
integrity (self-pointers, and the report prints the bad word as ASCII because
the bare-metal `cr2` *was* ASCII), shootdown churn, thread-pool churn, a
heartbeat watchdog for "created and never scheduled" seen from inside, and a
fault-kill reaping arm that forks a multi-threaded child, kills it with the
same near-NULL write, and waits with a deadline.

Driven by `scripts/benchmarks/amd64_mtstress_run.py`, which runs the same
static musl binary on the box's own Ubuntu first.

| arm | verdict |
|---|---|
| linux (calibration) | PASS |
| akuma FC vcpu=1 | PASS |
| akuma FC vcpu=4 | PASS |

120 s, 4 threads, all arms. **It does not reproduce the bug**, and that is
worth recording rather than tuning away: the matrix says several *processes*
are necessary, and every arm of this probe lives in one. A single-process
probe cannot reach this failure however hard it churns. What the run does buy
is the elimination of the simple stories — sibling-thread shootdowns, pointer
corruption under thread churn, and fault-killed multi-threaded children going
unreaped are each fine in isolation at SMP=4.

**The calibration arm earned its keep on its first run.** On Linux the probe
reported 62 findings, all `peer-self-pointer ... got=0` — its own race:
`main` mmaps every arena before creating any thread, so a thread reads a
peer's arena before that peer has filled it and sees legitimate zero pages.
A start barrier fixed it. Had that arm been skipped, the probe would have
reported "null pointers under SMP" against Akuma and been believed, because
it is precisely the shape being hunted.

#### Also: §11's `nosmp` LLD control never actually ran

§11 recorded as "untested, and honestly so" whether `rust-lld`'s default
threading also crashes at `nosmp`. The box still held the attempt
(`/root/lldctl2.sh`, `/tmp/a.err`, `/tmp/b.err`), and **both arms died
identically** before reaching the question:

```
rust-lld: error: duplicate symbol: main
>>> defined at hello.9466052f020585b9-cgu.0
>>> defined at hello.cdb25a3e6f7aad39-cgu.0
```

The object glob picked up two `hello` build's `.rcgu.o` files, so neither the
threaded nor the `--threads=1` arm linked the kernel at all. The control is
invalid and **§11's question is still open**. Pin the object list to one
crate's `out` directory before re-running it.

#### Next, in order

1. **Dump every thread slot's `(state, ON_CPU, LAST_CORE)` at wedge time.**
   This is the one instrument the "never scheduled" shape needs and it does
   not exist on this target: `akuma_threading::x86_slot_debug(slot)` already
   returns `(state, on_cpu)` and has exactly one caller (`net.rs:1234`). The
   hypothesis it tests is specific — `x86_pick_next` skips any candidate whose
   `ON_CPU` is non-zero, so a slot whose gate was left set is **permanently
   unpickable**, which would present as a task created and never run, only at
   SMP>1, and more often the more slots are recycled. All three match.
2. **Read `x86_yield_now`'s gate ordering against that.** It clears
   `ON_CPU[cur]` **before** `x86_switch_context` moves the stack, and the
   whole safety argument is the comment's "no other core can observe that
   until this core releases the kernel lock". That is weaker than the AArch64
   original, which clears the gate from the vector asm *after* `mov sp, x0`
   (`SMP_SHARED_ONCPU_GATE.md` §3). The kernel already doubts it: there is a
   `SWITCH_WITHOUT_BKL` counter and a `[SWITCH NO-BKL]` report for the case
   where the lock is not in fact held, and `x86_check_incoming_frame` carries
   a `[SWITCH FRAME MOVED]` report that fired once already — "a thread's saved
   frame moving while the switch that is about to restore it looks on".
3. Then the bare metal with `pmm-forensics`, for the `cr2`-is-ASCII
   corruption at 24 crates, which this session did not reach.


### 13. The wedge is a futex stall, not a scheduler starvation — §12's premise was wrong (2026-09-18)

*§12 named the primary failure "runnable tasks that no core ever picks" and
made the scheduler the next place to look. The instrument it asked for was
built, and it says the opposite: every wedged thread is **parked**, every one
of them in `futex`, and the scheduler is picking them roughly once a second
and has never lost a gate.*

#### The instrument

Three things that did not exist on this target, all printed from the idle
loop's existing 30 s `[PSTATS]` block:

- **`sched::dump_slot_table`** — one line per live slot, with the four facts
  that separate the ways a task can fail to run: the thread state, the `ON_CPU`
  gate, a **switch-in count** (`ins`), and the picker's own per-slot tally of
  hits / skipped-for-gate / skipped-for-pinning. Plus the last syscall each slot
  *entered* (`sc`) and how many it has entered (`scn`), recorded in
  `usermode::syscall_handler`.
- **`futex::dump_waiters`** — every queued waiter by `(tgid, uaddr)` key, with
  each waiter's age, and the `FUTEX_WAKE` tallies including wakes that found
  nobody.
- **`scripts/benchmarks/amd64_slot_report.py`** — reads two consecutive blocks
  and prints the **difference**, which is the only form in which these numbers
  answer the question.

**`ins` and `scn` together are the whole finding, and `ps` cannot express
either.** `ps`'s `TIME` column is `m:ss`, so a thread scheduled 600 times that
parks immediately each time reads `0:00` exactly like a thread that has never
run — which is how "created and never scheduled" survived two sessions.

The instrument also caught a defect in itself on its first boot, which is worth
recording because the same mistake is available to anything per-slot here: the
counters are per **occupant**, not per slot index, and without a reset at
`x86_claim_slot`/`prepare_task_slot` they accumulate across every rebirth. Three
idle threads reported eleven syscalls each, ending in `exit_group`, left behind
by the boot self-tests' tasks in the same low slots. Uncorrected, `ins = 0`
would have been read as "never scheduled" for a slot that had been recycled.

#### What a wedged guest actually contains

Same cell as §12 (`zerocopy`, vcpu=4, `-j4`), same outcome — WEDGE at the 600 s
budget, vCPU 8.2 % — and the last two 30 s blocks differ like this:

```
 slot        pid    state         sc gate       ins   +ins       scn    +scn
    0       None  WAITING          -    1     18486   +678         0      +0   boot thread
    1       None  RUNNING          -    1     58841  +2966         0      +0   idle, core 1
    2       None  RUNNING          -    1     58504  +2946         0      +0   idle, core 2
    3       None  RUNNING          -    1     59664  +2977         0      +0   idle, core 3
    5    Some(1)  WAITING  nanosleep    0    177668  +8930    709533  +35720   sshd
    7   Some(60)  WAITING      futex    0      1213    +59     22652    +472   cargo
    8   Some(61)  WAITING      futex    0       595    +30        79      +0
    9   Some(62)  WAITING      futex    0       595    +30        24      +0
   10   Some(63)  WAITING   recvfrom    0       594    +30        24      +0
   11   Some(64)  WAITING      futex    0       602    +30       186      +0   rustc
   12   Some(65)  WAITING      futex    0       594    +30         7      +0
   13   Some(66)  WAITING      futex    0       654    +30     18596      +0
   14   Some(67)  WAITING      futex    0       596    +30        11      +0
   15   Some(68)  WAITING      futex    0       599    +30        35      +0

leaked on-CPU gates: none
```

Read it in this order:

1. **`st` is `WAITING` (5), not `READY`.** No thread in the system is runnable
   and unpicked. The starvation hypothesis has nothing to stand on.
2. **`gate=0` on every user thread, and `gated_dead=0` in all twenty census
   lines.** The leaked-`ON_CPU` hypothesis §12 put first is dead. The four set
   gates are the boot thread and the three idle threads, which hold them because
   they are running.
3. **`+ins` is ~30 per 30 s and `+scn` is exactly 0.** Each of these threads is
   scheduled about once a second, runs, and makes no syscall. That is the
   signature of the untimed-park backstop (`akuma_threading`'s
   `UNTIMED_PARK_BACKSTOP_US`) releasing a waiter, the futex wait loop
   re-testing its table membership, finding itself still queued, and parking
   again. Forever: the loop's only other exits are a dequeue and
   `should_leave_now`.
4. **`sc=202` is `futex`** on eight of the nine user threads, and `sc` is the
   last syscall *entered*, so a `WAITING` thread is parked inside it.
5. The two address spaces are `0x208a4000` (cargo, four threads) and
   `0x23864000` (rustc, five threads). Both are entirely parked.

`cargo` is the one thing still moving — 472 syscalls in the last 30 s — so the
guest is not frozen, it is deadlocked around the futex table. sshd answers ssh
throughout, which is why `ps` works at all.

#### What this retires

- **"Created and never scheduled" is wrong**, and §10, §11 and §12 all lean on
  it. The threads were scheduled hundreds of times each. Nothing in the
  scheduler was ever the suspect the shape suggested.
- **The `ON_CPU` gate ordering question (§12 next-step 2) is moot for this
  bug.** It may still be worth tightening on its own merits — amd64 clears the
  gate before the stack moves where AArch64 clears it after `mov sp, x0` — but
  no gate was leaked in a full reproduced wedge, and `[SWITCH NO-BKL]`,
  `[SWITCH FRAME MOVED]` and `[BKL] stuck` were all silent (`tripwire lines in
  dmesg: 0`).
- **The orphaned-process hypothesis is eliminated too.** `[PROC-ORPHAN]` — the
  hook the previous session wired into this same idle-loop block — printed
  **zero** lines, as did `[unregister] … has NO map owner`, the suspected
  `x86_claim_slot`-window killer. Every process has a live thread; the threads
  are simply asleep. Eight `[TRAMP-MISMATCH]` lines do appear, at the same rate
  as §12's, and remain unexplained but are evidently survivable.

#### Also closed: three silent bail-outs in the spawn trampoline

`entry_point_trampoline` abandons a thread before its first user instruction in
three places — no process resolved, the thread already `TERMINATED`, the process
already exited — and each ended in `mark_current_terminated(); loop { yield_now()
}`. One carried a `log::debug!`; the other two were completely silent. The
middle one also leaves the **process** registered with no live thread, which is
the orphan shape three separate investigations have landed on. They now report
`[TRAMP-BAIL] tid=… reason=…`, bounded at 64 lines, with a `trampoline_bail_count()`
for the tally after that. This is a diagnostic gap closed, not a fix: on the run
above the count was zero.

#### The root cause: a group-fatal kill that cannot reach a parked leader

The second 4x4 wedge of the day carried the line the first did not:

```
[Fault] #GP general protection in ring 3 on cpu 0 err=0x0
        rip=0x00000000300465ec rsp=0x11d3b7810 cr2=0x10
        cr3=0x000000002383d000 task=16 pid=64 — killing the process
```

`cr3=0x2383d000` is `rustc`'s address space and `pid=64` its tgid. A worker
thread `#GP`ed. Ten minutes later the slot table said `term=6 wait=9`: six of
`rustc`'s threads dead, **and the leader still parked in `futex`, queued on one
key for 563 s**, with `[FUTEX] wakes=` frozen at 13 820 for the whole wedge —
not one `FUTEX_WAKE` issued by anybody, in either direction.

So "killing the process" killed the workers and left the leader asleep. The
process can never exit, so `cargo`'s `wait4` never returns, so the build hangs.

The mechanism is three functions that each behave exactly as documented:

1. `idt::user_fault` → `usermode::kill_current_from_fault` →
   `signal::notify_group_of_thread_fatal`, which records a **group exit
   status** and calls `deliver_signal(tgid, sig)`. Its own comment states the
   contract: *"every group member's next syscall return takes this exit"*.
2. `signal.rs`'s syscall-return epilogue honours that, unconditionally and
   ahead of every disposition — which is what stops a leader with a `SIGSEGV`
   handler (rustc installs one) from *handling* the notification and living on.
   That half was fixed on 2026-09-13 and works.
3. `futex::wait`'s park loop has exactly three exits: dequeued, deadline
   passed, or `thread::should_leave_now()`.

**A leader parked in an untimed `FUTEX_WAIT` has no next syscall return**, so
(1)'s contract is never discharged and (2) never runs. And (3) cannot save it,
for two independent reasons: `should_leave_now()` returns `false` for the main
thread *by construction* — `thread::drain` is called by the leader and must not
interrupt itself — and it reads `GROUP_EXIT`, which only `exit_group` and
`drain` ever set, neither of which is on a fault's path. The deadline is
`NEVER`. So the loop is closed, and the 1 s untimed-park backstop faithfully
wakes the thread every second to re-confirm that it is still stuck — which is
the `+ins 30 / +scn 0` signature above.

#### The fix

One check, beside the `should_leave_now` arm it cannot substitute for:

```rust
if akuma_exec::process::should_interrupt_blocking_syscall() {
    let _ = unsafe { (*waiters()).remove_anywhere(tgid, me) };
    return errno::EINTR;
}
```

That is the predicate every blocking arm in `akuma-syscalls-glue` already
consults, and the one `deliver_signal` actually sets. It **takes** the
interrupted flag rather than peeking, so it cannot become an `EINTR` storm;
`EINTR` is what Linux returns from an interrupted `FUTEX_WAIT`; and musl's
retry is precisely what carries the thread through the syscall epilogue where
`group_exit_status` is waiting for it.

`fd.rs`'s console/stdin read loop had the identical hole — an untimed park
whose only exit is a byte arriving — and got the identical check. That one is
not on the build path; it is the shape an interactive `sshd` session hangs in
when its process is killed while waiting for a keystroke.

#### The A/B: 600 s of silence becomes 2.2 s of error

Same harness, same crate, kernel rebuilt with the one check:

| vcpu | jobs | before | after |
|---|---|---|---|
| 4 | 1 | PASS 17.4 s | **PASS 17.4 s** — unchanged |
| 4 | 4 | WEDGE 600 s | **ERROR 2.2 s** |
| 4 | 4 | WEDGE 600 s | **ERROR 2.2 s** |

and what cargo now says, which it could never say before:

```
rustc --crate-name build_script_build … (signal: 11, SIGSEGV: invalid memory reference)
```

Deterministic both ways: two wedges before, two reported crashes after, and the
healthy cell unmoved to the tenth of a second. **The bug that took 600 s to
observe now reproduces in 2.2 s and prints its own name.**

**What this does not fix.** The `#GP` itself. With this change `rustc` dies
with a `SIGSEGV` the build *reports* instead of a 600 s silence, which is a
much better failure and a bisectable one, but it is still a failure. The
faulting `rip=0x300465ec` is in the dynamic loader's range and is a different
site from §12's `#PF cr2=0x34 rip=0x1028d0b20`; whether those are one
corruption or two is the next question.

#### Two more defects found on the way, both fixed

- **The futex waiter table was never purged on this target except from
  `thread::teardown`.** `akuma-threading` reaches `SLOT_PURGE_CALLBACK` from
  `mark_thread_terminated` and from its slot recycler; amd64 uses neither — it
  claims a `TERMINATED` slot directly in `x86_claim_slot` — and registered no
  callback at all. A thread that died without running `teardown` (a fault kill,
  a group-fatal signal) left its tid queued on a futex key forever, and the next
  occupant of its slot inherited the entry, where a `FUTEX_WAKE(uaddr, 1)` would
  spend itself on the corpse. `amd64::sched::install_untimed_park_backstop` now
  registers `futex::purge_task`, and `x86_claim_slot` runs the hook. Exactly the
  trap `thread::teardown`'s own comment describes for `set_cleanup_callback` —
  reasoned about for that hook and not for this one.
- **Three silent bail-outs in `entry_point_trampoline`** (§13 above), now
  `[TRAMP-BAIL]`.

#### Where the crash actually is: musl's allocator

With the wedge gone the crash reproduces in **2.0–2.3 s**, which made the next
three steps affordable in one sitting.

**First the console had to be made readable.** `idt::user_fault` printed its
report with ~20 separate `serial::puts` calls, and `serial::LOCK` is per call,
so two cores faulting at once shredded both lines into each other:

```
[Fault] #GP general protection#GP general protection in ring 3 on cpu 1 …
  [memwatch-at-kill] pmm_free=1380940 fpcache_len=138094041633 …
```

— two `pmm_free` values interleaved into one meaningless number, in the report
whose entire purpose is those numbers. It is now a single `StackWriter` flush,
the same fix and the same reason as `sched.rs`'s `[SWITCH NO-BKL]`. (It also
labels `cr2` `(stale)` on a `#GP`, which rejects its operand before translation
and leaves whatever the last page fault put there.)

**Then the rip resolved.** `INTERP_BASE` on this target is `0x3000_0000`
(`loader.rs`), so a `rip` of `0x3004_6xxx` is `ld-musl-x86_64.so.1 + 0x46xxx`.
Mount the guest image on the Ubuntu side and ask:

```
mount -o ro,loop /root/akuma-fc-rust.img /mnt/fcimg
nm -D --defined-only /mnt/fcimg/lib/ld-musl-x86_64.so.1
```

| observed rip | resolves to |
|---|---|
| `0x3004_65ec` | `aligned_alloc + 0xce` |
| `0x3004_6b96` | `aligned_alloc + 0x678` |
| `0x3004_6c93` | `aligned_alloc + 0x775` |

**Every fault is inside musl's allocator** — `aligned_alloc` is the last
exported symbol before the mallocng internals, so these are three points in the
same block of allocator code, within `0x6a7` bytes of each other. And the
clean report says what kind:

```
[Fault] #PF page fault in ring 3 on cpu 0 err=0x4 rip=0x30046c93
        cr2=0x0000000000000010 cr3=0x23840000 task=13 pid=64
  [memwatch-at-kill] pmm_free=1379502 fpcache_len=39618 cow_ref_frames=52191
```

`err=0x4` is a user-mode **read of a not-present page** and `cr2=0x10` is a
**null pointer plus a field offset** — `meta->area` is at `0x10` in mallocng's
`struct meta`. So: rustc's heap metadata contains a null where a pointer
belongs. Not memory pressure — `pmm_free` is 1.38 M pages.

That places the remaining bug squarely in **what the kernel hands a
multi-threaded process's heap**: `mmap`/`munmap`/`mremap` of the anonymous
groups mallocng allocates, the CoW fork that precedes them, or the mutual
exclusion mallocng's own lock depends on. It is no longer a scheduler question
at all.

Two sub-hypotheses eliminated on the spot:

- **Anonymous pages are zeroed.** `mm::populate_page` and `idt.rs`'s lazy arm
  both `write_bytes(.., 0, 4096)` before mapping, so "a recycled frame handed
  over with the previous owner's bytes" — the story §11's ASCII-in-`cr2` on
  bare metal suggests — is not happening on this path.
- **`%fs` was not it** (below), though looking cost a real bug.

#### Found while looking: `%fs` was restored conditionally, 642 times a boot

`hook_switch_to` restored the incoming task's TLS base only when it had one:

```rust
let fs = (*m)[to].uctx.fs_base;
if fs != 0 { crate::usermode::set_fs_base(fs); }
crate::usermode::set_user_gs_base((*m)[to].uctx.gs_base);   // unconditional
```

`IA32_FS_BASE` is one register per **core**, so skipping the write does not
leave `%fs` unset — it leaves the *outgoing* thread's TLS pointer live for
whoever runs next. The `%gs` line one row below makes exactly that argument
("a stale user value would follow a thread that never set one onto another
core"); the asymmetry reads as an oversight. The window it leaves open is real:
a freshly `execve`d main thread starts with `fs_base == 0` and does not call
`arch_prctl(ARCH_SET_FS)` until musl's `__init_tp`, so every instruction of
`ld-musl` before that ran on whatever TLS base the previous thread left on that
core — from another address space.

Now unconditional, with a counter for how often the window was open.
**642 times in one boot.** Nothing in ring 0 reads `%fs` (the kernel's per-CPU
block is `%gs`), so writing 0 for a kernel or not-yet-`arch_prctl`'d task costs
one `wrmsr` and takes a stale base away.

It is **not** the cause of the mallocng crash: ERROR at 2.1 s and 2.3 s with the
fix, ERROR at 2.2 s and 2.2 s without, and 4x1 PASS at 17.4 s throughout. Kept
on its own merits, and recorded here so the next investigation does not spend
the same afternoon on it.

#### Next, in order

> **Answered 2026-09-18 by §14.** Item 1 is solved: the corruption is a second
> demand fault on an already-populated anonymous page, which replaced it with a
> freshly zeroed frame. The `rip`s below are not a "corrupt pointer or corrupt
> `%fs`" — every one of them is a `hlt`, i.e. musl's own `assert()` firing on
> the metadata in that page. Items 2 and 3 were hypotheses about a *stall*, and
> there is no stall left to explain; they are retired unless one reappears.

1. **The heap corruption in `rustc`.** Now the headline, because the wedge that
   hid it is gone — and it reproduces in ~2 s, inside musl's allocator (above).
   The fixed run faulted **two threads of one process on two cores at once**:

   ```
   cpu 1  err=0  rip=0x300465ec  rsp=0x11d3b7b20  cr2=0x10          cr3=0x2383f000 task=16 pid=64
   cpu 0  err=0  rip=0x30046b96  rsp=0x11d809250  cr2=0x11d9e5190   cr3=0x2383f000 task=17 pid=64
   ```

   Same `cr3`, two tasks, two `rip`s 0x5aa apart in one code region, and
   `0x300465ec` is the *same* address the previous run faulted at. `err=0` on a
   `#GP` means a non-canonical operand or a null segment, not a missing page —
   so this is a corrupt pointer or a corrupt `%fs`, in a region both threads
   are executing at once. (`cr2` is stale on a `#GP` and should be ignored.)
2. **Which futex, and how old.** `futex::dump_waiters` prints the
   `(tgid, uaddr)` keys with per-waiter ages and the `wakes/empty/woken`
   tallies; it went in after the run above and the next 4x4 cell carries it.
   Two readings to separate: all nine threads on **one** key with nobody
   outside it is a userspace deadlock (possibly downstream of an earlier
   dropped wake); waiters whose `tgid` differs inside one address space, or
   `empty` climbing against a non-empty table, is the kernel losing wakes.
3. **`pthread_join` is the first suspect** for anything that still stalls.
   An untimed `FUTEX_WAIT` on musl's
   `&t->detach_state` is released only by the exiting thread's
   `clear_child_tid` write-and-wake, which on this target lives in
   `thread::teardown` — reached when a thread returns from ring 3 normally and
   **not** obviously reached when it dies by a deferred kill or a fault. A
   thread that dies without `teardown` hangs its joiner forever with no futex
   evidence at all, and that is the AArch64 bug
   `project_futex_wake_tgid_pthread_join` already fixed once, on the other
   kernel.
4. The bare metal with `pmm-forensics`, still not reached — §12's item 3.


### 14. SOLVED (2026-09-18): a second demand fault on the same anonymous page replaced it with zeros

*§13 left `rustc` dying ~2 s into every `cargo` build at `SMP>=2`, with a
`rip` in `ld-musl`'s allocator and the note that "whether those are one
corruption or two is the next question". They were one. The cause is four
lines of missing guard in this target's own demand-paging path, and the
`-j4` `cargo` build at `SMP>=2` now runs to completion where every single run
used to die in ~2 s.*

#### First: every "mysterious `#GP` in the allocator" is a `hlt`

§13 resolved three faulting `rip`s to `aligned_alloc + {0xce, 0x678, 0x775}`
and read them as three points in one block of allocator code. Disassembling
the guest's own `ld-musl-x86_64.so.1` at those offsets says something much
more specific — **all three are the instruction `f4`, `hlt`**:

```
   465de:  49 8b 48 f0     mov  -0x10(%r8),%rcx      ; rcx = base->meta
   465e2:  49 83 e8 10     sub  $0x10,%r8            ; r8  = base
   465e6:  4c 3b 41 10     cmp  0x10(%rcx),%r8       ; meta->mem == base ?
   465ea:  74 01           je   465ed
   465ec:  f4              hlt                       ; <-- the "#GP"
```

musl's `mallocng` compiles `assert()` to `a_crash()`, which on x86_64 is
`hlt` — privileged, so in ring 3 it is `#GP(0)` with no error code and no
faulting address. Every one of these reports is therefore **the allocator
catching its own corrupt in-band metadata and deliberately crashing**, not
the kernel losing a pointer. The `#PF err=0x4 cr2=0x10` variant is the same
assertion one instruction earlier, with `base->meta` reading as NULL so that
`meta->mem` at offset `0x10` faults instead.

That reframes the hunt completely: the question is not "which pointer went
wild" but "**which bytes of this process's own memory are wrong, and how did
they get that way**".

#### The matrix, re-measured — the threshold is 2x2, not 4x4

§12's matrix was taken on a kernel where the failing cell *wedged for 600 s*;
with §13's `EINTR` fix the same cell reports a `SIGSEGV` in ~2 s, which makes
the whole matrix affordable again. It is sharper than §12 could see:

| vcpu | jobs | outcome | wall |
|---|---|---|---|
| 1 | 1 | PASS | 14.3 s |
| 1 | 4 | PASS | 14.1 s |
| 4 | 1 | PASS | 17.9 s |
| **2** | **2** | **ERROR** | **2.0 s** |
| 2 | 4 | ERROR | 2.1 s |
| 4 | 2 | ERROR | 2.6 s |
| 4 | 4 | ERROR | 2.2 s |

§12's "both variables are necessary" stands and tightens: **two cores and two
concurrent jobs is already enough**, and neither alone is ever enough however
far it is pushed. Four cores running one `rustc` — which is itself
multi-threaded, 18 live user threads by the time it dies — is green.

#### Four eliminations, each with its own instrument

Cheap to state, and each cost a run:

- **The PMM's frame lifecycle.** A `--features pmm-forensics` kernel (UAF
  quarantine, premature-free check, CoW ledger) reproduced the failure
  identically and emitted **not one** `[PMM-UAF]`, `[PMM-PREMATURE]` or
  `[PMM-RESURRECT]` line. Every frame involved is correctly owned and
  correctly refcounted.
- **The shared file-page cache.** Built with
  `SHARED_FILE_PAGES_ENABLED = false`; the fault line confirms the arm
  (`fpcache_len=0 fpcache_cap=0`, `[FPCACHE] entries=0/0`). Same crash, same
  `rip`, same `task=17 pid=64`. The one mechanism that shares physical frames
  *between address spaces* is not it.
- **`mmap` handing back an address that is not free.** New: `--features
  mm-forensics` checks every placement, before anything is populated, against
  the region list (`[MM-OVERLAP]`), the page table (`[MM-LIVEPTE]`) and the
  sorted invariant the placer binary-searches (`[MM-UNSORTED]`). 5 459
  placements checked in the failing build, **zero** reports.
- **A stale peer-core translation after a CoW break.** `cowstale` passes 5/5
  at `SMP=4` with ~6 M reader checks and 0 reader faults per run — the
  2026-09-06 open issue in `AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` really was
  closed by `shootdown.rs`.

#### What decided it: the bytes, and that they were zeros and not poison

`user_fault` now dumps the ring-3 register file and, for every register that
looks like a mapped user address, 48 bytes around it — read through the page
table rather than through `uaccess`, so a dying thread cannot fault inside its
own post-mortem. The answer arrived on the first run:

```
[Fault] #PF ... err=0x4 rip=0x102c0aa26 cr2=0x0 cr3=0x2075f000 task=14 pid=64
  [regs] rax=0x11dad2030 ... rdx=0x0 ... rdi=0x11d6ea998 rbp=0x11d6ea998 r8=0x0
  [mem] rax=0x11dad2030: 0000000000000000 0002a00000000000 0000000000000000 ...
  [mem] rdi=0x11d6ea998: 0000000000000000 0000000000000000 0000000000000000 ...
```

Memory that should hold a structure reads back as **zeros**, and the program
dereferenced the null it loaded out of it. That is the
`SELFHOST_ZERO_PAGE_HUNT.md` signature and `cowstale`'s malignant case both,
so the next question is which: *a freed frame read through a stale mapping*,
or *a fresh zeroed frame put where data used to be*.

The PMM's quarantine answers it for free. A freed frame is filled with
`POISON_MAGIC ^ pa` (`0xFEEDFACE...`) and parked. Re-run under
`pmm-forensics`: the corrupt region still reads **zeros, not poison**, and the
UAF detector stays silent. So nothing was freed. The page is genuinely,
freshly zeroed — which is what this kernel does to a page it is about to hand
to ring 3 for the first time.

#### The bug

`mm::populate_page` — the anonymous demand-paging fill — allocates a frame,
zeroes it, and maps it. It never asks whether anything is already there:

```rust
let (pte, cow) = pte_prot_for(prot, frame);
if usermode::with_current_address_space(|uas| {
    uas.map_and_track_pte(va, PhysFrame::new(frame), pte, cow)
}) != Some(true)
```

and `map_and_track_pte` **overwrites a present leaf without asking**.

Two cores do take the same fault. The page is absent when each of them traps;
`idt.rs` takes the BKL for the *servicing* window, which serialises the two
handlers but not the two traps. So core A faults, is served, returns to ring 3
and writes; core B — whose fault was already delivered — is then served
against a page that has become present in between, allocates a second frame,
zeroes it, and installs it over A's. Everything A wrote is gone, the old frame
is orphaned, and because that frame is still tracked by the ledger and still
refcounted, **no PMM instrument can see it**. That is exactly why a full
forensics build ran the failure to completion without a complaint.

`fill_file_pages` has always known about this — its loop opens with
`is_current_user_range_mapped` and the comment *"A peer filled the very page
this fault is about. Present is present"*. The anonymous path never grew the
same guard, and anonymous memory is where the heap lives.

It is not this kernel's discovery either: the AArch64 side serialises demand
paging per page for precisely this reason
(`akuma-exceptions`' `fault_slot_hold`: *"prevent races when multiple
`CLONE_VM` threads fault on the same page"*). The amd64 port inherited the
region table and the fill paths and not the mutual exclusion.

#### The fix

One critical section instead of two steps. `install_filled_page` does the
presence test and the map **inside the same address-space hold**, so a peer
cannot land between them — the same serialisation, and the same reason,
as the owner's lock `cow_write_fault` takes:

```rust
fn install_filled_page(va: usize, frame: PhysFrame, pte: PteProt, cow: bool) -> PageInstall {
    usermode::with_current_address_space(|uas| {
        if uas.pte_prot(va).is_some() {
            PageInstall::Raced
        } else if uas.map_and_track_pte(va, frame, pte, cow) {
            PageInstall::Mapped
        } else {
            PageInstall::Failed
        }
    })
    .unwrap_or(PageInstall::Failed)
}
```

`Raced` is deliberately a **third** outcome and not folded into either of the
others. It is a *success* — the page is present, which is all the faulting
instruction was waiting for, so reporting failure would turn a served fault
into a `SIGSEGV` — whose frame the caller no longer owns, so reporting success
would publish a freed frame into the shared file-page cache. All five install
sites go through it: the anonymous fill, the two file fills, the eager `mmap`
fan-out, and `map_shared_file_page` (which arrives holding a cache reference
and would otherwise strand the previous mapper's).

The fill itself stays outside the hold at every caller: a file read takes the
descriptor table, and taking that underneath the address-space lock would be
the one place in the module where the two are ordered that way.

#### It fires once per build, and once was enough

`[MM] fault race #N` prints on the first occurrence and every 4096th.
A whole 4x4 `zerocopy` build produces **exactly one**:

```
[MM] fault race #1 (va=0x11d918000) — page already served by a peer
```

That is the shape of the whole investigation. One event per build, in the
mmap arena where `mallocng`'s groups and the thread stacks live, silently
replacing a live heap page with zeros — and one is enough, because the process
that owns that page is `rustc` and the bytes it lost were its allocator's.
A 1-in-a-build race presented as a 6-out-of-6 deterministic failure.

Note the counter reported by the boot suite is **0**, and will stay 0: the
boot suite is not concurrent. An instrument whose only reading is taken where
the phenomenon cannot occur is not evidence, which is why the milestone line
exists.

#### A/B/A, one binary pair, everything else equal

The B arm is the tree as it stands. The A arm is the same tree with the two
presence tests — and nothing else — disabled (`if false && uas.pte_prot(va)…`),
so the comparison is the guard and not the refactor around it.

| arm | guard | vcpu x jobs | outcome |
|---|---|---|---|
| B | on | 4 x 4 | **PASS 17.6 s** |
| A | off | 4 x 4 | ERROR 2.2 s |
| B | on | 4 x 4 | **PASS 17.4 s** |

and the full matrix on the fixed kernel, default features:

| vcpu | jobs | before | after |
|---|---|---|---|
| 1 | 4 | PASS 14.1 s | PASS 13.7 s |
| 4 | 1 | PASS 17.9 s | PASS 17.2 s |
| 2 | 2 | **ERROR 2.0 s** | **PASS 14.5 s** |
| 4 | 4 | **ERROR 2.2 s** | **PASS 17.6 s** |

#### What is kept

- `mm-forensics` (`amd64/Cargo.toml`) — the placement checks above, with
  `PLACEMENTS_CHECKED` reported on the fault line as their positive control.
  They found nothing this time and that *is* their result: the family is
  eliminated rather than untested, and the next placement bug reports itself.
- `dump_user_registers_and_memory` in `idt::user_fault` — the register file and
  48 bytes around every register that could be a user pointer. This is what
  turned "a `#GP` somewhere in the allocator" into "these bytes are zeros",
  and it cost one run.
- `mm::PAGE_FAULT_RACES` + the `[MM] fault race #N` milestone line, and
  `demand_paging_report`'s note for it.
- `mm::FILE_FILL_SHORT` + `[FILL-SHORT]`, added while looking at the residual
  below. `populate_file_page_by_inode`'s `want` is already clamped to what the
  file has from that offset, so a short read there cannot be "past EOF" — the
  bytes exist and the filesystem did not hand them over, and the page keeps its
  zeros for the shortfall. It was being discarded. This is the AArch64 kernel's
  `[FILL-SHORT]` tripwire, which `SELFHOST_ZERO_PAGE_HUNT.md` §12-§15 records as
  presenting exactly as "`rustc` cannot find something in a dependency's
  metadata". `check`ed rather than `note`d in the boot suite: unlike the race
  counter, this one needs no concurrency to happen.

#### The self-host build, and one residual

`--clean-all` on the kernel itself, `SMP=4 -j4` in the guest: **133 of 137
crates in 127 s**, where the same cell used to wedge for 600 s and then (with
§13's fix) die in 2 s. It is not yet fully green — the run ends on

```
error[E0531]: cannot find unit struct, unit variant or constant
              `PIDFD_SEND_SIGNAL` in module `nr`
```

— at the same crate, at the same line, in **both** of two repeats. That
repeatability is the useful fact: this is not the old failure wearing new
clothes. The old one was a `SIGSEGV` from a corrupted heap, at a `rip` that
moved around; this is `rustc` completing normally and reporting a name
resolution failure, identically, twice.

**A `-j1` comparison here is easy to get wrong, and this section got it wrong
once.** `cargo build -p akuma-syscalls-glue -j1` in the same guest succeeds —
but that builds glue with its *default* features, and the failing arm is behind
`#[cfg(feature = "sc-pidfd")]`, which only `akuma-amd64`'s feature set turns on.
The arm was compiled out, so the build proved nothing. The control that means
something is the same `--clean-all` build of `akuma-amd64` at **1x1**, and it is
the one to run before concluding anything about this.

Whatever it turns out to be, do not read it as "the fix did not work": the
failure it replaced was a `SIGSEGV` at 2 s in **every single run**, and this
build now gets 133 crates further.

#### Traps

- **A boot-suite reading of a concurrency counter is not a reading.** The suite
  reports `PAGE_FAULT_RACES` as 0 and always will; the race needs two cores and
  two processes and the suite has neither. The milestone console line exists
  because the number that matters can only be taken after a real build.
- **`amd64-fc-run.sh` truncates `/root/akuma-fc.log` on every boot**, and the
  matrix harness boots per cell. Grepping the log after a multi-cell run reads
  only the *last* cell — which, if that cell was `1x4`, is single-core and
  cannot contain the evidence. Measured here: a clean 4-cell run reported zero
  races and the 4x4 rerun immediately afterwards reported one.
- **`pmm-forensics` being silent is a real result and a narrow one.** It proves
  the *frame* lifecycle is sound. It says nothing about a frame that is
  correctly allocated, correctly tracked and mapped over the top of another —
  which is this bug, and why a forensics build reproduced it without comment.


### 15. GREEN (2026-09-18): the `-j4` self-host build finishes, and §14's residual was the image

*§14 ended with `cargo build -p akuma-amd64 -j4` at `SMP=4` getting 133 of 137
crates and then failing, twice identically, on `error[E0531]: cannot find
… PIDFD_SEND_SIGNAL in module nr`. It is not a code defect and it was never
going to reproduce on the laptop: `nr::PIDFD_SEND_SIGNAL` has been in the tree
since `ddda5c01` (2026-09-17). The **guest image's copy of the source** predated
it.*

`/src/akuma` inside `akuma-fc-rust.img` is a build input that drifts — §10 says
so about `sched.rs` and this is the same trap one crate further along. The
repair is §10's:

```sh
# guest down, image mounted at /mnt/fcrust
for d in crates amd64 src; do
  rsync -a --delete --exclude vendor --exclude target /root/akuma/$d/ /mnt/fcrust/src/akuma/$d/
done
cp /root/akuma/{Cargo.toml,build.rs,clippy.toml} /mnt/fcrust/src/akuma/
```

**Do not carry `Cargo.lock` across with it, and do not `cargo vendor` to make it
fit.** The image's lock and its `vendor/` are one pair (measured here: the image
wants syn 2.0.114 / quote 1.0.44 / proc-macro2 1.0.106 where the checkout's lock
wants 2.0.111 / 1.0.42 / 1.0.103), and the lock is the only file in that sync
whose partner is rig state. Syncing the sources alone leaves the pair intact;
§11 records the same trap from the other direction on the bare-metal root.

With the tree matching the kernel, the self-host gate at the cell that used to
wedge for 600 s:

| what | cell | outcome |
|---|---|---|
| `akuma-amd64`, `--clean-all`, whole dependency graph | 4 vCPU x `-j4` | **PASS, 189.6 s, rc=0** |

That is 137 crates from a cleaned `target/`, in a Firecracker guest, on four
cores, at `-j4` — the thing §10 called "what gates a fast in-guest kernel
build". `scripts/benchmarks/amd64_fc_build_matrix.py --crate akuma-amd64
--clean-all --cells 4x4` is the gate; it reboots the guest per cell, so the
console log it leaves behind is that cell's alone (§14's trap).

### 16. `^C` over `ssh` worked exactly once per boot — the console's line discipline was every session's (2026-09-18)

*Reported as "`^C` does not break `tail -f`", with the guess that it was signal
delivery or futex. Signal delivery is fine and the futex path is not involved:
`kill -INT` on a `sleep 60` parked in `nanosleep` kills it in **1.2 s**,
measured in this same guest. What was broken is upstream of any signal — the
kernel was correctly declining to raise one.*

#### The probe scored a job that never ran, and that is the first finding

`scripts/utils/amd64_ctrlc_probe.py` gained a `--job tail` mode to match the
report, following `/etc/passwd`. That file **is not in this image**, so `tail`
exited instantly with `can't open`, the shell ran the next command, and the
probe reported `KILLED after 3.3s` — a green reading of a `^C` that was never
tested. The mode now creates the file it follows and refuses to score a trial
whose job printed its post-job marker *before* the interrupt (`NO-JOB`, a third
outcome beside KILLED and SURVIVED). Same family as `mem_suite.py` refusing to
score a silent probe as a pass.

#### The tripwire that turned silence into evidence

`write_to_process_stdin` prints `[ISIG]` when it turns an INTR byte into a
`SIGINT`. A run with the bug produced **zero** of them — which says nothing on
its own, because the byte may never have arrived, and keystrokes plainly *were*
arriving (the typed command line ran). `intr_miss` is the other half, and it
costs one branch on a path that already tests for the INTR byte:

```
[ISIG-MISS] pid=59 ISIG clear lflag=0x8a30
```

`0x8a30` is `0x8a3b` with `ISIG|ICANON|ECHO` cleared — raw mode. The kernel was
right to deliver the byte as data; the terminal said the program wanted it.

#### What the terminal flags did, session by session

A temporary trace on the `TCSETS` arm (`amd64/src/fd.rs`), one line per set:

```
[TCSETS] pid=57 lflag 0x8a3b -> 0x8a30     session 1: raw, for the line editor
[TCSETS] pid=57 lflag 0x8a30 -> 0x8a3b     restored to cooked before the job
[ISIG]   pid=57 fg_pgid=57 sig=2 members=1 ^C -> SIGINT -> job dies. Correct.
[TCSETS] pid=57 lflag 0x8a3b -> 0x8a30     raw again for the next prompt
                                           ... and the session ends, raw
[TCSETS] pid=59 lflag 0x8a30 -> 0x8a30     session 2 STARTS raw
[ISIG-MISS] pid=59 ISIG clear lflag=0x8a30
```

`busybox`'s line editor puts the terminal in raw mode per prompt and restores
**the flags it read when it started**. Session 2 read raw as its baseline, so
its "restore to cooked" restored raw, and `ISIG` never came back — for that
session and every session after it, for the life of the guest. One session per
boot worked, which is exactly why this survived: the first `ssh` anyone opens
after a reboot behaves.

#### The bug: `console_attached` was a question about the fd table, and the fd table changed

`register_exec_process` decides whether a process is the serial console's by
looking at its fd 0:

```rust
let console_attached = matches!(fds.table.lock().get(&0), Some(FileDescriptor::Stdin));
```

with a note saying "a spawned child's fd 0 is a `PipeRead` from
`fd::bind_stdio`, so everything `sshd` spawns is not". That was true when it was
written. **C2 slice 6 retired the pipes** and gave a spawned child
`SharedFdTable::with_stdio()`, whose fd 0 *is* `FileDescriptor::Stdin` — so every
`sys_spawn` child started answering `true`, and took the branch below it:

```rust
terminal_state: term
    .or_else(|| console_attached.then(crate::console::terminal_state).flatten())
    .unwrap_or_else(|| Arc::new(Spinlock::new(default_terminal_state()))),
```

`sys_spawn` passes `None` for `term` and its comment promises "a spawned child
gets a **fresh** terminal state, deliberately". The `or_else` in between quietly
made it the console's **shared** one instead. Nothing failed at spawn time;
sessions simply began inheriting each other's termios.

The same flag has a second consumer, wrong in the same direction and unnoticed:
`console::set_attached_pid(pid)`, so the serial line's keystrokes were being
addressed to the newest `ssh` session's shell.

#### The fix

One clause, and it is a statement about what a console is:

```rust
let console_attached = channel.is_none()
    && matches!(fds.table.lock().get(&0), Some(FileDescriptor::Stdin));
```

A process that carries its **own** `ProcessChannel` is a session, whatever its
fd table says; only `init` on the serial line and its `fork` descendants (which
pass no channel and inherit their parent's terminal `Arc`) are the console's.
The `unwrap_or_else` below then does what `sys_spawn` always intended.

#### A/B and the boot-suite check

`session_terminal_is_private_test` (`amd64/src/usermode.rs`, run from
`boot.rs` beside `winsize_to_child_test`) dirties the **console's** state the
way a departing session leaves it, spawns a `SPAWN_FLAG_PTY` child, and reads
the child's. Written that way on purpose: `Arc::ptr_eq` would pass a kernel that
copied the console's values into a private cell, which is also wrong.

| arm | `console_attached` | boot suite | live `^C`, 5 consecutive sessions |
|---|---|---|---|
| A | fd-table only (pre-fix) | **FAIL** — "the child's ISIG is set though the console's is not" | session 1 KILLED, sessions 2-5 SURVIVED |
| B | `channel.is_none() && …` | 771 passed, 0 failed | **5 of 5 KILLED, ~3.2 s each** (`tail -f` and `sleep` alike) |

Both jobs matter: `tail -f` polls (`inotify_add_watch` is `ENOSYS` here, so
busybox falls back to read+`nanosleep`) and `sleep` is one long park, so they
fail differently if the interrupt is lost rather than never raised.

Verified once more in the state that produced the report — three trials run
immediately after a full `--clean-all -j4` build of the kernel in the same
guest, with every pid in the high hundreds: **3 of 3 KILLED, three `[ISIG]`
lines, zero `[ISIG-MISS]`.** The `-j4` gate itself is unmoved by the fix
(§15's 189.6 s, then 182.6 s with it in), and the boot suite is 771 passed.

#### What this was not, and how that was settled cheaply

- **Not signal delivery.** `kill -INT` on a `sleep 60` from a second `ssh`
  connection: dead in 1.2 s. A thread parked in `nanosleep` for 60 s is woken by
  `pend_signal_for_thread`'s `wake()` and takes the `EINTR` on the next pass —
  which is `sys_nanosleep`'s loop working as written.
- **Not the futex path.** Nothing in this reaches it; §13's `EINTR` fix stands
  on its own and is unrelated.
- **Not the shell.** `busybox` does exactly what it does on Linux; it is the
  kernel that gave two sessions one termios.

#### The report's guesses, scored

The symptom arrived as *"if `^C` does not break `tail -f` it might be something
in signal delivery or whatever"*, alongside *"check futex work too"*. Worth
scoring, because the hit rate is the useful part and two of the three were
wrong in a way that would have cost a day each to chase:

| guess | verdict | what settled it |
|---|---|---|
| **`^C` does not break `tail -f`** | **RIGHT, and it was a real defect** | reproduced on the second ssh session of every boot; `-n 3` on the probe |
| it is **signal delivery** | **wrong** | `kill -INT` on a `sleep 60` parked in `nanosleep` kills it in **1.2 s** — delivery, the wake of a parked thread, and `EINTR` all work |
| check the **futex** work | **wrong, and not involved** | nothing on this path touches the futex table; §13's `EINTR` fix stands unrelated |

The failure was one layer *above* all three: the signal was never raised at all,
because the terminal the kernel consulted said `ISIG` was off. Everything
downstream of that was working the whole time, which is why "it must be signal
delivery" is the natural reading and why the `[ISIG-MISS]` tripwire — which
distinguishes *declined* from *never arrived* — is the thing that made the
difference. **When a signal "does not arrive", measure delivery directly before
believing it: a `kill` from a second session is one command and it eliminates
the whole downstream half.**


### 17. The fixed point: the kernel the guest built builds itself, byte-identically (2026-09-18)

*§15 got `-j4` green, which proves the build **runs**. It does not prove the
build is **right** — a kernel that produces a subtly wrong binary compiles just
as happily. The cheap check for that is the one a bootstrapping compiler uses:
take the output, run it, and build again.*

Three steps, no new tooling:

```sh
# 1. out of the guest, byte for byte (md5 checked on both ends — a pty would
#    have mangled it; this ssh has no -t)
$SSH "cat /src/akuma/target/x86_64-unknown-none/release/akuma-amd64" > /root/akuma-selfbuilt-fc
# 2. boot it, 4 vCPU, same rootfs, its own FC config and log
# 3. build the kernel again inside it, -j4, from a cleaned target/
```

| generation | built by | size | md5 | boot suite |
|---|---|---|---|---|
| 1 | the box-built kernel, in-guest `-j4` | 3 389 240 B | `22c696a9…` | — |
| 2 | **generation 1**, in-guest `-j4`, 177 s | 3 389 240 B | **`22c696a9…`** | — |
| — | generation 1 booting at `vcpu_count=4` | | | **768 passed, 0 failed** |

Generation 2 is byte-identical to generation 1. That is the fixed point: the
compiler, the linker, the filesystem, the page cache and the scheduler
underneath them all produce the same 3.4 MB of output whether the kernel running
the build came from the Ubuntu side or from Akuma itself.

(768, not §16's 771: the guest's source tree predates
`session_terminal_is_private_test`, which is three checks. A suite count that
moves between a host-built and a guest-built kernel is worth reading before
celebrating — here it is accounted for.)

#### What this retires from §11

The bare-metal section added `-C link-arg=--threads=1` to the rig's rustflags
and called LLD's default thread pool "the thing this kernel cannot survive at
SMP>1". **The Firecracker guest's `.cargo/config.toml` has never carried that
flag**, so every build in §15-§17 — three clean 137-crate builds, each ending in
a real `rust-lld` link — ran the pool multi-threaded on four cores and linked
correctly. Whatever LLD did to the metal in §11, the guest does not reproduce it
on today's kernel, and §14's demand-fault race is the obvious candidate for
what it actually was.

The bare-metal arm is still **untested** on this kernel and the workaround is
still in that root's config. Removing it is a one-line change to rig state and a
23-minute build, and it now has a prior worth acting on rather than a guess.


### 18. `-j8` was `ENFILE` at a 64-pipe machine ceiling, and the ceiling's stated cost was wrong (2026-09-18)

*With `-j4` green (§15) the obvious next question is how much parallelism this
kernel will take. `-j8` failed in 8 s, three times, at a build-script link.*

What cargo says is `error: could not compile \`proc-macro2\` (build script)`,
which reads like a toolchain problem. The note two lines further in is the whole
story:

```
error: could not exec the linker `cc`
  = note: Too many open files in system (os error 23)
```

`ENFILE`, not `EMFILE` — **the machine**, not the process. And it comes out of a
*spawn*, because `std`'s `Command::spawn` makes its pipes before it `exec`s, so
a pipe ceiling presents as a linker that cannot be launched.

#### Demand or leak — the instrument that was missing

`amd64::pipe::MAX_PIPES` was 64, and its comment justified that in two ways,
both of which turn out not to hold:

- *"each pipe is up to 64 KiB of kernel buffer allocated on a userspace
  request"* — `akuma_pipe::Pipe::with_capacity` starts with an **empty
  `VecDeque`**. The capacity is a limit, not an allocation; an idle pipe costs
  its struct.
- *"a number this size means a leak announces itself instead of being
  absorbed"* — it does not. From outside, a refusal and a leak look identical,
  which is exactly the ambiguity that had to be resolved before touching the
  number. (The leak that comment was written for is real and fixed:
  `proposals/AMD64_SPAWN_PIPE_LEAK.md`, one pipe per `sys_spawn`, found with a
  temporary `pipe_live_count()` print.)

So the temporary print became permanent: `[PIPES] live=N high=N refused=N
cap=N` in the 30 s idle block, with a high-water mark and a refusal count.
A high-water that climbs across a workload that ends is a leak; one that sits
under the cap with refusals moving is demand.

| clean 137-crate build | outcome | high-water | refused | live after |
|---|---|---|---|---|
| `-j4`, cap 64 | PASS 178 s | **39** | 0 | **0** |
| `-j8`, cap 64 | **fail 8 s** | 66 | 2 | 0 |
| `-j8`, cap 256 | **PASS 163 s** | **75** | 0 | **0** |
| `-j4`, cap 256 | PASS 174 s | 41 | 0 | 0 |

Demand, unambiguously: every run drains to zero, and the peak scales with the
job count (~10 pipes per job). `MAX_PIPES` is now **256** — worst case 16 MiB
against `mem::HEAP_SIZE`'s 512 MB, and only if every pipe is simultaneously
full.

`-j8` on four vCPUs is also **the fastest cell measured** (163 s against
`-j4`'s 174 s): the jobs block on the filesystem often enough that
oversubscription pays.

#### One wrinkle worth knowing: the cap is soft by one race

The high-water at cap 64 is **66**. `at_capacity()` samples the live count
*before* `glue::pipe_create`, which has no ceiling of its own, so concurrent
creators can land a few over. That is fine — the number is a policy, not an
invariant anything indexes — but it means a check for "exactly the cap" in a
future test would be wrong.


### 19. Where the build time actually is now — the scaling table, and what it is against (2026-09-18)

Clean 137-crate `cargo build -p akuma-amd64 --target x86_64-unknown-none
--release --offline`, Firecracker guest at `vcpu_count=4`, 6144 MB, host-timed,
all on one kernel and one source tree:

| jobs | wall | vs `-j1` |
|---|---|---|
| `-j1` | 226.5 s | — |
| `-j2` | 233.6 s | **+3 % (slower)** |
| `-j4` | 174.6 / 178 / 174 / 182.6 / 189.6 s → **~178 s** | −21 % |
| `-j8` | 163.0 / 165.0 s → **~164 s** | **−28 %** |

**Eight jobs on four cores buy 1.39x.** That is the headline of this table and
it is not a good number — an ideal 4-core machine would be near 4x, and the host
gets ~2.8 cores of parallelism out of the same graph. The build is dominated by
something that does not parallelise: the BKL and the filesystem, in some
proportion this table cannot separate.

`-j1` and `-j2` are **one run each**, and their 3 % gap is inside the spread the
five `-j4` runs show (174-190 s, ±4.5 %), so read it as *"a second job returns
nothing measurable"* and not as "two jobs are slower than one". The `-j4` and
`-j8` gains are outside that spread and are real.

**So the remaining win is scaling, not per-job speed.** The per-job path has had
five rounds of work (§1-§9) and is now fast enough that four more jobs add 28 %.

#### Against what this doc previously recorded

Two prior numbers, and both comparisons need their caveat stated or they
flatter this one:

| measurement | then | now | change |
|---|---|---|---|
| in-guest clean build, **single job** (`AKUMA_SELF_HOSTING_AMD64.md`, 2026-09-13) | **473 s** / 94 crates | **226.5 s** / 137 crates | **−52 % wall on a 46 % larger graph** |
| the same, **per crate** | 5.03 s | 1.65 s | **−67 %** (3.0x) |
| best cell available, per crate | 5.03 s (`-j1` was the only one that worked) | **1.20 s** (`-j8`) | **−76 %** (4.2x) |
| in-guest clean `-j1`, SMP=1, 95 crates (§11's table) | 223 s → 2.35 s/crate | 1.65 s/crate | **−30 %** |

Caveats, in the direction that matters:

- **The graph grew.** 94 → 137 crates between those dates, so every wall-clock
  comparison above understates the per-crate improvement; the per-crate rows are
  the honest ones.
- **Today's `-j1` runs at 4 vCPU**, and §10 measured a single-job build paying
  **+26 %** for SMP=4 over SMP=1. So the like-for-like `-j1` figure against
  §11's SMP=1 number is better than the −30 % shown.
- `AKUMA_SELF_HOSTING_AMD64.md`'s status line reads the 473 s against the box's
  own Linux (69 s for the same graph, single-job) as "the guest is ~30-50x
  slower". **Its own two numbers divide to 6.9x**, not 30-50x; the larger figure
  belongs to the single-crate `akuma-exec` anchor in §"Measurements" above, not
  to the whole-graph build. Corrected here rather than left to be re-derived.

#### What is not in these numbers

`-j8` needs `MAX_PIPES` ≥ ~128 (§18) and every cell needs §14's demand-fault
race fix — before it, this table could not be taken at all: the 4x4 cell died in
2.2 s and the 4x8 cell would have died sooner.


### 20. The whole arc, earliest to latest (2026-09-13 → 2026-09-18)

Every in-guest amd64 build timing this tree has on record, in order. All of it
is the same Firecracker guest on the same box, and — checked, because it would
invalidate the comparison — `/tmp/ktarget` (the 09-13 target dir) and
`/src/akuma/target` (today's) are on the **same ext2 root disk**; the guest has
no tmpfs.

#### A. The whole kernel graph

| date | state of the kernel | crates | jobs / vCPU | wall | **s/crate** |
|---|---|---|---|---|---|
| **09-13** | first in-guest kernel build that finished at all | 94 | `-j1`, 1 vCPU | **473 s** | **5.03** |
| 09-18 (earlier) | after §1-§9: CR3-skip, futex `hlt`, PIT/TSC clock, block double-copy, O(n²) `mmap` placer | 95 | `-j1`, SMP=1 | **223 s** | **2.35** |
| 09-18 (this session) | after §14's demand-fault race fix | 137 | `-j1`, 4 vCPU | 226.5 s | 1.65 |
| 09-18 | — same kernel, more jobs (first `-j4` that ever finished) | 137 | `-j4`, 4 vCPU | 189.6 s | 1.38 |
| 09-18 | — repeats of that cell | 137 | `-j4`, 4 vCPU | 174.6 s | 1.27 |
| 09-18 | after §18's `MAX_PIPES` 64 → 256 | 137 | **`-j8`, 4 vCPU** | **163.0 s** | **1.19** |
| 09-18 | §17's generation 2 — built *inside* the self-built kernel | 137 | `-j4`, 4 vCPU | 177 s | 1.29 |

**5.03 → 1.19 s/crate: −76 %, a 4.2x speedup, in five days.** Read the last
three rows as one kernel measured at three job counts, not as three
improvements; the kernel changed at the rows that name a fix.

The wall-clock column is the one to quote carefully — the graph went 94 → 137
crates (+46 %) over the same five days, so 473 s → 163 s understates it at
−65 % where the per-crate figure says −76 %.

#### B. The single-crate anchors that got there

The whole-graph number moved because two much cheaper measurements did:

| anchor | before | after | step |
|---|---|---|---|
| `akuma-exec` rebuild, `-j1` | 102 s | 83.5 s | CR3-skip on same-root switches (−18 %) |
| " | 83.5 s | **77.9 s** | untimed futex waits stop `hlt`-ing first (−7 %) |
| `zerocopy`, `-j1` | *never finished in 15+ min* | 63 s | §5: `find_free_va` was O(n²) in region count |
| " | 63 s | 18.3 s | §8: it sorted 4 400 regions on **every** `mmap` — 74 % of `rustc` |
| " | 18.3 s | **15.1 s** | §9: binary-searched lookups + placement cursor |
| " | 15.1 s | **13.24 s** | §10, SMP=1 |

`zerocopy` against the same `rustc` on the box's Linux: **8.8 s**, so that gap
went **7.2x → 1.5x**.

#### C. Against Linux on the same physical box

| | s/crate | guest / host |
|---|---|---|
| box's own Linux, 94 crates, `-j1` | 0.73 | 1.0x |
| Akuma guest, 09-13, `-j1` | 5.03 | **6.9x** |
| Akuma guest, 09-18, `-j1` | 1.65 | **2.3x** |
| Akuma guest, 09-18, `-j8` | 1.19 | **1.6x** |

(The last row is not like-for-like — it is Akuma at 8 jobs against Linux at 1 —
and it is kept because it is the number that matters in practice: *how long do I
wait for a kernel*. The honest kernel-to-kernel comparison is the `-j1` row.)

#### D. Bare metal, for contrast — still the outlier

| | crates | wall | s/crate |
|---|---|---|---|
| Firecracker guest, `-j1`, image on the internal SSD (09-18) | 95 | 223 s | 2.35 |
| bare metal, `nosmp`, root on the USB disk (§11, 09-18) | 95 | **1 374 s** | **14.5** |
| bare metal, **SMP=4** `-j1`, 512 MiB heap / 128 MB cache (09-18, gen-1) | 95 | 1 398 s | 14.7 |
| bare metal, **SMP=4** `-j1`, **1 GiB heap / 256 MB cache** (09-18, gen-2) | 95 | **640 s** | **6.7** |

The last two rows are the same machine, same source, same cell — **only the
kernel heap differs**, and with it the ext2 block cache (`HEAP_SIZE/4` was the
binding term of `min(RAM/8, FSCACHE_CEILING_MB, HEAP_SIZE/4)` on a 16 GiB box).
**2.2x**, and the build that had never once reached a successful link, linked.
Note also that SMP=4 `-j1` (14.7) matches `nosmp` (14.5) almost exactly: four
cores buy a single-job build nothing here, which is the same finding as §10's
"+26% for SMP" seen from the other side.

`AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 has the crossover table and the reason
this is the first lever to reach for on any new machine: the heap is the only
term in that `min` that does **not** scale with RAM, so on a big box it binds
silently, and only on the workload that notices.

**6.2x, on the same physical machine.** Untested since §14 — every number in A,
B and C is the guest.

> **Corrected 2026-09-18 by `AKUMA_AMD64_BARE_METAL_SELFHOST.md` §2.** This
> paragraph read "it is storage rather than the kernel", and that attribution is
> wrong as stated. Measured directly with `dd` and `O_DIRECT`, the USB root is
> **1.4–1.5x** the internal disk sequentially and **2.3x** on 4 KiB random — so
> the *device* accounts for at most 2.3x of the 6.2x. The rest is **cache
> residency**, which is the same mechanism named below but is a tunable rather
> than a hardware limit: Ubuntu's page cache fronts the guest's root image with
> gigabytes, while the metal had only Akuma's own ext2 block cache — pinned at
> **128 MB** by the `HEAP_SIZE/4` term of
> `min(RAM/8, FSCACHE_CEILING_MB, HEAP_SIZE/4)` on a 16 GiB machine. Do not
> optimise the disk on the strength of the 6.2x; the headroom is in the cache.

#### What each era was actually limited by

1. **09-13 → 09-17: per-syscall and per-fault cost.** TLB flushes, a 10 ms
   clock, a double copy per block, and an `mmap` placer that was quadratic.
2. **09-17 → 09-18 morning: correctness at SMP>1.** Not speed at all — every
   parallel build died or wedged, so `-j1` was the only cell that existed.
3. **09-18: resource ceilings.** A 64-pipe machine limit that presented as a
   broken linker.
4. **09-18, in the guest: scaling.** 8 jobs on 4 cores buy 1.39x (§19). The
   per-job path is done; what remains is whatever serialises — the BKL and the
   filesystem.
5. **09-18, on the metal: a cache starved by a constant.** Every number above is
   the guest, and the metal turned out to be limited by something the guest
   never was — a hard-coded 512 MiB `HEAP_SIZE` capping the ext2 block cache at
   128 MB on a 16 GiB machine. Fixing that was 2.2x on the whole build and made
   the link stop failing. Era 4's "the per-job path is done" was true *of the
   guest*; it was not true of the metal, and the difference was one constant.
   [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md)


## Background

- `docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md` — the AArch64 original of §6's leak.
- `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` — where §7's 384 MB ceiling comes from.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the self-host bring-up stages.
- `docs/runbooks/selfhost-kernel-build.md` — aarch64 self-host procedure,
  detach/poll mechanics, and the aarch64 baseline numbers (44 s clean build).
- `docs/archive/COW_PILE_AUDIT.md` §10 — the freed-L0 hazard this fix leans
  on the liveness gate for.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — procfs on amd64.
