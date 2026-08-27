# Why a bare syscall costs 3x Linux (2026-08-27)

Status: **open — handoff brief.** The floor is measured and the rig to measure
it is in the tree. The cause is not established. Four candidates have been
tested and killed; one strong lead has been read in the source but never timed.

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
reason for the gap; it is accumulated per-syscall bookkeeping that has never
been measured, which is exactly what this document exists to hand over.

## Background

- [`EXT2_READ_PATH_STAGE_PROFILE.md`](EXT2_READ_PATH_STAGE_PROFILE.md) — the
  read-path stage table, where this floor was found, and the estimator story.
- [`USER_COPY_BYTE_LOOP.md`](USER_COPY_BYTE_LOOP.md) — the change that made the
  boundary the dominant term.
- `src/syscall/utils/read_profile.rs` — the spans, the floor arm, the caveats.
- `docs/reference/subsystems/config-flags.md` — `read-profile` and the syscall
  debug flags.
