# Repeated in-guest clean builds leak kernel heap to the OOM wall

**Status: OPEN, not root-caused.** Found 2026-09-01 while running a Tier 5
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
