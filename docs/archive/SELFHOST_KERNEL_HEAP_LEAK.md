# Repeated in-guest clean builds leak kernel heap to the OOM wall

**Status: ROOT-CAUSED and FIXED 2026-09-03.** The fix is the `drain_retired`
terminal gate plus `CACHE_CHUNK_BYTES` 1 MB -> 64 KB; abandoned drains went
**77 -> 0**, orphaned `user_frames` entries **1 466 360 -> 0**, and `stuck=(0,0)`
after the reclaim gate. **Independently re-confirmed the same day** by two
8-build campaigns that were measuring something else entirely — see
"2026-09-03 (third session) — independent confirmation" at the end. Read on for
the root cause; the theory list T1-T5 is kept because the reasoning that killed
each one is still the map of this subsystem.
`drain_pending_ttbr_frees` (`crates/akuma-mmu/src/lib.rs`) `swap_remove`s
`PendingAsFree` entries out of the global `PENDING_TTBR_FREES` list into a local
`Vec` and only *then* frees them in a loop. It runs on the deferred-reclaim
path, which `process/reclaim.rs` documents as running "on an already-terminated
thread" that "can be permanently preempted mid-sweep" — and this kernel does not
unwind. When that happens the entries it is carrying are orphaned: already gone
from the global list, still holding whole `UserAddressSpace::user_frames`
`BTreeMap`s, unreachable forever. Measured **82 abandoned drains per clean
build, holding 1 466 360 `user_frames` entries** — which is exactly the leaked
`BTreeMap` node count. Read the last two sections for the evidence chain and the
fix options. Found 2026-09-01 while running a Tier 5
self-host campaign (`docs/runbooks/verify-trim-fat-change.md`) to clear the
`akuma-exceptions` extraction. It is **pre-existing** — A/B'd against the
pre-extraction commit on the same image, same script, same day, and both arms
behave identically. This doc records what was measured, what that measurement
already rules out, and five theories each paired with a test that can kill it.

Nothing here is fixed. The one thing to take away operationally: the runbook's
**"10/10 clean builds" baseline no longer reproduces; 8/10 is the current
number**, and a campaign that dies at trial 9 or 10 is this, not your change.

## Symptom

Ten sequential in-guest `cargo clean && cargo build --release -p akuma -j4
--offline` trials on devbox-smoltcp, `MEMORY=4096 SMP=4`. The kernel heap climbs
**monotonically** across trials and never falls — not between trials, not while
the guest is idle. `[HEAP] NMB used` prints on each 5 MB crossing and walks
13 → 758 MB. Around trial 9:

```
[OOM] allocation of N bytes failed (heap ~755MB / ~796MB used) — killing process
```

rustc takes the SIGKILL (`cargo` reports `exit status: 137`), and on the next
trial the VM stops answering ssh while QEMU stays alive.

## Measured

| arm | commit | clean builds | heap at trial 9 |
|---|---|---|---|
| post-extraction | `f9808bd6` | **8/10** | 758 MB |
| pre-extraction  | `56a8635f` | **8/10** | 733 MB |

Same image, sequential, same 10-trial script. Both die VM-UNREACHABLE at trial
10 with the same trajectory. Every corruption tripwire is **silent on both
arms** — `PMM-RESURRECT`, `PMM-UAF`, `PMM-POISON`, `WILD-DA`, `FILL-SHORT`,
`PANIC`, `[BKL] stuck` all zero. So this is a leak, not corruption, and it is
not the `akuma-exceptions` extraction.

## What the number already rules out

Three deductions come free from reading the allocator, and they are worth
stating because each closes off a whole class of explanation.

**1. It is not arena fragmentation.** `[HEAP] NMB used` reports
`ALLOCATED_BYTES` (`crates/akuma-alloc/src/lib.rs`), which is incremented by
`layout.size()` in `talc_alloc` and decremented by the same in `talc_dealloc`.
It is a **live user-bytes** counter, not the size of the Talc arena. A monotone
climb therefore means objects that are still allocated — not free space stranded
between them.

**2. It is not the counter lying.** The `[OOM]` line prints two *independent*
numbers: `stats.allocated` (the counter above) and `stats.heap_size` (`HEAP_SIZE`,
bumped only in `PmmOomHandler::handle_oom` when it claims pages from the PMM).
They agree — 755 MB live inside a 796 MB arena. The arena really did grow to
796 MB of PMM-backed pages, so the memory is genuinely gone, whatever the
counter says. Registry and canaries are compiled out
(`ENABLE_ALLOCATION_REGISTRY = false`), so the accounting path is a plain
add/sub with no third code path to get wrong.

**3. It is a kernel-heap leak, not a user-page leak.** The PMM tripwires are
silent and there is no per-process page drift. That is the opposite signature
from `EXECVE_STACK_LEAK_OOM_HANG.md`, which reached the same end state
(`[OOM]` → unresponsive VM) by leaking ~1.1 MB of *user* pages per exec. Do not
re-run that fix's diagnostics here; they will all come back clean, as they did.

## Scale to explain

758 MB over 9 trials is roughly **80 MB per trial**. A `-p akuma -j4` build is
~147 compilation units, so on the order of 400-500 processes and a few thousand
pipes and fds per trial. Any theory has to produce ~80 MB out of *that* volume,
which is a useful filter: it wants either **~1300 objects of 64 KB**, or
**~40 000 objects of 2 KB**, or **~40 objects of 2 MB**, per trial. Several
candidates below are real unbounded growth that nonetheless cannot reach 80 MB,
and saying so is the point — they should be fixed, but fixing them will not move
the campaign from 8/10.

## Theories

### T1 — pid-keyed tables are unbounded by construction, and `SYSCALL_LOG` is one

**Confirmed unbounded; too small to be dominant.**

`allocate_pid()` is `NEXT_PID.fetch_add(1)` — pids are **never recycled** in this
kernel. So any global table keyed by pid grows for the life of the boot unless
something explicitly prunes it.

`src/syscall/log.rs` holds `SYSCALL_LOG: BTreeMap<u32, ProcessSyscallLog>`, one
entry per pid, each a `VecDeque` of up to `PROC_SYSCALL_LOG_MAX_ENTRIES` (64)
32-byte records ≈ 2 KB. `PROC_SYSCALL_LOG_ENABLED` is `true` on every profile
except `extreme-size`. Entries are retained `PROC_SYSCALL_LOG_RETAIN_MS` (10 s)
past exit — but **the only code that ever removes one is the `log.retain(…)`
inside `get_formatted`**, which runs only when somebody reads
`/proc/<pid>/syscalls`. `mark_exited` just stamps a timestamp;
`list_pids_with_logs` filters the expired without deleting them. During a build
campaign nothing reads that file, so nothing is ever pruned.

Arithmetic: ~500 pids/trial × ~2.2 KB ≈ **1 MB/trial**. Real, free to fix
(prune from a maintenance pass or from `mark_exited`), and ~1% of the leak.

**Test:** count distinct pids in a trial and multiply; or add the map's `len()`
to an existing heartbeat line.

### T2 — leaked pipe endpoint refcounts (best fit for the size)

`KernelPipe` (`src/syscall/pipe.rs`) buffers up to `PIPE_CAPACITY` = **64 KiB**,
and its `PIPES` entry is removed only when `write_count == 0 && read_count == 0`.
A single leaked endpoint reference therefore pins up to 64 KB of heap forever,
and `80 MB / 64 KB ≈ 1300 pipes/trial` — squarely in range for a `-j4` cargo
build, where every process pair gets several.

This codebase has already shipped **exactly this bug once**: an abandoned
`close_all` leaked a pipe write ref, which is what made rustc's `read()` never
see EOF in the `-j4` hang (`ktg_stale_tid_exit_stamp`, and `fd.rs`'s own
`close_all` doc comment calls the class out by name). The teardown paths that
have to get the refcount right — `sys_close`, `SharedFdTable::close_all`, the
SIGKILL/`exit_group` route, and fork/`CLONE_FILES` sharing — are the same ones
that were wrong before.

**Test — this one is cheap and decisive.** `pipe_dump()` already exists and
prints `[PIPE-DUMP] N live` plus per-pipe `bytes=/readers=/writers=`. Trigger it
at idle after each of three trials. If `N` climbs by ~1300 per trial, T2 is the
answer and the per-pipe `writers=`/`readers=` columns name the leaking side.

### T3 — RETIRED process slots holding fat `Process` structs

`crates/akuma-exec/src/process/table.rs` retires a slot (`ACTIVE → RETIRED`) and
frees the `Process` only later, in `reclaim_retired_processes`, behind a
cooldown and a `request_retired_reclaim()` nudge. A rustc `Process` is not
small — its mmap region list and per-page frame bookkeeping run to megabytes.
If reclaim lags under `-j4` load, up to `MAX_PROCESSES - 1` = 255 of them sit
live at once.

**Why it is probably not the whole story:** this is *bounded*, so it predicts a
plateau at ~255 × per-process cost, and it predicts the heap **falls when the
build ends**. The observation is a monotone climb that never falls, including at
idle. T3 can be a large constant term but cannot by itself be the ramp.

**Test:** log `retired_process_count()` alongside `[HEAP]` at idle between
trials. Heap high with the retired count at 0 rules T3 out entirely.

### T4 — mapping bookkeeping outliving the mapping it describes

The signature "heap grows, PMM tripwires all silent, no page drift" is exactly
what you get when the **heap-side record** of a mapping survives while the
physical frame it names is correctly returned. `MmapRegion`'s per-page frame
vector is the candidate: a 500 MB lazily-faulted rustc heap is ~128 K page
entries, ~2 MB of `Vec` per process at 16 B an entry. 40 such processes per
trial is 80 MB.

Adjacent, same shape, same test: `SHARED_FILE_MAPPINGS` in `src/syscall/mem.rs`
is keyed `(tgid, base)` — tgid, so unrecycled per T1 — and its `remove` sits on
the munmap/msync path. A process that dies by SIGKILL without unmapping needs
some other route to clear it.

**Test:** sample PMM `free_count()` and `allocated_bytes()` together at idle
between trials. PMM flat while the heap climbs is T4's fingerprint and
distinguishes it from every user-page theory in one reading.

### T5 — many small survivors rather than a few big ones

The three size regimes above (64 KB / 2 KB / 2 MB) are mutually exclusive
predictions, and one already-shipped instrument separates them without any new
code: `[HEAP-GROW] total={}MB used={}MB this_req={} bytes claimed={} pages`
fires at every 256 MB arena crossing and **prints the allocation size that drove
the growth**. A campaign that reaches 758 MB emits three of these. `this_req`
≈ 65536 points at T2; ≈ 2 MB points at T4; a few hundred bytes points at a
per-object table like T1 and means the object count, not the object size, is the
problem.

The `[OOM]` path also calls `syscall_counters::dump()`, so the boot log of any
campaign that died already carries a full syscall histogram taken at the moment
of death — read which family ran hot before instrumenting anything.

## Where to start next time

In order, cheapest first, all against a log you may already have:

1. `grep -a 'HEAP-GROW' <boot.log>` — three lines, and `this_req` picks the
   size regime. Do this before writing any new instrumentation.
2. `grep -a 'PIPE-DUMP' <boot.log>`, or trigger `pipe_dump()` at idle between
   trials — kills or confirms T2 outright.
3. Sample `akuma_pmm::free_count()` and `akuma_alloc::allocated_bytes()`
   together at idle between trials — separates T4 from everything user-page.
4. If none of the above resolves it, add a **live-bytes histogram by log2 size
   class** to `talc_alloc`/`talc_dealloc`: 32 `AtomicUsize` counters, O(1), no
   allocation, dumped from the existing heartbeat. The 4096-entry
   `ALLOCATION_REGISTRY` is not the tool for this — it is O(n) per allocation
   and cannot track hundreds of thousands of live objects.

## 2026-09-03 — root cause narrowed: a per-build small-object leak, not T1–T5

Step 4 of the list above was implemented (temporary, in-tree at the time of
writing): `akuma-alloc` now keeps live-bytes/live-count histograms per log2
size class plus an exact-size table for the hot class, dumped as
`[LIVEHIST]`/`[LIVE8]` from the `[FSCACHE]` 30 s console cadence
(`akuma_alloc::dump_live_histogram()`). One controlled run on
devbox-smoltcp (`MEMORY=4096 SMP=4`, instrumented kernel, guest tree
`25c817b8`) settled most of this doc's open questions.

### Controlled isolation (all deltas are `[HEAP]` live bytes)

| workload | live-heap delta |
|---|---|
| fresh boot, idle 100 s, no connection | none — live ~3.5 MB, arena 256 MB |
| 5 × ssh connect + one trivial command, disconnect | none |
| **one clean `cargo build -p akuma -j4 --offline` (89–98 s)** | **<13 MB → 462–477 MB, and it does not drain at idle afterwards** |
| one standalone `rustc -O hello.rs` | +11 objects in the leaking class |
| sshd accept-loop running throughout (≈800 accept+wait4/s, continuous) | exonerated: idle intervals with 24 000+ accepts and **zero** growth |

### Composition of the post-build heap

- **384 MB is the ext2 block cache by design** — `fs-cache` chunks are 1 MiB
  kernel-heap allocations that never shrink for the boot (`akuma-vfs-glue/
  src/fs.rs` sizes it `min(RAM/8, 384 MB)`; slots 98304/98304 after any
  build). This is the floor of every `[HEAP]`/meminfo reading after a build,
  not a leak.
- **The leak is ~56–93 MB per clean build of small objects**: the 2^8 class
  (129–256 B) went from 279 objects at boot to 281 053–415 060 after one
  build. Exact-size breakdown at settle: **144 B × ~240 000**, **240 B ×
  ~38 000–51 000**, 224 B × ~2 900. One earlier run additionally held ~325 k
  objects of exactly 256 B — run-to-run variance, unexplained; console volume
  perturbing build timing is the leading suspect.
- The 2^9 class (~21 k objects, ~8 MB) is consistent with fpcache's
  `BTreeMap` nodes at its measured ~137 k entries — bounded by the cache, not
  the leak.
- **Churn-proportional, not per-process**: a full build leaks ≈ 1 900
  objects per compile unit but one standalone rustc leaks 11. Whatever the
  object is, the build's *aggregate* syscall churn creates it, and nothing
  ever frees it.

### What this does to the T-list

- **T1 (`SYSCALL_LOG`) confirmed real and confirmed negligible** — still
  unpruned (the only `retain` runs inside `get_formatted`), ~1 MB/trial.
  Fix and forget; it is not the ramp.
- **T2 (pipe endpoint refs) not implicated in this workload** — no pipe-heavy
  traffic ran, and the leak reproduced without one. The class stays a hazard
  (`sys_pipe2` still leaks both endpoints on the `write_user_val` EFAULT
  path, `pipe.rs`).
- **T3 (RETIRED Process parking) disproven as the ramp** — idle/terminal
  drain sites run (`retired=0/0p` in PSTATS at idle) and the histogram shows
  hundreds of thousands of small objects, not fat `Process` structs.
- **T4 (mapping bookkeeping) likewise** — region frame-vecs are ~2 MB
  objects; the histogram shows no 2^21 accumulation. The "PMM flat while
  heap climbs" test was never needed: the histogram attributes directly.
- **The leak is a new T**: ~281 k objects of 129–256 B per clean build,
  created by build churn, never reclaimed, permanent for the boot.

### Exonerations worth recording

- **The userspace sshd's accept poll-loop is innocent.** It runs accept →
  wait4 → nanosleep ~800×/s from boot to shutdown (512 303 accepts in 630 s
  in an unmodified run), which makes *any* wall-clock-correlated measurement
  look leak-like. But idle intervals with the loop running flat show zero
  heap growth. Earlier readings that correlated heap growth with sshd
  activity were correlating with the *session's process churn*, not the loop.
- **The ASID question is closed.** `grep -ac 'asid. EXHAUSTED'` = 0 in every
  run, including the 10-trial campaign. `UserAddressSpace::drop` runs, the
  deferred-reclaim machinery works, and `docs/archive/
  ASID_EXHAUSTION_TIGHT_THREAD_LOOP.md` remains what it says: a rate problem
  with a working return path, unrelated to this leak.

### `free`/meminfo decode (so the next person doesn't re-derive it)

`render_meminfo` reports PMM page counts plus `Cached = fpcache::len()`;
busybox derives `used ≡ total − free − buff/cache` and the arithmetic closes
to the kB. Consequences: **`used` contains the block cache (384 MB after a
build), the heap *arena* (not live bytes), and live user pages** — `[HEAP]`
is live, `FSCACHE heap_mb` is `stats.heap_size` = arena. Fresh-boot floor on
a 4 GiB box: arena 256 MB plus ~390 MB of other PMM consumers before anything
runs. A `free` showing "1.2G used" after one build on a fresh boot is
therefore mostly block cache + arena + one build's leak — the 2026-09-02
"fresh evidence" reading that started this hunt.

### Instrumentation status and the next step

`[LIVEHIST]`/`[LIVE8]` work. `[LIVENR]` (per-syscall attribution) **does
not** — the only hookable source was the *global* `CURRENT_SYSCALL_NR`, which
races under SMP and underflows its u32 per-nr counters; its output is noise.
To name the struct, register the per-thread syscall number instead (the slot
table already tracks it — the `[THR-DUMP]` line prints `sc=`), rerun one
build, and read `[LIVENR]`. Alternatively tag the two alloc sites that
produce 144/240 B. Note also: one run's build died mid-compile (rustc exited,
no `[OOM]`, PMM 2.5 GB free) — unexplained, watch for recurrence before
calling it the leak's downstream effect.

## Operational note

Until this is fixed, cap Tier 5 at ~8 trials per boot or reboot between batches;
the heap does not recover within a boot. Watch
`grep -aoE '\[HEAP\] [0-9]+MB used' <boot.log> | tail -1` as the campaign's fuel
gauge. A trial failing with `exit status: 137` on a rustc invocation is the OOM
killer, **not a miscompile** — check the heap line before chasing it as a codegen
bug. Run `e2fsck -fy devbox.img` between campaigns
(`/opt/homebrew/opt/e2fsprogs/sbin/e2fsck`; there is no `e2fsck` on PATH):
hard-killed campaigns really do damage the image, and the resulting 15-minute
boot behind a watchdog storm impersonates a kernel regression.

## Background

- `docs/runbooks/verify-trim-fat-change.md` § Tier 5 — the campaign this came
  out of, and the doc whose 10/10 baseline this corrects.
- `docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md` — the *other* leak that ends in
  `[OOM]` + unresponsive VM. That one was user pages and is fixed; the PMM
  tripwires tell them apart.
- `docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md` — deferred process
  reclaim, T3's mechanism, and the `PIPE_CAPACITY` cap that bounds T2's
  per-object cost at 64 KB.
- `docs/archive/HEAP_AND_MEMORY_IMPROVEMENTS.md`, `docs/reference/subsystems/memory.md`
  — heap arena growth and `reclaim_to_pmm()`, which can only return a span once
  it is *fully* free (at 755/796 MB live, none are).
- `docs/archive/SELFHOST_DEVBOX_SMOLTCP.md` — the `-j4` jam that `pipe_dump()`
  was written for, and the previous confirmed pipe-refcount leak.
- `docs/archive/ASID_EXHAUSTION_TIGHT_THREAD_LOOP.md` — the suspicion that
  this leak and ASID exhaustion shared a broken reclamation path was raised
  and closed 2026-09-03: the ASID canary (`[asid] EXHAUSTED` = 0 through a
  full campaign) proves the drops run; the two issues are unrelated.
- `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` — where the block cache's cap
  was measured; the 384 MB permanent kernel-heap floor every post-build
  reading sits on.

## 2026-09-03 (second session) — the struct is named: `user_frames`

The previous section left one question: *which* 129-256 B object. It is
`UserAddressSpace::user_frames` — `BTreeMap<usize, u32>`, one entry per
physical page an address space has resident, keyed by PA and counting VAs
(`crates/akuma-mmu/src/lib.rs`).

### How it was identified

Three independent steps, each cheap, none of them a guess:

1. **Size → type, on the host.** The leaked sizes are **144 B** and **240 B**,
   and they differ by exactly 96 = 12 × 8 — the edge-pointer array that
   separates a Rust `BTreeMap` `InternalNode` from its `LeafNode`. A host probe
   (a counting `GlobalAlloc` over candidate map types) settles which `(K, V)`
   produces that pair: `BTreeMap<usize, u32>` allocates **144/240**. The same
   probe explains the rest of the histogram and so validates itself —
   `BTreeMap<usize, u16>` is **128/224**, which is PMM `COW_REFCOUNTS` (2^7:
   19 756 objects ≈ 2 466 KB, and 224 B × 2 984, both matching to the byte), and
   fpcache's `BTreeMap<(u32,u32,usize), Entry>` is **368/464**, which is the
   2^9 class. Do not re-derive these by hand: hand-computing the `repr(C)`
   layout gives 152 for `BTreeMap<usize,u32>` and is wrong.
2. **Live-per-callsite attribution in the allocator.** Cumulative alloc counts
   cannot find this leak — 86 % of 144 B allocations are freed normally, so the
   leaking site and the churning site are the same site. What works is *live*
   count per call chain: capture the x29 frame chain at allocation, intern it,
   and keep a direct-mapped `ptr -> chain slot` side table so the free
   decrements the right one. Built with `-C force-frame-pointers=yes`
   (`aarch64-unknown-none` omits x29 chains otherwise) and symbolized with
   `llvm-nm` against the kernel ELF — `llvm-symbolizer` returns `??` because
   the release profile carries no DWARF.
3. **Address-space lifecycle counters** in `akuma-mmu` to say whether the maps
   are stranded because address spaces leak, or because entries never leave
   maps that do drop.

### The two allocating chains

```
[live 61 231]  BTreeMap<usize,u32>::insert_recursing
            <- UserAddressSpace::adopt_user_frame
            <- akuma_exceptions::demand_page_lazy_region
            <- rust_sync_el0_handler_inner <- sync_el0_handler

[live 61 225]  BTreeMap<usize,u32>::insert_recursing
            <- UserAddressSpace::track_user_frame
            <- akuma_exec::process::cow_share_and_demote_range
            <- akuma_exec::process::fork_process
            <- sys_clone_pidfd <- handle_syscall
```

Together with their sibling chains (same two functions, different inlined call
sites) these account for ~249 000 of the ~261 000 live 144 B objects. The
remaining traffic is the ext2 `ClockBlockCache::occupy` index — which is *also*
a `BTreeMap<u32, usize>` at 144/240 — but it is bounded and holds only ~6 000
live nodes, exactly as its 98 304-slot cap predicts.

### What this rules out

| candidate | verdict | evidence |
|---|---|---|
| ext2 block-cache `index` | not it | ~6 000 live nodes; only 146 656 inserts in a build vs a leak 13× larger |
| fpcache | not it | different node size (368/464), bounded by the cache |
| `COW_REFCOUNTS` | not it | 128/224, and its size tracks real CoW pages |
| `VFORK_WAITERS` | not it | right node size, wrong magnitude by ~3 orders |
| `PENDING_TTBR_FREES` | not it | `[ASPARK]` flat **0** through a whole build — the first time this list has ever been printed |
| `SHARED_L0_TABLE` deferred maps | not it | `[L0PARK]` flat **0** |
| address spaces leaking | **no** | `new=426 / drop=424`, `new_shared=1113 / drop_shared=1113` — every address space is dropped |
| ASIDs leaking | no | `[asid] EXHAUSTED` = 0; with only 256 ASIDs, a missed `Drop` would exhaust them fast |
| `BTreeMap` alloc/free asymmetry | no | host-tested: build-then-drop, drain-then-drop and `mem::take`-then-drop all return to zero residual. A drained but **still live** map retains exactly one 144 B root node |

### Where the entries actually are

The `[UFFLOW]` counter splits the residual into its terms. Mid-build:

```
[UFFLOW] inserts=12322868 removed=4573090 freed_at_teardown=7418964 dropped_with_struct=0 silent=0
[ASLIFE] new=411 new_shared=905 drop=406 drop_shared=900 live_uf_entries=330814
```

`dropped_with_struct=0` and `silent=0` are the load-bearing numbers: at every
`UserAddressSpace::drop` the map had already been emptied by the owner branch's
`mem::take`, and no map ever died down a silent path. So the residual is not
leaked *maps* — it is entries in maps that are **still alive**, and at
post-build idle only **two** address spaces are alive at all
(`new=426 / drop=424`) holding ~295 000 entries between them.

### The maps are orphaned, not held

`[UFBIG]` closes it. It sweeps every active process on the same 30 s cadence
and prints any whose `resident_pages()` exceeds 5 000. Mid-build it names the
rustc processes exactly as expected (`pid=1022 uf_entries=104435`,
`pid=1078 uf_entries=102929`, …). In the final two dump cycles after the build
settles it prints **nothing at all**, while the same cycle reports:

```
[ASLIFE] new=426 new_shared=1113 drop=424 drop_shared=1113 live_uf_entries=470160
[UFFLOW] inserts=13858644 removed=6131634 freed_at_teardown=7256850 dropped_with_struct=0 silent=0
[ASPARK] pending=0 parked_user_frames=0 parked_pt_frames=0
[L0PARK] entries=0 deferred_user_pages=0 deferred_pt_frames=0
[LIVE8] size=144: 288409 objs
```

So 470 160 entries across 288 409 leaf nodes belong to **no live address
space, no park list, and no live process**. They are orphaned maps: reachable
from nothing, never dropped, permanent for the boot.

Note what `freed_at_teardown=7256850` means in that light. `free_as_frames_now`
subtracts `user_frames.len()` **on entry** and then frees the pages one at a
time. A map can therefore be fully "accounted" by the flow counters and still
have its `BTreeMap` nodes leak, if control never reaches the end of the
function that owns the local.

### The escape route, confirmed

Two brackets settle it. `UserAddressSpace::drop` was given an
enter/exit counter pair plus a fixed-size **in-flight ledger** (claim a slot on
entry, release on exit; anything still held at dump time was abandoned), and
`free_as_frames_now` was given the same treatment, tagged by which of its two
call sites reached it.

```
[ASDROP]  drop_exit=1537 free_now_enter=399 free_now_exit=317
[ASLIFE]  new=426 new_shared=1113 drop=424 drop_shared=1113
[ASSTUCK] in_flight=82 holding_uf_entries=1466360 ledger_overflow=0
[ASSTUCK] slot=2  kind=free_now/from_drain tid=18 l0=0x7190f000 uf_entries=40886
[ASSTUCK] slot=7  kind=free_now/from_drain tid=40 l0=0x73444000 uf_entries=42193
[ASSTUCK] slot=9  kind=free_now/from_drain tid=25 l0=0x76b2e000 uf_entries=11872
...
```

Read it in order:

- `drop_exit = 1537 = drop + drop_shared`. **`UserAddressSpace::drop` always
  completes.** No ledger slot is ever held by an `as_drop` bracket.
- `free_now_enter - free_now_exit = 82`. `free_as_frames_now` has no early
  return, so 82 invocations were entered and abandoned. The gap grows
  monotonically through the build and **does not recover at idle**.
- Every stuck slot is tagged **`free_now/from_drain`** — not one is
  `from_drop`. The abandoned calls all come from `drain_pending_ttbr_frees`.
- The 82 of them hold **1 466 360** `user_frames` entries. At ~6 entries per
  `BTreeMap` leaf that is ~245 000 leaf nodes, against 264 734 live 144 B
  objects measured in the same dump cycle. The arithmetic closes.

### Why every earlier signal looked clean

`drain_pending_ttbr_frees` takes its work list like this:

```rust
let ready: Vec<PendingAsFree> = with_irqs_disabled(|| { ...swap_remove... });
for e in ready {
    free_as_frames_now(e.l0_frame, &e.user_frames, &e.pt_frames);
}
```

The entries leave the global list **before** anything is freed, and
`free_as_frames_now` subtracts the map's length from the live-entry counter on
**entry**, before its page loop. So an abandoned drain is invisible to every
instrument that was reached for first:

| signal | why it read clean |
|---|---|
| `[ASPARK] pending=0` | the entry was `swap_remove`d out of `PENDING_TTBR_FREES` before it was lost |
| `[L0PARK] entries=0` | a different table entirely |
| `new=426 / drop=424` | the address space really was dropped; the map outlived it inside `PendingAsFree` |
| `[UFBIG]` silent | the map is reachable from no live process |
| `freed_at_teardown` large | the length is subtracted at entry to the loop, not on completion |
| `dropped_with_struct=0`, `silent=0` | the map never took either of those routes |
| PMM tripwires silent | nothing is freed twice or used after free — it is simply never freed |

It also explains the workload dependence recorded in the first 2026-09-03
section: the leak is churn-proportional because it needs a teardown to race a
kill, so it scales with how often that happens, not with process count.

Note that this leaks **user pages as well as heap** — the abandoned loop stops
partway through `free_page_at`, so most of those 1.47 M pages are never returned
to the PMM either. The earlier "kernel-heap leak, not a user-page leak" reading
in this doc is correct only about which tripwires fire; PMM free really does
drop (624 420 / 1 048 576 after one build).

### Fix options

Not implemented — the choice has real safety weight, because this code path
exists to *prevent* a use-after-free and both liveness gates in it are
load-bearing (`docs/archive/COW_PILE_AUDIT.md` §10, the F8 fix).

1. **Never let an entry leave the list until it is fully freed.** Claim it in
   place with a CAS (the shape `SlotTable::reclaim_retired` already uses for the
   process table: swap the pointer out, and exactly one racer wins), so an
   abandoned drain leaves the entry claimable again instead of unreachable.
   This is the only option that makes abandonment *recoverable*.
2. **Pop one entry at a time** rather than bulk-draining into a local. Cheap and
   strictly better, but it only bounds the loss to one address space per
   abandonment — and the measured entries are 12 000-42 000 pages *each*, so
   this alone would not move the campaign much.
3. **Stop draining from a thread that can be permanently preempted.**
   `process/reclaim.rs` already carries a `DRAINING[tid]` guard and a
   `clear_draining` fix precisely because that site "runs on an already-
   terminated thread"; the same site is losing memory here. Moving the pending
   drain to a site that cannot be abandoned addresses the cause rather than the
   symptom.

Options 1 and 3 are complementary and probably both wanted. Whichever is taken,
the regression gate is the bracket itself: `free_now_enter == free_now_exit`
after a clean build, and `[ASSTUCK] in_flight=0`.

### Reproducing

The instrumentation is uncommitted and temporary — `[LIVEHIST]`, `[LIVE8]`,
`[PCLIVE]`, `[ASLIFE]`, `[UFFLOW]`, `[ASPARK]`, `[L0PARK]`, `[UFBIG]`,
`[ASDROP]`, `[ASSTUCK]`. To take a reading:

```bash
RUSTFLAGS="-C link-arg=-Tlinker.ld -C force-frame-pointers=yes" \
  scripts/build_devbox_smoltcp.sh
RUSTFLAGS="-C link-arg=-Tlinker.ld -C force-frame-pointers=yes" \
  overlays/devbox/run-smoltcp.sh > logs/leak.log 2>&1 &
python3 scripts/vm_ready.py 2222
# in the guest, CARGO_HOME is /.cargo, NOT /root/.cargo (an --offline build
# against the wrong one fails to resolve and looks like a dependency error):
ssh -p 2222 root@localhost "cd /src/github.com/netoneko/akuma && \
  /bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root \
  CARGO_HOME=/.cargo RUSTC=/usr/local/bin/rustc \
  cargo build --release -p akuma -j4 --offline"
```

One clean build takes ~94 s and moves the 2^8 class from ~210 live objects to
~300 000. `RUSTFLAGS` must be passed to **both** scripts — `run-smoltcp.sh`
runs its own `cargo run` and will silently relink without frame pointers
otherwise. Set the full `RUSTFLAGS` including `-C link-arg=-Tlinker.ld`: the
env var replaces `.cargo/config.toml`'s `rustflags` rather than adding to it.
Frame pointers are needed only for `[PCLIVE]`; the `[ASDROP]`/`[ASSTUCK]`
brackets work without them.

`e2fsck -fy devbox.img` between runs
(`/opt/homebrew/opt/e2fsprogs/sbin/e2fsck`) — a hard-killed guest really does
damage the image.

## 2026-09-03 (third session) — independent confirmation

The fix was verified by the session that made it. It was then **re-confirmed
accidentally**, which is the stronger evidence: two 8-build in-guest campaigns run
to investigate an unrelated `rustc` SIGSEGV (`ERET_ELR_CLOBBER_ENTER_USER_MODE.md`)
logged `heap_mb` throughout and it never moved.

`heap_mb` from `[FSCACHE]`, every sample across 8 consecutive `cargo clean` +
full-kernel builds in one boot:

| arm | samples | heap_mb |
|---|---|---|
| `MEMORY=2048` | 9 | `287 290 290 290 290 288 294 289 290` |
| `MEMORY=4096` | 53 | `441` then `444` for every remaining sample |

Spread at 2 GB is **7 MB across 8 builds, with no trend**. The pre-fix symptom was
`13 MB -> 760 MB` over a comparable campaign, ending at the OOM wall with rustc
exiting 137. `[PMM-BUDGET] heap=` agrees independently: 73 699 - 75 273 pages
(288-294 MB), flat.

The 4 GB arm's higher-but-equally-flat 444 MB is not a partial leak — it is the
ext2 block cache's cap, which is `min(RAM/8, FSCACHE_CEILING_MB)` = 384 MB at
4 GB versus 256 MB at 2 GB. Heap total tracks the cache, exactly as it should.

### "Fixed" means the heap no longer GROWS — not that it has room

Worth stating, because reading this doc's status line alone would mislead. At 2 GB
the flat heap sits at ~290 MB **of which the unreclaimable ext2 block cache is
256 MB — 90 %** — and the same campaign recorded six kernel-heap OOMs on
allocations of 0.8-2 MB at 97-98 % heap used:

```
[ALLOC FAIL] requested=819200  heap_used=283/287MB (98%)
[OOM] allocation of 2102032 bytes failed (heap 281MB / 289MB used) — killing process
```

Those are **not** this leak returning. They are a heap that is flat but almost
entirely block cache, and the cache cannot shrink: `ClockBlockCache` only ever
`push`es its `chunks: Vec<Vec<u8>>`, eviction recycles a slot **in place**, and no
`PmmHooks` reclaim path even names it. Chase those to
`EXT2_UNLINK_INODE_BLOCK_LEAK.md`'s sibling issue and `FSCACHE_CEILING_MB`, not
back to `drain_pending_ttbr_frees`.

The regression gate stated above still stands: `free_now_enter == free_now_exit`
and `[ASSTUCK] in_flight=0`.
