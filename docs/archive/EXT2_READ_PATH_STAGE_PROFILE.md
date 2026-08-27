# Where a warm `read(2)` actually spends its time (2026-08-27)

Status: **investigation — instrument landed, no behaviour changed.** The kernel
feature `read-profile` and two probes are in the tree; nothing on the read path
was modified as a result of them yet.

Companions: [`USER_COPY_BYTE_LOOP.md`](USER_COPY_BYTE_LOOP.md) (the change that
made this the next question), [`EXT2_PER_FD_INODE_READ_PATH.md`](EXT2_PER_FD_INODE_READ_PATH.md)
(read-by-inode), [`EXT2_WRITEBACK_DESIGN.md`](EXT2_WRITEBACK_DESIGN.md).

---

## The question, and why the previous answer was wrong

`USER_COPY_BYTE_LOOP.md` closed by saying `seq_read` had flipped from byte-bound
to **syscall-fixed-cost-bound**, at "**~17 µs of fixed cost per `read(2)`**", and
named that as the next thing to attack. That figure was never measured. It was
*derived*: `seq_read`'s 5 ms wall time, minus an estimated per-byte term, divided
by 256 reads.

Measured directly, from inside the kernel, a warm 8 KB `read(2)` excursion costs
**about 2.4 µs**, and a 4 KB one about **2.0 µs**. There is no 17 µs of fixed
cost in the read path. The wall time it was inferred from is real, but almost
none of it is in `read(2)`.

This is the third time in this subsystem that a number inferred from a fitted
cost model has been wrong (D-4 in `EXT2_WRITEBACK_DESIGN.md`, the "`seq_read` →
2 ms" prediction, and now this), and the second time the same lesson applies:
**a per-stage cost is only credible if something measured that stage.**

## The instrument

Three pieces, all in the tree:

| | what it measures | where |
|---|---|---|
| `read-profile` (kernel feature) | per-stage tick accounting inside `sys_read`, plus the two wrappers around it and the gap between consecutive calls | `src/syscall/utils/read_profile.rs` |
| `read_stage_profile.py` | drives a workload, parses `[READPROF]` windows, **drops the dirty ones** | `scripts/benchmarks/` |
| `read_syscall_cost.c` | one static musl aarch64 binary, run on Akuma **and** on Linux | `userspace/ext2probe/c/` |

The kernel side times three nested spans, each by the function that owns it, so
nothing is passed between them and nothing can go stale across a preemption:

```
  rust_sync_el0_handler   SPAN_EXC   BKL entry, tripwires, dispatch, deferred kill
    handle_syscall        SPAN_HS    + pid lookup, counters, syscall-stats hooks
      sys_read            SPAN_SR    + the per-stage breakdown
```

`exc − hs` is the outer wrapper (`wrap`), `hs − sr` is `handle_syscall`'s own
prologue and epilogue (`pro_epi`), and inside `sys_read` each stage is lapped
individually. A fourth number, `gap`, spans the *other* side: from one read's
kernel exit to the next read's kernel entry.

### Four ways this measurement lied before it worked

Each of these produced a confident, plausible, wrong number first.

1. **Out-of-order counter reads.** `akuma_timer::read_counter` is a bare `mrs
   cntvct_el0`. `mrs` does not serialise, so on an out-of-order core the
   timestamp *after* a memcpy can execute before the memcpy retires — and a 4 KB
   widened user copy is only ~400 instructions, which fits inside the reorder
   window. The first sweep reported a 4096-byte `copy_to_user` at **66 ns**
   (62 GB/s) and a 65536-byte one at **`min=0 ns`**. The fix is `isb` before the
   `mrs`, which is what Linux's `arch_timer_read_counter` does; its cost shows up
   honestly as `cal=` in every dump (~12–25 ns/lap).

2. **The mean is not the cost.** Two or three reads per thousand are descheduled
   mid-syscall for hundreds of microseconds. In one window the mean was 10.6 µs
   and the minimum 1.25 µs *for the same 1024 reads of the same 4096 bytes* —
   two statistics that support opposite conclusions ("every read costs 10 µs,
   fix the read path" vs "990 reads cost 1.3 µs and three were preempted"). Only
   the distribution separates them, so the dump carries a log2-µs histogram and
   the harness discards any window with a sample past 8 µs.

3. **Foreign reads in the window.** Every file read in the system lands in the
   accounting, including sshd's and busybox's few-byte reads. One 32-byte read
   makes every `min` in the window the cost of *that* read. Hence `MIN_BYTES`
   and the `bytes=<mean>/<min>..<max>` field the harness checks.

4. **The instrument's own console output.** `dump()` writes ~14 lines to the
   serial console — about **55 ms** per window on this machine, which is the
   single most expensive thing in the whole experiment. Stamped in the wrong
   order it was charged to the next read's `gap`, taking the mean gap from
   ~1.9 µs to ~315 µs and making the instrumented `read(2)` arm look **15×
   slower** than the uninstrumented `pread(2)` one. Both were artifacts.
   **Never read wall-clock throughput off a `read-profile` build.**

## What a warm read costs

`dd` over a warm 8 MB file, `SMP=1`, `MEMORY=2048`, HVF, `--features
read-profile,no-tests`. Medians over clean windows only (768 reads at 4 KB,
512 at 8 KB); `resid` is `sr` minus the named stages.

| | 4096 B | | 8192 B | |
|---|---:|---:|---:|---:|
| **whole excursion (`exc`)** | **1974 ns** | | **2399 ns** | |
| `wrap` — EL0 handler around dispatch | 157 ns | 8.0% | 152 ns | 6.3% |
| `pro_epi` — `handle_syscall` pro/epilogue | 572 ns | 29.0% | 545 ns | 22.7% |
| `validate_user_ptr` | 21 ns | 1.1% | 22 ns | 0.9% |
| `get_fd` (proc lookup + fd clone) | 120 ns | 6.1% | 113 ns | 4.7% |
| `VfsBklGuard::new` | 34 ns | 1.7% | 32 ns | 1.3% |
| staging `vec![0u8; n]` | 252 ns | 12.8% | 428 ns | 17.8% |
| `read_at_open_file` (ext2, warm) | 638 ns | 32.3% | 882 ns | 36.8% |
| `copy_to_user` | 88 ns | 4.5% | 133 ns | 5.5% |
| `update_fd` (advance position) | 75 ns | 3.8% | 72 ns | 3.0% |
| `resid` | 17 ns | 0.9% | 18 ns | 0.8% |

Three things fall out of this table, and none of them was the expected answer:

- **`copy_to_user` is finished as a cost centre.** 133 ns for 8 KB is 62 GB/s.
  The widening in `USER_COPY_BYTE_LOOP.md` did not just improve it, it removed
  it from the list. (This number is real — it survives the `isb` fix that
  invalidated the first, larger-looking measurement.)
- **`pro_epi` is the second-largest single item**, at 22–29% of the whole
  syscall, and it is not filesystem work at all — see below.
- **The staging buffer's `alloc_zeroed` costs more than `copy_to_user`**, at
  every size. It allocates and zeroes bytes that are overwritten on the next
  line.

## `pro_epi`: a quarter of the syscall is debug bookkeeping

`handle_syscall`'s prologue and epilogue are not free, and most of what they do
is instrumentation that ships enabled. `PROCESS_SYSCALL_STATS` and
`PROC_SYSCALL_LOG_ENABLED` are both `true` in `src/config.rs` on every build
except `kernel_profile_extreme`; together they add, per syscall, two
`lookup_process_shared` calls, two `uptime_us()` reads, a `syscall_stats` update
and a push into a per-process ring buffer.

Same-source A/B, both arms `--features read-profile,no-tests`, `.bin` files
confirmed to differ, clean windows only:

| | 4096 B | 8192 B |
|---|---:|---:|
| `pro_epi`, both flags **on** (shipping default) | 572 ns | 545 ns |
| `pro_epi`, both flags **off** | **303 ns** | **290 ns** |
| whole `exc`, on | 1974 ns | 2399 ns |
| whole `exc`, off | **1580 ns** | **2045 ns** |

**~260 ns per syscall, or 15–20% of a warm `read(2)`.** Every stage that should
not have moved did not (`fs` 638→585, `alloc` 252→231 — within host drift),
which is the check that the lever hit only what it was aimed at.

This is a real trade, not a free win: turning them off removes
`/proc/<pid>/syscalls` and the per-process exit stats, which are load-bearing for
debugging. It is recorded here as a measured price, not as a recommendation. A
third option nobody has costed is keeping the facilities but making the
*recording* cheaper — three `lookup_process_shared` calls per syscall for one
pid is the obvious redundancy.

## Against Linux, with the same binary

`userspace/ext2probe/c/read_syscall_cost.c` built once as a static musl aarch64
binary and run on both kernels, so the two differ by the kernel and nothing
else. Linux is Ubuntu 26.04 in Lima under Apple `vz`, ext4; Akuma is plain
`cargo build --release` under QEMU HVF, `SMP=1`, ext2 — same host, same silicon.
Figures are the **cheapest of 100 passes of 100 calls** (see § "The estimator was
the finding"; a median over long passes measures interference, not the syscall).

| arm | Linux | Akuma | ratio |
|---|---:|---:|---:|
| `getpid` — a syscall that does nothing | 147 ns | 440 ns | **3.0×** |
| `read` of 0 bytes | 172 ns | 640 ns | 3.7× |
| `/dev/zero` 4 KB | 294 ns | 1160 ns | 3.9× |
| `/dev/zero` 64 KB | 2235 ns | 5330 ns | 2.4× |
| warm file `pread` 4 KB | 249 ns | 2050 ns | **8.2×** |
| warm file `pread` 8 KB | 328 ns | 2700 ns | 8.2× |
| warm file `pread` 64 KB | 1614 ns | 15110 ns | 9.4× |
| warm file `read` 4 KB | 248 ns | 2110 ns | 8.5× |
| warm file `read` 64 KB | 1602 ns | 14850 ns | 9.3× |

Subtracting the rows gives the split directly, with no fitting:

| | Linux | Akuma |
|---|---:|---:|
| syscall round trip (`getpid`) | 147 ns | 440 ns |
| + reaching the fd and returning 0 bytes | +25 ns | +200 ns |
| + **the actual work of a 4 KB warm read** | **+77 ns** | **+1410 ns** |

The round trip is **3.0×**; the read work is **~18×**. Both matter, the read work
matters more, and it is the half this kernel can actually change.

**That 1410 ns is the stage table above.** `fs` 638 + `alloc` 252 + `fd` 120 +
`copy` 88 + `pos` 75 = 1173 ns, plus the part of `pro_epi` a `getpid` does not
pay. Three independent instruments — the in-kernel stage laps, the EL0-side gap,
and this cross-kernel probe — now agree within their noise, which they did not
before the estimator was fixed.

Two incidental results worth keeping:

- **`/dev/zero` is slower than a warm file on both kernels.** Zeroing a fresh
  buffer costs more than copying page-cache bytes. An arm meant purely as a
  no-filesystem control turned out to be an upper bound, not a lower one.
- **Static musl has no vDSO for `clock_gettime`.** The `timed=` column (a
  `clock_gettime` pair around *each* read) is ~1.7 µs on Linux and ~10–30 µs on
  Akuma, where `clock_gettime` is a real syscall with 1 µs resolution. It is
  printed to document that cost, and is not a read measurement.

## The estimator was the finding

The first version of this table reported **`getpid` at 2948 ns and a 0-byte read
at 5130 ns — a 37× gap on an empty syscall.** Both were wrong, and the error was
not in the kernel or in the probe's logic. It was the sampling.

Interference on this guest arrives as a few multi-hundred-microsecond stalls per
thousand syscalls. A pass of 2000 calls therefore almost always contains one, so
**every** pass is contaminated and even the minimum across passes is an
interference measurement. 100 passes of 100 calls makes a clean pass
overwhelmingly likely:

| `getpid`, same binary, same boot | reported |
|---|---:|
| median of 5 passes × 2000 calls | 3044 ns |
| min of 5 passes × 2000 calls | 2437 ns |
| **min of 100 passes × 100 calls** | **440 ns** |

**A 6.9× error, entirely in how the samples were grouped** — same total work
either way. It produced a false positive on the way out: an A/B that deleted the
NEON save/restore from `sync_el0_handler` appeared to cut `getpid` from 2437 ns
to 455 ns, a 5× "win" that was one arm catching a clean pass and the other not.
Re-measured properly the NEON block costs under ~30 ns and is not worth touching.

The tell was arithmetic that does not close: **36 store/load-pair instructions
cannot be 2 µs.** That should have stopped the conclusion before it was written
down. Same failure as "~17 µs of fixed cost", and as D-4 before it — a statistic
that was never a measurement of the thing it was named after. The rule that
survives all three: *state the per-operation cost the number implies, in
instructions or bytes, and reject the number if the arithmetic does not close.*

## The syscall floor, and four theories about it

> The floor is now its own open investigation with its own handoff brief:
> [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md).
> This section is what that document was built from.


`getpid` is instrumented with the *same two spans* as `read` (`FLOOR_NR` in
`src/syscall/utils/read_profile.rs`), so the shared part of every syscall can be measured
rather than inferred. Minima, `SMP=1`, `--features read-profile,no-tests`:

| | ns |
|---|---:|
| EL0 round trip — vector asm save/restore, `svc`, `eret`, the user loop | **180** |
| `wrap` — BKL enter/leave, entry tripwires, `SYNC_EC_EL0`, the SVC-verify read | **167** |
| `handle_syscall` prologue + epilogue + dispatch | **333** |
| **= a syscall that does nothing** | **680** |
| `sys_read` body, 4 KB (`fs` 541 + `alloc` 208 + `fd` 83 + `copy` 41 + `pos` 41) | ~914 |
| **= a warm 4 KB `read(2)`** | **~1600–2050** |

The 680 ns is measured two independent ways and they agree exactly: the probe's
undisturbed `getpid` is 680 ns, and `exc` (500) + round trip (180) is 680.
`wrap` reads 167 ns from *both* the `getpid` arm and the `read` arm, which is the
control — it is the same code on both paths and it measures the same.

Four theories were tested against this. **Three were wrong**, and each was wrong
in a way worth recording.

### ❌ "It's the NEON save/restore" — disproved

`sync_el0_handler` saves and restores all 32 `q` registers plus FPCR/FPSR on
every syscall; Linux does this lazily. Deleting the block appeared to cut
`getpid` from 2437 ns to 455 ns. That 5× was an artifact of long sample passes
(see § "The estimator was the finding"); re-measured, the block costs under
~30 ns. **The arithmetic said so before the experiment did: 36 store/load-pair
instructions cannot be 2 µs.**

### ❌ "The instrument inflates `pro_epi`" — disproved

`Rec::commit` closes the `sr` span and then does ~20 atomic read-modify-writes,
all of which land inside `pro_epi`. Plausible, and measured directly (the
`commit` line in the dump): **83 ns**, not the ~300 needed. `pro_epi` is real.

### ❌ "`getpid` takes a fast path that skips the prologue" — disproved

The only remaining way `pro_epi` (500 ns on the read path) could exceed a whole
`getpid` round trip. It does not: `getpid` dispatches from the same `match` and
its `hs` span is 333 ns. The real explanation is duller — `pro_epi` on the read
path is `hs − sr`, so it also carries `commit`'s 83 ns, `Rec::new`'s calibration
pair, and `sys_read`'s own (large) function prologue.

### ❌ "The per-syscall user-instruction read is expensive" — disproved

`VERIFY_SVC_AT_ENTRY` reads 4 bytes of user code on **every** syscall to catch a
stale-I-cache spurious SVC — a full `copy_from_user_safe` with fault-handler
setup and two fences. Turning it off: `exc` 500 → 500 ns, `hs` 333 → 333 ns,
probe round trip 680 → 670 ns. **No measurable cost.** Note the resolution
limit: the counter ticks at 41.7 ns, so a minimum cannot resolve anything
smaller — this bounds the tripwire at ≤42 ns rather than pricing it.

### ✅ "It's the two shipping debug facilities" — confirmed

Same-source A/B on `PROCESS_SYSCALL_STATS` + `PROC_SYSCALL_LOG_ENABLED`, both
`true` in every build except `kernel_profile_extreme`:

| `getpid`, minima | flags on | flags off | |
|---|---:|---:|---:|
| `handle_syscall` pro/epilogue (`hs`) | 333 ns | **125 ns** | **−62%** |
| whole kernel excursion (`exc`) | 500 ns | **291 ns** | **−42%** |
| full round trip (probe) | 680 ns | **460 ns** | **−32%** |
| `wrap` (control — lever is inside `hs`) | 167 ns | 166 ns | unchanged |

**~208 ns on every syscall in the system**, and 62% of everything
`handle_syscall`'s prologue and epilogue do. The unchanged `wrap` is what says
the lever hit only what it was aimed at. Against Linux the floor is 460 ns vs
147 ns with them off, and 680 ns vs 147 ns with them on — so they are a third of
that gap on their own.

Still a real trade: they are `/proc/<pid>/syscalls` and the per-process exit
stats. The untested third option is keeping both and making the recording
cheaper — the prologue and epilogue between them do **three**
`lookup_process_shared` calls and two `uptime_us` reads for one pid.

## The `nr > 500` band: 2.7 ms, and a wrong answer

This started as a wrong attribution and is kept because the correction is the
useful part. The probe's first "unimplemented syscall" arm used number **4095**,
on the reasoning that "definitely not a syscall" was the cleanest case. It
measured **1,972,440 ns** and that was written up as the cost of the
dispatcher's `[ENOSYS]` console print. Both halves were wrong.

An actually-unimplemented syscall (`107`, `timer_create`) costs **470 ns** —
the same as `getpid` within noise. The console print is real but rare, and it
now sits behind `SYSCALL_ENOSYS_DIAG` (off by default; `src/config.rs`), which
is where a per-call debug print belongs. What 4095 was measuring is a different
mechanism entirely.

`src/exceptions.rs` treats **any syscall number above 500** as evidence of a
stale instruction cache rather than as a syscall:

```rust
if syscall_num > 500 {
    let count = JIT_RETRY_COUNT.fetch_add(1, Relaxed);
    if count < 16 {
        safe_print!("[JIT] IC flush + replay #{} bogus nr={} …");
        ic iallu; dsb ish; isb;
        if !prev_is_svc { frame.elr_el1 = elr - 4; }   // replay
        return frame_ref.x0;                            // NOT dispatched
    }
    …
}
```

The hazard is real: a JIT writes code, the CPU (or QEMU's TB cache) still holds
the old translation, `x8` is set by stale code, and dispatching that garbage
writes an errno into `x0` over a live pointer — the intermittent `WILD-DA`
class. Not dispatching, and leaving `x0` alone, is the right response *to that*.

### It does not replay, and the retry bound never engages

`prev_is_svc` guards the ELR rewind, and for a genuine `svc` the instruction at
ELR-4 **is** an svc — so ELR is not rewound and nothing re-executes. Measured
across 20,000 calls: **20,000 `[JIT] IC flush` lines and zero "giving up"**. One
trap per call, no retry loop, and the 16-retry bound is unreachable on this
path. The cost is one console line and one `ic iallu` per call: **2,696,750 ns**.

### It silently reports success

Returning `frame_ref.x0` without dispatching hands userspace back its own first
argument. `userspace/ext2probe/c/` built as a four-number probe:

| syscall number | returns | |
|---|---|---|
| 107 (unimplemented) | `-1`, `errno=38` | ENOSYS, correct |
| 499 (just under the band) | `-1`, `errno=38` | ENOSYS, correct |
| **501** (just over) | **`43690`, `errno=0`** | **not ENOSYS** |
| **4095** | **`43690`, `errno=0`** | **not ENOSYS** |

43690 is `0xAAAA`, the first argument passed. A libc probing for a syscall above
500 concludes it exists and succeeded. That is worse than the 2.7 ms: it is a
wrong answer, not a slow one. And 500 is close — Linux aarch64 numbering is past
460 and still allocating; this kernel already defines up to 439 (`faccessat2`).

### The fix the code already has the ingredients for

The number cannot distinguish the two cases:

* stale I-cache → must **not** touch `x0`, must not dispatch;
* genuinely out-of-range syscall → must return `-ENOSYS`.

But `prev_is_svc`, computed two lines above, does. The `VERIFY_SVC_AT_ENTRY`
block says as much itself — it "generalizes the >500 JIT workaround below (which
a VALID syscall number like 95 slips past)", and "a legitimate syscall always
reads back an svc here". So: when `prev_is_svc` is true, fall through to normal
dispatch and return `ENOSYS`; only when it is false do the flush-and-don't-
dispatch. That keeps the anti-clobber protection exactly where it is justified
and removes the false positive.

**Not implemented** — it is the exception path with documented crash history
behind it, so it wants its own A/B (the `501`/`4095` return values above are the
test) rather than a drive-by change.

## What this reorders

The list this replaces (the previous session's handoff) put "per-syscall fixed
cost, ~17 µs, look at `validate_user_range` / `VfsBklGuard` / `get_fd`" at the
top. Measured, those three are **21 ns, 34 ns and 120 ns**.

In evidence order now, for a warm 4 KB read costing ~2050 ns against Linux's
249 ns:

1. **The `nr > 500` band** — 2.7 ms per call *and* a silent wrong answer
   (returns the caller's first argument instead of `ENOSYS`). Not on the read
   path at all, and the only correctness bug in this document.
2. **`PROCESS_SYSCALL_STATS` + `PROC_SYSCALL_LOG_ENABLED`, ~208 ns on *every*
   syscall** — 62% of `handle_syscall`'s prologue/epilogue, 32% of the syscall
   floor, A/B'd twice with an unchanged control. A real trade, or a cheaper
   recording path (three `lookup_process_shared` calls for one pid).
3. **`read_at_open_file`, 541 ns** — the largest stage on the read path and the
   only one doing real filesystem work. Warm, so this is block-cache lookup plus
   a memcpy, and nothing has profiled *inside* it.
4. **The staging buffer, 208 ns** — `alloc_zeroed` for bytes overwritten on the
   next line, five times what `copy_to_user` costs. Needs the safe removal from
   `USER_COPY_BYTE_LOOP.md`.
5. **`wrap`, 167 ns** — BKL enter/leave and the entry tripwires. Not the
   SVC-verify read, which is ≤42 ns.
6. **The EL0 round trip, 180 ns.** Small, and not the NEON block.
7. `validate_user_ptr`, `VfsBklGuard`, `copy_to_user`, `update_fd` — 0/0/41/41 ns
   at the minimum. Below the counter's resolution; not worth a line of code.

`delete` at ~150× Linux is untouched by any of this and remains the worst ratio
in the subsystem; it is device-write-bound and on a different axis.

## Method notes

- **Group samples so a clean one can exist.** The single largest error in this
  investigation (6.9×) was not a wrong lever or a wrong reading — it was
  averaging over passes long enough to guarantee interference in every one. Size
  a pass so that a *typical* pass escapes the disturbance, then take the
  minimum.
- **Check the arithmetic a number implies before believing it.** 2 µs for 36
  instructions, 66 ns for a 4 KB copy, 17 µs for a syscall that does a memcpy —
  each was ruled out by one line of arithmetic that was done too late.
- **The minimum has a resolution floor of one counter tick — 41.7 ns here.**
  Every `min` in this document is a multiple of it, so a lever worth less than
  that reads as exactly zero change (`VERIFY_SVC_AT_ENTRY`: 500 → 500 ns). That
  bounds such a lever; it does not price it.
- **Instrument the floor, not just the feature.** Almost every contradiction
  here dissolved the moment `getpid` was measured with the same spans as `read`.
  A stage cannot cost more than the syscall containing it, and having both
  numbers makes that check possible instead of rhetorical.
- **Check host load before any guest measurement**, still. Arm A measured
  `exc=1594 ns` on a quiet host and `1974 ns` with a second QEMU running — but
  `dd`'s wall time over the same reads differed by **4×**. The clean-window
  filter is what makes the in-kernel number survive that; the wall clock does
  not.
- Prefer the histogram to any single statistic. Every wrong turn in this
  investigation was a mean.
- The probe is one binary run on two kernels *on purpose*. Building it with each
  system's own toolchain would have put a different libc wrapper in front of each
  `svc`, and the musl-vs-glibc `clock_gettime` difference above shows that is
  worth 1.5 µs — ten times the thing being measured on the Linux side.

## Reproducing

```bash
cargo build --release --features read-profile,no-tests
INSTANCE=1 MEMORY=2048 SMP=1 DISK=<clone>.img \
  scripts/cargo_runner.sh target/aarch64-unknown-none/release/akuma > rp.log &
scripts/benchmarks/read_stage_profile.py --log rp.log --port 2322 --bs 4096 8192

# cross-kernel, uninstrumented kernel only:
cargo build --release
userspace/ext2probe/c/build.sh --push-akuma 2322
userspace/ext2probe/c/build.sh --push-lima fc
```
