# What a console line costs, and the trace that was 99.94% of a syscall

**Date: 2026-08-29.** Found while widening `userspace/memprobe/`'s arms for the
`akuma-syscalls-mem` extraction ([`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md)
§10.3.1). Not a memory bug — a logging one, which is why it lives here.

**Outcome: `SYSCALL_ERRNO_DIAG_ENABLED` is now `false`.** One syscall went from
249,806 ns to 150 ns.

---

## 1. The symptom

A new probe arm, `mremap_efault` — `mremap()` with an out-of-range old address,
which is argument decode and nothing else — measured **~250 µs**, about
**1,600× the syscall floor**. Every other decode arm in the same table read
130–200 ns.

The arm was not wrong. `mem_op_cost` verifies each arm's return value before
believing its cost, and this one returned `-1`/`EFAULT` as documented. It was
measuring something real; the question was what.

## 2. The measurement

Three builds, same probe, same host, `getpid` control steady at 154–166 ns:

| build | `mremap_efault` | line emitted |
|---|---:|---|
| `SYSCALL_ERRNO_DIAG_EXTRA = true` (default) | 249,806 ns | 103 bytes |
| `SYSCALL_ERRNO_DIAG_EXTRA = false` (compact) | 161,788 ns | 68 bytes |
| `SYSCALL_ERRNO_DIAG_ENABLED = false` | **146 ns** | none |

The cost is the console line, and it is **linear in the line's length with no
fixed component**:

```
verbose:  (249806 - 146) / 103 bytes = 2424 ns/byte
compact:  (161788 - 146) /  68 bytes = 2377 ns/byte
```

Predicting the compact arm from the verbose arm's ns/byte gives 164,824 ns
against 161,642 measured — **within 2%**. The model is one cost per byte and
nothing else.

**Why per byte.** `src/console.rs`'s `Uart::write` is a single
`write_volatile` to the PL011 data register, one call per byte, with no
buffering and no TXFF poll (that constant is `#[allow(dead_code)]` — nothing
waits on the FIFO). Under emulation each store to a device register traps out
of the guest, so ~2.4 µs per byte is the cost of a VM exit, not of a UART.

Corroboration from the other side: `[PSTATS]` for the same run reported
**90,113 `mremap` calls and 1 ms of kernel time between them** — ~11 ns each.
The kernel was never slow. The UART was.

So the trace was **99.94%** of an EFAULT-returning syscall.

## 3. What the trace is, and the dead code inside it

`src/syscall/mod.rs`'s epilogue, gated on `config::SYSCALL_ERRNO_DIAG_ENABLED`.
Its purpose is real and worth keeping in mind before deleting anything: a Go
runtime that does not check an error return will dereference the negative value
as a pointer, crashing with `FAR` = the errno, and this line is what names the
syscall that produced it.

**But it had been narrowed, and the narrowing silently broke it.** The gate is

```rust
errno_diag: self.cfg.errno_diag && is_efault,   // is_efault == (result == EFAULT)
```

with a comment recording why: *"TEMP DEBUG nca-build EFAULT: EINVAL floods
readlinkat probes during cargo builds."* `readlinkat` on a non-symlink correctly
returns `EINVAL` per POSIX, and cargo probes it per file, so the flood was real
and the workaround was reasonable. The consequence was not noticed:

- `err_name` computes `"ENOSYS"` and `"EINVAL"` arms that **cannot be reached** —
  `result` is `EFAULT` by construction inside the block.
- The whole `if syscall_num == nr::MMAP && result == EINVAL` decode — the one its
  own comment says *"the §E investigation hinges on"* — is **unreachable**.

So the code paid 250 µs per EFAULT to print a line, while the part described as
load-bearing had been dead since the narrowing.

**Fixed 2026-08-29.** The parameter is now `is_diag_errno` and the kernel passes
`result == EFAULT || result == ENOSYS || result == EINVAL`, so the block handles
what it claims to. Turning the flag on now costs the `readlinkat` flood as well
as the per-line cost — that is the caller's trade, but it is at least the trade
the source describes.

## 4. Why it mattered beyond tidiness

`errno_diag` fires on a path *userspace controls*. A loop on `mremap` with a bad
address drove the serial console at ~4,000 lines/second, and console writes
serialise across cores (`kernel_console_lock`). One probe run left a
**165,389-line** boot log — 2,938 identical lines in the last 3,000 alone.

That is an unprivileged local process degrading the whole system through a debug
trace. Not the headline reason to turn it off, but it is a reason.

## 5. What was considered

| option | verdict |
|---|---|
| Shorten the line | Linear only. 103 → 68 bytes bought 35%; ~160 µs is still ~1,000× the floor. |
| Batch the writes | Not available. PL011's `DR` is a byte register; there is no burst, and the cost is per store. |
| Rate-limit (emit occurrences 1, 2, 4, 8, …) | Works — `O(N)` → `O(log N)`, ~17 lines for 100k occurrences — and would have let the errno set widen back to EFAULT/ENOSYS/EINVAL, since the flood was the only reason it was narrowed. Built and host-tested, then **dropped as unnecessary complexity for a trace nobody is currently reading.** In git history if the situation changes. |
| **Turn it off** | **Chosen.** 250 µs → 150 ns, no machinery, no new state, nothing to maintain. |

The trace is a tool for a specific hunt. Turn it on for that hunt, turn it off
after — and read §3 first.

## 6. Result

| | before | after |
|---|---:|---:|
| `mremap_efault` | 249,806 ns | **150 ns** |
| …relative to `getpid` | 1,542× | **0.94×** (below the floor) |
| boot-log lines added by one probe run | ~160,000 | **55** |
| `[EFAULT]` lines in a boot | 2,938+ | **0** |

`mremap_efault` is now indistinguishable from `mmap_einval` (160 ns), which is
the EINVAL path that was never traced — exactly where an argument-decode
rejection should sit.

Re-measured against the pre-work checkpoint `f49ca08f` on an idle host, in ratio
terms: **1895.14x → 1.00x of `getpid`**. For scale, the same arm on Linux — same
static binary, run in the Lima VM — is 1.18x of its own `getpid`, so the path is
now *below* Linux's. Full three-way table, both platforms' regression runs, and
where the remaining gap to Linux actually is:
[`SYSCALL_TRACE_AUDIT.md`](SYSCALL_TRACE_AUDIT.md) § "Verification".

## 7. The rest of the audit

Every other `bool = true` debug flag in `src/config.rs` was checked. **The errno
diag was the only default-on trace writing to the console on a per-call path
userspace can drive:**

| flag | verdict |
|---|---|
| `PROC_SYSCALL_LOG_ENABLED` | Fine, and the pattern to copy — records `(pid, box, nr, time, duration, result)` into a **ring buffer** read through `/proc/<pid>/syscalls`. No console, no VM exit. |
| `FUTEX_ORPHAN_DIAG` | Bookkeeping (tid tracking, age computation), not a per-call print. |
| `DEBUG_SIGSEGV_SYSCALL_STUB` | SIGSEGV path only — rare by construction. |
| `SYSCALL_ERRNO_DIAG_EXTRA` | Format selector for the above; moot while it is off. |

**The rule this leaves behind:** anything that prints per syscall costs
~2.4 µs/byte and is reachable from userspace. If a diagnostic needs to run at
that rate, it belongs in a ring buffer like `PROC_SYSCALL_LOG_ENABLED`, not on
the console. `CLAUDE.md` already forbids *allocating* on the console path; this
is the same argument one step further out — the console is expensive even when
you allocate nothing.

## 8. The audit that followed

If one trace could be 99.94% of a syscall, the next question is how many others
there are. Every print under `src/syscall/` and `crates/*/src/` was classified —
**and the instances live in their own document**, because the list is long and
this one is about the method:

> **[`SYSCALL_TRACE_AUDIT.md`](SYSCALL_TRACE_AUDIT.md)** — every instance found,
> what each cost, what was done about it, and what was deliberately left
> unconditional.

Headline: 279 sites under `src/syscall/`, 80 with no gate, **25 of them
userspace-drivable** and now gated behind the flag that already existed for their
subsystem. Plus two in `crates/`, one of which (`[clock-diag]`) is the worst
instance in the whole audit.

The one rule worth repeating here, because it is a *method* point and not an
instance: **gate the argument, not just the print.** Three AIO stubs opened with

```rust
let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
```

and all three return `0` unconditionally — `exists` had one consumer, choosing
which debug string to print. Wrapping the `print!` alone would have left an IRQ
mask, a spinlock and a map probe on every call. Ask not "is the print gated" but
"is anything computed *for* the print gated with it".

## 9. The same question one layer down: a lock per page

Drilling into `madv_unmapped` — the second-loudest arm in `mem_op_cost`, and
clean of console writes (§7) — found the same shape without a print involved.

`madvise_dontneed_range`'s first pass filtered the range with

```rust
lazy_region_lookup_for_pid(proc.tgid, va)   // once PER PAGE
```

and that helper is `lookup_process_shared(pid)` + `with_irqs_disabled` +
`lazy_regions.lock()` + the lookup. A 64-page `MADV_DONTNEED` therefore performed
**64 process-table walks, 64 IRQ mask/unmask pairs and 64 spinlock round-trips**,
to consult one map that never changed — and to re-find a `Process` the caller
was already holding as `proc`.

Fixed by taking the lock **once for the range**. The `Vec` is reserved *before*
the hold, because `collect()` inside it would allocate with IRQs masked and a
spinlock held — the re-entrancy hazard `CLAUDE.md` § "Kernel conventions" is
about.

Measured A/B/A, 2 baseline and 2 fixed runs interleaved on a **loaded** host, so
read the ratio to `getpid` and not the ns:

| | ratio to `getpid` | spread |
|---|---|---|
| one lock per page | 12.5 – 49.7, median **30.1×** | 4.0× |
| one lock per range | 3.6 – 5.5, median **4.8×** | 1.5× |

**6.3× faster on the median; 2.3× comparing the baseline's best run to the
fixed version's worst.** The *stability* is the second result and arguably the
more interesting one: 64 lock acquisitions are 64 opportunities to contend or be
preempted, so the per-page version degraded badly under load while the hoisted
one barely moved. On an idle host the same arm measured 13.1× before the fix.

Correctness unchanged: `scripts/mem_suite.py` 10/10, boot suite 315 PASSED / 0
failures.

## 10. Applying it: marking syscalls for bypass

Removing work from a syscall is only half the point. `akuma_syscalls::FastPath`
already has a `Leaf` tier that skips the identity read, the two `Process` syscall
stamps, the epilogue re-resolve, the stats row, the `/proc/<pid>/syscalls` entry
and the clock read entirely — it just had **two members**, `uptime` and
`akuma_get_version`, because criterion 2 ("touches no `Process`, no process
table, no fd table, no address space") is genuinely rare.

Scanning every `sys_*` arm for a constant return with no argument use found four
more that qualify unconditionally: **`getuid`, `geteuid`, `getgid`, `getegid`**.
Three are literally `nr::GETUID => 0` in `handle_syscall`'s match; `geteuid`
calls a function whose entire body is `0`. There is no user model in this kernel,
so none of them consults a `Process`. They were already in `takes_no_args`; only
`needs_identity` was holding them back.

`getpid`, `gettid` and `getppid` deliberately stay `Full` — they read
`read_current_pid()`, the thread id and the process table respectively. All three
*are* identity.

**`shutdown` is the trap worth recording.** A scan for constant-return arms
flags it, because `sys_shutdown` has a `#[cfg(not(feature = "smoltcp"))]`
variant whose body is `0`. The shipping (smoltcp) build resolves an fd and calls
`socket_shutdown`. Classifying it `Leaf` on the strength of the stub would have
been wrong in every profile anyone runs. Check the arm you actually ship.

### 10.1 What the bypass buys

`mem_op_cost` gained a `getuid_leaf` arm next to `getpid`, and the pair is now a
permanent instrument rather than a one-off: both take no arguments and return one
integer, so **the gap between them is the prologue+epilogue cost**.

| arm | tier | ns (idle host, 3 runs) |
|---|---|---|
| `getpid` | `Full` | 152 / 134 / 134 — median **134** |
| `getuid_leaf` | `Leaf` | 110 / 100 / 98 — median **100** |

**~34 ns, ~25% of the call.** On four of the syscalls a libc startup calls most.

### 10.2 The conditional case — attempted, and reverted

The `aio` stubs are constant-return once the debug lookup is inside the gate, so
a Cargo feature looked like the clean answer: unlike a runtime flag it is resolved
at compile time, so `FastPath` membership stays constant within a build, which is
what the entry-vector change needs. `akuma-syscalls` gained a `debug-info`
feature and a `flat_when_untraced()` predicate admitting
`IO_SUBMIT`/`IO_CANCEL`/`IO_GETEVENTS`.

**It was wrong, and reverted.** The body is not the arm:

```rust
nr::IO_SUBMIT => aio::sys_io_submit(args[0], args[1] as i64, args[2]),
```

The *dispatch arm* reads `args[0..2]` regardless of what the callee does with
them, and [`takes_no_args`] is documented as a property of the arm. With the
`x0`–`x5` change landed, that dispatch would read unrestored registers — harmless
in effect, since the callee discards them, but the invariant would be false, and
an invariant that is false in a corner cannot carry an assembly change.

Every member that *is* admitted dispatches without touching `args`:
`nr::GETUID => 0`, `nr::GETEUID => proc::sys_geteuid()`,
`nr::AKUMA_GET_VERSION => version::sys_akuma_get_version()`.

Admitting the trio needs the dispatch arms made feature-conditional too — three
`#[cfg]` pairs and a per-feature signature, for three stubs. Deferred to 0.0.8. A
test (`the_aio_stubs_are_never_leaf_because_the_arm_reads_args`) guards against
re-adding the predicate without that.

The `syscall-debug-info` feature itself **stays** — it gates real console work in
`akuma-syscalls-time` and `aio.rs`, which was always its main job.

### 10.3 Where the flat share stands

| | flat | of 192 dispatched |
|---|---:|---:|
| before this work | 2 | **1.0%** |
| `--features syscall-debug-info` | 6 | 3.1% |
| **default (traces off)** | **9** | **4.7%** |

Nearly a 5x increase, and the ceiling is low by construction: criterion 2
("touches no `Process`, no process table, no fd table, no address space") is what
a syscall is normally *for*. The remaining four members of `takes_no_args` —
`getpid`, `getppid`, `gettid`, `sched_yield` — are all identity or scheduler by
definition and will never be admitted.

### 10.4 A pre-existing hazard this surfaced — FIXED

`takes_no_args` is documented as "the arm reads no element of `args`", and it is
the stated precondition for the entry-vector change that stops restoring
`x0`–`x5` for argument-less calls. **The generic prologue trace violated it.**
`[SC] nr=… a0=… a1=… a2=…` formats `args[0..3]` for every number not on
`debug_io_suppressed`, and that was a **noise** list — high-traffic calls — not an
argument list. `AKUMA_GET_VERSION`, the original `Leaf` and the floor control the
whole tier is measured against, was not on it; neither were the four uid/gid
additions. `uptime` was covered only by the luck of also being high-rate.

With the asm change landed and `SYSCALL_DEBUG_IO_ENABLED` on, that line would
print **stale register contents** for exactly the syscalls the optimisation
targets.

Fixed by making the predicate *start* from the contract rather than restate it:

```rust
pub const fn debug_io_suppressed(nr: u64) -> bool {
    takes_no_args(nr) || matches!(nr, /* the high-rate noise list */)
}
```

Structural, not a list to remember — and correct on its own merits well before
the asm change, since `a0=…` for an argument-less syscall was always meaningless.
Three tests pin it:

- `takes_no_args_implies_suppressed` — the implication, over every number.
- `every_leaf_is_suppressed` — the corollary for the tier the asm change targets.
- `the_noise_list_still_does_its_job` — that the widening did not swallow the
  original purpose: `read`/`write`/`mmap`/`futex` take arguments, so they can
  only still be suppressed via the `matches!`.

The independent oracle in `tests.rs` was updated to spell the new exclusions out
rather than delegate to the predicate — an oracle that calls the code it checks
is not an oracle.

## 11. The same audit inside `crates/`

Also in [`SYSCALL_TRACE_AUDIT.md`](SYSCALL_TRACE_AUDIT.md), including the two
things the scan got wrong before it got them right — crate gating idioms, and the
`log` facade, which was missed entirely on the first pass and hid the worst
instance found.

Two results from it belong here rather than there, because they are about method:

- **The leaf-most crates on the memory path emit nothing at all.** `akuma-mmap`
  and `akuma-syscalls-mem` have **0 print sites** — the first because it is
  dependency-free by construction, the second because it forbids `unsafe` and has
  no console dependency. A crate that cannot reach the console cannot have this
  bug, which is an argument for the extraction programme independent of testing.
- **Most ungated prints in `crates/` are anomaly detectors and must stay that
  way.** `[PMM-UAF]`, `[KTG-STALE]`, `[TRAMP-MISMATCH]` fire when an invariant is
  already violated. "Ungated" is not a synonym for "wrong"; the question is
  always whether *userspace* chooses the trigger.

## 12. How to re-measure



```bash
userspace/memprobe/c/build.sh --push-akuma 2322
ssh -p 2322 root@localhost /tmp/mem_op_cost 30 500 1
```

`mem_op_cost` now flags any arm more than **50× the floor** with
`<-- >50x floor: something other than the named op dominates`. That marker
exists because of this investigation: a probe reporting a plausible number for
the wrong reason is worse than one that crashes.

**`madv_unmapped` is the case that shows the threshold is not too tight.** It
reads 2,092 ns — 13× the floor, the second-loudest arm in the table — and it is
clean. Checked three ways:

- Its only console statement, `[DONTNEED-FILE]`, is gated on
  `!file_vas.is_empty()`, and the arm walks an *anonymous* `PROT_NONE`
  reservation, so the gate never opens. **0 such lines** in the boot log across
  the run.
- 2,092 ns over `RESERVE_BYTES / 4096` = 64 pages is **32.7 ns/page** — a
  `translate` plus a `cow_ref_get` each, which is what the per-page rule costs.
- Arithmetically it could not hide one. A single 60-byte line costs ~144 µs at
  2.4 µs/byte — **69× the arm's entire measured time**. On this kernel, any arm
  containing a console write is not 13× the floor, it is four digits of it.

That gap is what makes the 50× threshold safe: real work and a console write are
two orders of magnitude apart, not adjacent.

**A naming trap worth knowing.** `tprint!` is the *timestamped* print — it
prefixes `[T{s}.{cs}]` and then writes every byte, exactly like `safe_print!`.
It is not throttled, and nothing in the tree is. There is no cheap console
print; the choice is whether to print at all.

## Background

- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §10.3.1–§10.3.2 — the probe
  work that surfaced this.
- [`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) — the `readlinkat`
  EINVAL flood that prompted the narrowing.
- [`UART_SMP_INTERLEAVE_FIX.md`](UART_SMP_INTERLEAVE_FIX.md) — why console
  writes serialise across cores.
- [`../reference/subsystems/console.md`](../reference/subsystems/console.md) —
  the printing rules.
