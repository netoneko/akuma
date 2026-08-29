# Userspace-drivable diagnostics on syscall paths: every instance

**Date: 2026-08-29.** The catalogue. The *method* — why a console line costs
~2,400 ns/byte, how that was measured and fitted — is
[`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md); this file is the list of places the
pattern was found, what each one cost, and what was done about it.

## The pattern

> A diagnostic on a syscall path, emitted unconditionally (or gated on a flag
> that ships on), whose trigger condition **userspace chooses**.

Three properties together make it a bug rather than a debug aid:

1. **The console is ~2,400 ns per byte.** One `write_volatile` per byte to the
   PL011 data register, each trapping out of the guest. A 103-byte line is
   ~250 µs against a ~150 ns syscall.
2. **Userspace picks the trigger.** A bad `clock_id`, an out-of-range address, an
   unsupported `fcntl` cmd — all arguments, all loopable.
3. **Console writes serialise across cores** (`kernel_console_lock`), so one
   unprivileged process degrades the whole system.

The tell in a measurement: the arm reads hundreds of times the syscall floor
while `[PSTATS]` reports almost no kernel time for it. The cost is the UART, not
the code.

## What was found

### Class A — turned off

| where | trigger | cost | disposition |
|---|---|---|---|
| `syscall/mod.rs` epilogue `[EFAULT]` | any EFAULT-returning syscall | **249,806 → 150 ns** (measured) | `SYSCALL_ERRNO_DIAG_ENABLED = false` |

The headline case, and the one that started the audit. 99.94% of an
EFAULT-returning call. Full writeup in `CONSOLE_LOG_COST.md` §1–§6.

### Class B — gated behind a feature, because gating changed a classification

| where | trigger | cost | disposition |
|---|---|---|---|
| `akuma-syscalls-time` `[clock-diag]` | `clock_gettime(clock_id > 0x1000_0000)` | ELR read + **2 user-memory reads** + ~130-byte 2-line `log::warn!` (~310 µs predicted from the fitted model) | `akuma-syscalls-time/debug-info`, off by default |
| `syscall/aio.rs` ×3 stubs | every `io_submit`/`io_cancel`/`io_getevents` | IRQ mask + `AIO_CONTEXTS.lock()` + map probe **per call**, purely to choose a string | `syscall-debug-info` feature; the three arms are now `FastPath::Leaf` when it is off |

`[clock-diag]` is the worst single instance found — worse than the EFAULT case,
because it also performs two user reads before printing. It reached the audit
only because the scan was widened to the `log` facade (§"What the scan missed").

### Class C — gated behind an existing subsystem flag

25 sites in `src/syscall/`, all reachable in a loop from userspace, none of which
had any gate. Flags chosen from what already existed; no new knobs.

| sites | where | trigger | flag |
|---:|---|---|---|
| 8 | `aio.rs` | every AIO call, success paths included | `SYSCALL_DEBUG_INFO_ENABLED` |
| 4 | `msgqueue.rs` | every `msgget`/`msgctl` | `SYSCALL_DEBUG_INFO_ENABLED` |
| 4 | `mem.rs` | `[mmap] REJECT` ×2, eager-OOM fallbacks ×2 | `MEM_SYSCALL_TRACE_ENABLED` |
| 3 | `fs.rs` | every `renameat`; unsupported `fcntl` cmd; pipe-write `EPIPE` | `SYSCALL_DEBUG_INFO_ENABLED` |
| 1 | `timerfd.rs` | every `timerfd_settime` | `SYSCALL_DEBUG_INFO_ENABLED` |
| 1 | `pidfd.rs` | every `pidfd_open` | `SYSCALL_DEBUG_INFO_ENABLED` |
| 1 | `proc.rs` | unsupported `prctl` option | `SYSCALL_DEBUG_INFO_ENABLED` |
| 1 | `net.rs` | unsupported `socketpair` domain | `SYSCALL_DEBUG_NET_ENABLED` |
| 1 | `pipe.rs` | pipe-write on a missing pipe | `PIPE_TRACE_ENABLED` |
| 1 | `mod.rs` | `copy_from_user_str` on invalid UTF-8 | `SYSCALL_DEBUG_INFO_ENABLED` |
| 1 | `akuma-exec` threading | **every thread recycle** — a `-j4` build recycles constantly | `lifecycle_trace_on()` |

### Class D — not a print, same shape

Found by asking the same question one layer down: *is anything computed for the
diagnostic that the syscall does not otherwise need?*

| where | what | result |
|---|---|---|
| `syscall/aio.rs` ×3 | `with_irqs_disabled(\|\| AIO_CONTEXTS.lock().contains_key(&ctx))` computed **outside** the gate, consumed only by the message. All three stubs return 0 regardless. | Moved inside the gate; the stubs are now `return 0` and nothing else |
| `syscall/mem.rs` `madvise_dontneed_range` | `lazy_region_lookup_for_pid(proc.tgid, va)` **per page** — a process-table walk + IRQ mask + spinlock each, to read one map that never changed and re-find a `Process` the caller already held | One lock for the range. **30.1× → 4.8× `getpid`** (median, loaded host); 2092 → 622 ns idle |

Gating the `print!` alone would have left every bit of that work in place. **The
rule: gate the argument, not just the print.**

## Deliberately left unconditional

Not everything ungated is a bug, and treating it that way would be its own kind
of damage.

| what | why it stays |
|---|---|
| `akuma-pmm` ×7 — `[PMM-UAF]`, `[PMM-PREMATURE]`, `[PMM-QUAR-DF]`, `[PMM WARN] Double allocation` | Corruption detectors. They fire when memory is *already* wrong, and they are what the archive's premature-free hunts were solved with |
| `akuma-exec` ×29 — `[KTG-STALE]`, `[PROC-ORPHAN]`, `[TRAMP-MISMATCH]`, `[RUN-REFUSED]`, stale-tid warnings | Same: invariant-violation reports, not progress logs |
| `[SGI-DBG]`, `[SGI] POOL contended` | Already correct — a one-shot and a rate limiter (`n.is_multiple_of(1000)`) respectively |
| `[PTLOCK]`, `[FILL-SHORT/prefault]` | Already conditional — a hold-duration threshold and a short-read check |
| kernel-test `[PASS]`/`[FAIL]`, `[PSTATS]`, the read-path profiler | Not userspace-drivable |

## What the scan missed, twice

Both misses are worth recording, because both produced a confident wrong number.

**1. Crate gating idioms.** A first pass grepping `config::UPPER_CASE` reported
**130** ungated sites in `crates/`. Crates cannot see `src/config.rs` — they gate
through `config().syscall_debug_info_enabled`, `lifecycle_trace()`, a local
`if debug`, an `_ENABLED` const, a threshold, or a rate limiter. Correcting the
pattern: **69**.

**2. The `log` facade.** The scan looked for `safe_print!`, `tprint!`,
`console::print` and `print_str` — and missed `log::*` entirely. There are **56**
invocations, and `src/klog.rs` has installed a real logger since 2026-08-21, so
they reach the same UART.

That one resolved better than feared: **52 of the 56 are `log::debug!`**, which
the logger rejects (`enabled()` is `level <= Info`) and which the `log` macros
short-circuit before formatting — an atomic load and a compare, no console write.
Only **4 emit**, of which two are comments and one is an exit-path anomaly
(`[ktg] grace expired`). The fourth was `[clock-diag]`, the worst instance in this
document.

**The lesson is not "grep harder".** It is that "how does this code emit to the
console" is a question with more than one right answer in this tree, and a scan
that assumes one idiom will report a clean bill of health for the paths that use
another.

## Follow-on: categorising syscalls by what they lock

The natural next question after "what does this syscall print" is "what does it
lock" — same shape, and a lock taxonomy would feed another bypass tier the way
the print audit fed `FastPath::Leaf`. A first pass, recorded here because it is a
**screen, not a clearance**, and because it got two things wrong before it got
them right.

### The screen

Each `sys_*` entry point was scanned for nine lock families (`bkl`, `as_lock`,
`vm_lock`, `lazy_regions`, `proc_table`, `pmm`, `fd_table`, `thread_pool`,
named global mutexes), transitively to depth 3 over a name-resolved call graph.

| lock families touched | entry points (of 146) |
|---:|---:|
| 0 | 6 |
| 1 | 2 |
| 3–5 | 28 |
| 6–7 | 34 |
| **8–9** | **76** |

**Direct-body analysis is worthless here** and the first run proved it: it put
`sys_clone`, `sys_execve` and `sys_close` in the "0 families" bucket, which is
absurd — fork obviously locks. Only the transitive pass produces a usable answer,
and even it under-approximates (no method resolution, no trait dispatch).

### What it found

Zero families, transitively: `akuma_get_version`, `core_init`, `geteuid`,
`getgroups`, `prlimit64`, `set_tpidr_el0`. Exactly one: `rt_sigprocmask`,
`sigaltstack` (both `thread_pool`).

The useful cross-check: **`akuma_get_version` and `rt_sigprocmask` are already on
the hand-audited `SYSCALL_BKL_OPTOUT_SEED` list**, which the screen did not know
about. Independently rediscovering two of sixteen from the clean end is the
evidence that the method is sound. The other six are **candidates** — the real
criterion in `smp_shared.rs`'s tranche comments includes a blocking-window
analysis ("does it carry a `Process`-derived reference across a wait?") that a
static family scan cannot do.

### And a correction worth recording

A first count reported "**64% of syscalls reach the BKL**". That is wrong, and
wrong in the flattering direction for the wrong reason:

- The BKL is **acquired in exactly one place** — `bkl::enter_kernel()` in the EL0
  trap path, skipped when `syscall_bkl_optout(x8)`.
- The 56 `BklGuard::new` sites the scan counted are `MmBklGuard`/`VfsBklGuard`/
  `NetBklGuard` — **carve-out windows that DROP the lock**. Counting them as
  acquisitions inverts their meaning: they are precisely where the BKL is *not*
  held.

The honest taxonomy:

| | count | of 192 |
|---|---:|---:|
| skip the BKL entirely at trap entry (opt-out list) | 16 | **8%** |
| take it, then drop it for a carve-out window | 39 entry points, 5 files | |
| acquisition sites in the whole tree | **1** | |

So "the BKL is almost gone" is right about *held time* and wrong about *entry
count*: ~92% still take it on the way in, but a large share hand it straight back
for most of their body. Those are different claims and the distinction is exactly
what the carve-out phases have been buying. Anyone quoting a BKL number should
say which one they mean.

## Verification

Every change here is behaviour-preserving with the flags in their shipping
(default-off) state, so the gate is that nothing moved:

- 1008 host tests; clippy clean on release, release + `syscall-debug-info`,
  rump devbox and `extreme-size`.
- `scripts/mem_suite.py` 10/10 probes, 3 DIVERGE.
- Boot suite 316 PASSED / 0 failed on QEMU.
- `id -u` / `id -g` return 0 through the real ABI (the uid/gid arms became
  `FastPath::Leaf` in the same work).

## Background

- [`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) — the cost model, its measurement,
  and the `FastPath::Leaf` work the audit fed.
- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §10.3 — the probe arms that
  surfaced the first instance.
- [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)
  § "Method warnings" — why ratios, not nanoseconds.
