# Why a bare syscall costs 3x Linux (2026-08-27)

Status: **RESOLVED (same day) — cause established, fixed, re-measured.** See
"Resolution" at the bottom. The original handoff brief is preserved below
verbatim, followed by the deferred follow-ups.

Scope: **the syscall boundary only** — `svc` in, `eret` out, with nothing in
between. Read-path work is a different document
([`EXT2_READ_PATH_STAGE_PROFILE.md`](EXT2_READ_PATH_STAGE_PROFILE.md)); this one
is about the cost every syscall pays before any subsystem is reached.

---

## The number

Same static-musl aarch64 binary on both kernels, same host, same silicon
(Apple), `SMP=1`. Cheapest of 100 passes of 100 calls.

| | Linux | Akuma | |
|---|---:|---:|---|
| `getpid` — a syscall that does nothing | **147 ns** | **440 ns** | **3.0×** |
| same, with the two debug flags off (below) | | **330 ns** | 2.2× |
| `getppid` (control — must match `getpid`) | ~150 ns | 460 ns | |

> **This table is superseded — do not quote it as current (noted 2026-08-28).**
> It is the *problem statement*, preserved as written. The cause was found and
> fixed the same day (see "Resolution" below), and the numbers were re-measured
> on 2026-08-28 with the same probe binary on both guests while extracting
> `akuma-syscalls`:
>
> | | Akuma `SMP=4` | Akuma `SMP=1` | Linux (4 vCPU) |
> |---|---:|---:|---:|
> | `getpid` | **130 ns** | 190 ns | **136 ns** |
> | `uname` | 140 ns | 240 ns | 154 ns |
> | leaf (`akuma_get_version`) | **90 ns** | 160 ns | — |
>
> Parity at `SMP=4`, 1.4× at `SMP=1`, and a leaf syscall *below* Linux's floor.
> The `uname`/`getpid` ratio — 1.08× on Akuma against 1.13× on Linux — says
> `copy_to_user` is not where Akuma loses; what remains at `SMP=1` is the fixed
> boundary. **The analysis below still stands; only this headline does not.**
> Source: [`AKUMA_EXTRACT_SYSCALLS.md`](AKUMA_EXTRACT_SYSCALLS.md) §7.8.

Linux is Ubuntu 26.04 in Lima under Apple `vz`; Akuma is `cargo build --release`
under QEMU HVF. Different hypervisors, which is a caveat on the absolute ratio —
but not on the internal decomposition below, and not on any A/B, all of which
are Akuma-vs-Akuma.

**Why this matters more than it looks.** 300 ns × every syscall is a tax on
every subsystem at once, and it is the floor under every other performance
number in this repo. A warm 4 KB `read(2)` is 2050 ns, so the boundary is ~21%
of it before the filesystem does anything.

## The budget

From `read-profile`'s floor arm (`FLOOR_NR` = `getpid` in
`src/syscall/utils/read_profile.rs`), which wraps `getpid` in the *same two
spans* used for `read`. Minima:

| | ns | what is in it |
|---|---:|---|
| EL0 round trip | **180** | vector asm save/restore, `svc`, `eret`, the user loop |
| `wrap` | **167** | BKL enter/leave, entry tripwires, `SYNC_EC_EL0`, SVC-verify |
| `handle_syscall` prologue + epilogue | **333** | pid resolution, counters, stats/log hooks |
| **total in-kernel + boundary** | **680** | |

Two independent measurements agree exactly: the probe's undisturbed `getpid` is
680 ns, and `exc` (500) + round trip (180) = 680. `wrap` reads **167 ns from
both the `getpid` arm and the `read` arm** — same code, same number, which is
the control that says the spans are honest.

> **The 680 ns is on the `read-profile` build, which adds ~210 ns to every
> syscall.** The shipping number is 440 ns. Use the table for *proportions* and
> the plain-kernel probe for absolutes. Reconciling the two is itself a small
> open task.

## The one confirmed lever

`PROCESS_SYSCALL_STATS` and `PROC_SYSCALL_LOG_ENABLED` (`src/config.rs`) are
`true` in every build except `kernel_profile_extreme`.

| | flags on | flags off |
|---|---:|---:|
| plain kernel, `getpid` | 440 ns | **330 ns** |
| plain kernel, warm 4 KB `read` | 2110 ns | **1740 ns** |
| `read-profile` kernel, `hs` span | 333 ns | **125 ns** |
| `read-profile` kernel, `wrap` (control) | 167 ns | 166 ns — unchanged |

The unchanged `wrap` is what says the lever hit only what it was aimed at.

**The two measurements of the same lever disagree** — 110 ns on the plain
kernel, ~210 ns on the instrumented one — and that gap is about the size of
inter-boot noise on this host (the floor has read 440/470/500/520 ns across
boots of the *same* binary). Tightening this is a good first task: it is cheap,
and it calibrates how much any later result can be trusted.

Cost: `/proc/<pid>/syscalls` and the per-process exit stats. A third option
nobody has costed is keeping both and making the recording cheaper — see below,
because the expensive part is probably not the recording.

## The prime lead: Akuma has no `current`

**Not measured. Read in the source, never timed.** This is where to start.

Linux gets the current task from a register (`sp_el0` → `task_struct`): one
load, O(1), no lock, no IRQ mask. Akuma re-derives it. `handle_syscall`'s
prologue, **unconditionally, before any flag is consulted**:

```rust
akuma_exec::threading::set_thread_current_syscall(syscall_num);
let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
if let Some(proc) = akuma_exec::process::lookup_process_shared(owner_pid) {
    proc.last_syscall.store(syscall_num, Ordering::Relaxed);
    proc.current_syscall.store(syscall_num, Ordering::Relaxed);
}
```

Follow those two calls down:

`read_current_pid()` (`crates/akuma-exec/src/process/children.rs:368`)
1. `current_thread_id()` — a register read, cheap;
2. `with_irqs_disabled(|| THREAD_PID_MAP.lock().get(&tid).copied())` —
   **IRQ mask + a lock + a map lookup**;
3. `table::with_process(pid, |p| p.tgid)` — **IRQ mask + a linear scan**.

`lookup_process_shared(pid)` (`children.rs:491`) → `table::get_process_ptr` →
`with_irqs_disabled(get_process_ptr_inner)`:

```rust
fn get_process_ptr_inner(pid: Pid) -> Option<*mut Process> {
    for i in 0..MAX_PROCESSES {            // MAX_PROCESSES = 64
        if SLOT_STATES[i].load(Relaxed) != slot_state::ACTIVE { continue; }
        let ptr = PROCESS_SLOTS[i].load(Acquire);
        if !ptr.is_null() && unsafe { (*ptr).pid } == pid { return Some(ptr); }
    }
    None
}
```

So the **unconditional** prologue of every syscall does, at minimum: one locked
map lookup and **two IRQ-masked linear scans of a 64-slot table**, each scan
touching an atomic per slot across several cache lines, plus a pointer chase per
active slot. With the debug flags on it is up to **four** scans (`syscall_stats`
in the prologue, `add_time_us` + `log::record` in the epilogue).

That would explain the confirmed lever without the recording itself being
expensive: what the flags add is not the bookkeeping, it is **more lookups**.
Which is a much better fix than turning the diagnostics off — resolve the
process once per syscall and pass it down, or give threads a `current` pointer.

`BORROW_TRACKING_ENABLED` is `false`, so `borrow_inc` inside
`lookup_process_shared` is already a no-op. Ruled out.

### First experiment to run

Time `get_process_ptr_inner` directly with the same span machinery
(`src/syscall/utils/read_profile.rs` shows the pattern; `isb` before every
`mrs cntvct_el0`). Then A/B the obvious shape: resolve the process **once** in
`handle_syscall` and thread it through, and see how much of the 330 ns floor
moves. Predict the number first, from the scan length and cache-line count, and
reject the result if the arithmetic does not close.

## Already dead — do not redo these

Each was measured, not argued.

| candidate | verdict | evidence |
|---|---|---|
| **NEON save/restore** — all 32 `q` regs + FPCR/FPSR on every trap, in the 832-byte frame; Linux does this lazily | **free** | deleting the whole block: `getpid` 470 vs 440 ns. Also: 36 store/load-pairs cannot be 2 µs |
| **`VERIFY_SVC_AT_ENTRY`** — reads 4 bytes of user code on every syscall via the full `copy_from_user_safe` path | **≤42 ns** | off: `exc` 500→500, `hs` 333→333, probe 680→670. Bounded, not priced — see the resolution note below |
| **`Rec::commit`'s ~20 atomics** inflating `pro_epi` | **83 ns** | measured directly; the `commit` line in the `[READPROF]` dump |
| **`getpid` taking a dispatch fast path** | **no such path** | its `hs` span is 333 ns through the same `match` |
| **The `[ENOSYS]` console print** | **not a floor cost** | an unimplemented syscall is ~470 ns; the 2 ms once blamed on it was the `nr > 500` band (a separate bug, see the read-path doc) |

**Not completed: the two `isb`s** around `msr daifclr/daifset` in
`sync_el0_handler`. Each is a full pipeline flush and they are on every syscall.
An attempt was made and abandoned because the arms got mislabeled (a restored
copy still had the ISBs removed, and one build failed while the boot silently
used a stale binary). Unknown, cheap to test, and the natural companion to the
`current` work.

## Other untested surface, roughly by size

1. **`wrap`, 167 ns** — BKL `enter_kernel`/`leave_kernel` (uncontended at
   `SMP=1`, but still atomics), `note_exception_entry`, `note_exc_class`, the
   `SYNC_EC_EL0[ec]` atomic increment, `record_el0_trap`, the stale-window
   tripwire, `take_thread_kill_request`, and the `SVC POISON` frame check
   (`is_kernel_text(elr)`) on the way out. Several of these are diagnostics that
   run unconditionally.
2. **The vector asm itself** — 832-byte frame, ~34 GPRs + 6 system registers
   saved and restored. Linux's `kernel_entry` saves less on the syscall path
   because a syscall only has to preserve what the AAPCS says it must. NEON is
   already ruled out; the GPR block is not.
3. **`set_thread_current_syscall`** and `CURRENT_SYSCALL_NR` — global stores per
   syscall, cheap individually, on a shared line.
4. **`is_current_interrupted()`** in the prologue, before the dispatch.

## Deferred: audit for duplicate instrumentation machinery

**Deferred — pick up with the waste sweep above.** Reading the entry path for
this audit found several *overlapping* recorders paying for the same fact on
every syscall, grown one diagnostic at a time:

- "What syscall is running" is recorded **four times** per excursion:
  `CURRENT_SYSCALL_NR` (global), `THREAD_CURRENT_SYSCALL[tid]`
  (`set_thread_current_syscall`), `Process::current_syscall`, and
  `Process::last_syscall` — the last two via a full process lookup.
- Two independent counters systems (`syscall_counters::inc_*` with its own
  22-arm `match` before dispatch, and `ProcessSyscallStats` keyed off
  `PROCESS_SYSCALL_STATS`), plus the `PROC_SYSCALL_LOG` ring (`log::record`:
  spinlock + BTreeMap + VecDeque push per syscall).
- The wrap layer adds `note_exception_entry`, `note_exc_class`,
  `SYNC_EC_EL0[ec]`, `record_el0_trap` — several unconditional.
- `with_irqs_disabled` itself hides a per-call `isb`
  (`crates/akuma-primitives/src/irq.rs` `IrqGuard::new`), so every masked
  region — a dozen per syscall with the flags on — pays a context-sync
  barrier. Whether that `isb` is required for `DAIF` masking semantics at all
  (Linux's `arch_local_irq_disable` has none) is part of this audit.

The audit should determine, per recorder: who reads it, is that reader still
alive, and what one shared recording point would cost instead.

## Deferred: a per-syscall-path waste sweep

**Deferred — do after the `current`-resolution fix lands and is re-measured.**
The identity-lookup finding below generalizes: the syscall layer was grown one
diagnostic at a time, and nobody has audited the *whole* dispatch path for
work that is repeated, uncached, or allocated where a fixed buffer would do.
When picked up:

- **Clustered lookups**: values that arms routinely consume *together* (pid +
  tgid-leader `Process` + `channel` + `box_id`, the fd-table pointer) should
  come back as **one** cached resolution — ideally one struct per thread so a
  conjunction of N values costs the same one or two validated loads as any
  single value, instead of N accessor calls each re-validating. The identity
  cache added by this audit already clusters the pid/`Process` pair; extending
  the cluster is cheaper than adding more parallel caches, because each
  parallel cache is another set of writers to keep in sync. Decide cluster
  membership empirically: log which fields each syscall family actually reads
  (read/write/openat/futex first), then cluster what co-occurs.
- **Repeated resolution**: count every call to `read_current_pid`,
  `current_process_shared`, `lookup_process_shared`, `current_channel`,
  `with_current_process`, `uptime_us` *per syscall* (each entry arm, not just
  `getpid`) — anything resolved more than once per excursion is a candidate to
  hoist to one resolution threaded through the arm. The floor work found
  4×/5× per `getpid`; read/write arms start with `current_process_shared`
  again inside `sys_*` and are unaudited.
- **Mismanaged allocations**: per-syscall heap churn that a stack buffer or a
  per-thread scratch buffer would remove (`Vec`/`String` builds in hot arms,
  `Arc` clones where a borrowed read suffices — `current_channel`'s
  `Arc<ProcessChannel>` clone per syscall is the known example).
- **Not caching results**: any pure function of the process/thread re-derived
  per call (pid, tgid, box_id, fd-table pointer, signal state) belongs in the
  per-thread identity cache or the `Process`, not re-derived.
- **Redundant flag/counter work**: the per-syscall counter `match` (22 arms
  before dispatch) and tripwires that run even when their diagnostic is off at
  runtime.

Method: same as this audit — ablation A/B on a plain build, spans on a
`read-profile` build, arithmetic that closes. Start from the biggest syscall
families (read/write/openat/futex) since they dominate real workloads.

## The measurement rig

```bash
# kernel-side per-stage + floor spans
cargo build --release --features read-profile,no-tests    # SMP=1 only
INSTANCE=1 MEMORY=2048 SMP=1 DISK=<clone>.img \
  scripts/cargo_runner.sh target/aarch64-unknown-none/release/akuma > rp.log &
scripts/benchmarks/read_stage_profile.py --log rp.log --port 2322 --bs 4096

# cross-kernel / absolute numbers — PLAIN build only
cargo build --release
userspace/ext2probe/c/build.sh --push-akuma 2322
userspace/ext2probe/c/build.sh --push-lima  fc
```

`userspace/ext2probe/c/read_syscall_cost.c` is built by `userspace/build.sh` into
`bootstrap/bin/`, so a populated disk already has it. Its `getpid`/`getppid`
arms are the floor; `--push-lima` puts the *same binary* on the Linux VM, which
is the point — building it separately in each guest would put a different libc's
wrapper in front of each `svc`, worth ~1.5 µs on the Linux side.

## Method warnings — every one of these cost hours

1. **Group samples so a clean one can exist.** Interference here is a few
   multi-hundred-microsecond stalls per thousand syscalls, so a 2000-call pass
   almost always contains one and even the minimum across passes measures
   interference. 100 passes × 100 calls, take the minimum. This single change
   moved the headline **6.9×** (a "37× empty-syscall gap" was really 3×) and had
   already produced a false 5× "win" from deleting the NEON block.
2. **`mrs cntvct_el0` is not serialising.** Timing a region with a bare `mrs`
   lets the closing read execute before the region retires: an 8 KB
   `copy_to_user` measured `min=0 ns`. Use `isb; mrs`, and print the calibration.
3. **A minimum has a resolution floor of one counter tick — 41.7 ns here.**
   Every `min` is a multiple of it, so a lever worth less reads as *exactly zero
   change*. That bounds a lever; it does not price it.
4. **Verify the build succeeded before believing a boot.** A failed `cargo build`
   leaves the previous `akuma.bin` in place and QEMU boots it happily. `cmp` the
   two arms' `.bin` files and require they differ.
5. **Restore A/B levers from `git show HEAD:<path>`, not from a copy you saved.**
   A saved copy silently goes stale the moment anything else in the file moves.
6. **Instrument the floor, not just the feature.** Nearly every contradiction in
   the read-path work dissolved once `getpid` was measured with the same spans.
   A stage cannot cost more than the syscall containing it — having both numbers
   turns that from rhetoric into a check.
7. **Check the arithmetic a number implies.** 2 µs for 36 instructions, 66 ns
   for a 4 KB copy, 2 ms for one `printf` — each was refutable on paper before
   the experiment, and each was believed anyway for a while.
8. **Never read wall-clock throughput off a `read-profile` build**: its window
   dump is a serial-console write inside a `read(2)`.
9. **Check host load first**, and expect ±100 ns between boots of the same
   binary on the floor. A/B arms cannot be interleaved (each needs a reboot), so
   take the minimum and repeat the pair if they are close.

## What "good" would look like

Linux does a bare `getpid` in ~147 ns on this hardware including its own
hardening. Akuma has no KPTI, no seccomp, no audit, and a single core — so the
floor should be *at or below* Linux's, not 2–3× above it. There is no structural
reason for the gap; it is accumulated per-syscall bookkeeping that has never been
measured, which is exactly what this document exists to hand over.

## Resolution (2026-08-27, same day)

**Cause confirmed: the prime lead was right, and bigger than written here.**
The unconditional prologue/epilogue re-derived "who am I" up to **nine times
per syscall** (not the 2–3 counted above): `read_current_pid` ×4 (prologue,
stats block, `sys_getpid`'s own arm, epilogue), `lookup_process_shared` ×5
(prologue, `is_current_interrupted` → `current_process_shared`, stats, epilogue
clear, epilogue timing), each costing an `isb` (every `with_irqs_disabled`
executes one — `IrqGuard::new`), a spinlock+BTreeMap walk, and a
**256**-slot masked scan (`MAX_PROCESSES` is 256 now, not 64 as this brief
said). Ablation ladder on the plain kernel priced the pieces (best-of-100×100,
same day, same host):

| arm | getpid | delta | names |
|---|---:|---:|---|
| R0 baseline (shipping flags) | 410 | | |
| A2 − `is_current_interrupted` | 370 | 40 | lock+map+scan + `Arc` clone |
| A3 − prologue/epilogue identity block | 330 | 40 | `read_current_pid` + lookup + stores |
| R0f flags off (pre-fix) | 240 | | → flags' own hooks ≈ 90 ns |
| **F1 fixed, shipping flags** | **150** | **−260** | |
| F1f fixed, flags off | 120 | 30 | remaining flag tax: `log::record` + `uptime_us` ×2 + inc/add |
| Linux, same probe (Lima `vz`) | 147 | | parity (different hypervisor caveat) |

The lap instrument added to `read_profile` (floor-arm laps, `F_LAP_*`) bounded
each cluster at ≤~41 ns warm but is tick-floored (41.7 ns) — the ablations
above are the numbers to trust. `read-profile`'s own overhead measured ~450 ns
with the laps today (~210 without, matching the brief's estimate).

**Fix**: a per-thread identity cache, `table::THREAD_IDENTITY`
(`crates/akuma-exec/src/process/table.rs`) — for each thread slot, both
resolutions a caller can want (own pid+`Process` for `current_process_shared`
semantics; tgid+leader `Process` for the syscall prologue/stats/log), written
only inside the same IRQ-masked critical section that writes `THREAD_PID_MAP`
(`thread_pid_map_insert`/`_remove` wrappers; every non-test map write site was
converted), re-validated against `SLOT_STATES` on every fast read so a retired
process falls back exactly like an uncached lookup. `handle_syscall` resolves
once and threads it through; `is_current_interrupted` reads the borrowed
channel instead of cloning the `Arc`. `IDENTITY_FALLBACKS` counts slow-path
fallbacks (health check: nonzero *steady-state* means a writer bypassed the
wrappers).

Collateral: warm 4 KB `pread` 2110 → **1100 ns** (the `fd` stage's
`current_process_shared` is now a cache hit); guest `rustc` self-host build
passes on the fixed kernel.

Validation: full host suite 824/824, `akuma-exec` 269/269, clippy clean,
`read-profile` arm builds, in-kernel boot suite passes, devbox boots sshd/shell
and runs the full probe. **Not yet soaked at SMP>1** (measured at `SMP=1`;
the cache is written under the map's existing IRQ-mask+lock discipline but the
`forktest_smp_matrix` should run before trusting `SMP=4` numbers).

Observed once, pre-existing (also on the unfixed kernel): a guest read-back
anomaly — `md5sum` of a freshly-pushed file disagreed with its true md5 while
`exec` read it correctly, and one guest `cargo` run died parsing its own dep-info
`.d` file as non-UTF-8 (a user rebuild did not reproduce). Signature points at
ext2 page-cache coherence, not this change. Uninvestigated — needs its own doc
if it recurs.

## Background

- [`EXT2_READ_PATH_STAGE_PROFILE.md`](EXT2_READ_PATH_STAGE_PROFILE.md) — the
  read-path stage table, where this floor was found, and the estimator story.
- [`USER_COPY_BYTE_LOOP.md`](USER_COPY_BYTE_LOOP.md) — the change that made the
  boundary the dominant term.
- `src/syscall/utils/read_profile.rs` — the spans, the floor arm, the caveats.
- `docs/reference/subsystems/config-flags.md` — `read-profile` and the syscall
  debug flags.
