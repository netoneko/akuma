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

### The remaining crates, inspected

`akuma-net` (13), `akuma-virtio` (15), `akuma-primitives` (4) and `akuma-ext2` (1)
were counted in the first sweep but not read. They have now been read
individually, and **none warrants gating**:

| crate | what the ungated sites are |
|---|---|
| `akuma-net` | `[SmolNet] Initialized` / `DHCP configured` / `IP:` / `DHCP deconfigured` — boot and lease-change lifecycle, one-shot or rare. Plus `[NET] CORRUPT HANDLE` ×3, which is a detector: *"a corrupted async state machine could overwrite `handle_index` with garbage; catch it here instead of panicking inside smoltcp"* |
| `akuma-virtio` | `[SND]`, `[Block]`, `[virtio] slot N`, `[RNG] Found virtio-rng at slot` — all inside `probe::scan()` device enumeration, boot-time only |
| `akuma-primitives` | `[WATCHDOG] preemption disabled Nms` — gated on `duration >= PREEMPTION_WATCHDOG_PANIC_US`; the rest is `console.rs`'s own plumbing and a unit test |
| `akuma-ext2` | `[E2-EOF]` — **already rate-limited**: `if prev < 32`, behind an `E2_READ_AT_EOF` counter, and only for `offset > file_size` (an ordinary read *at* EOF is deliberately silent and uncounted) |

Other console mechanisms were swept too, not just the macros: direct
`StackWriter` uses, `print_args`/`print_args_if_registered`, and every `.flush()`.
They resolve to `src/klog.rs` (the logger), boot/heartbeat paths in `main.rs`,
`[BKL] stuck`/`[BKL] RECOVERED` (anomaly), and
`log_memory_stats_on_crash` — which runs immediately before `loop { wfe }`, i.e.
once, at a kernel halt.

**The audit is closed.** Every remaining ungated console path in the tree is a
crash handler, a corruption detector, a boot/device probe, a watchdog threshold,
or already rate-limited. That is a measured statement, not an untested
assumption — which is the difference between this line and the one the first
sweep would have supported.

> **Correction, 2026-08-30.** That claim was wrong when written, and not by one
> instance. Reading the serial console of an in-VM `cargo build` turned up **seven**
> ungated, userspace-drivable families, none of them a crash handler, detector,
> probe, watchdog or rate-limited line: `execve` argv, `[TERM]`, the four
> `[signal]`/`[sigreturn]` delivery lines, `[KTG]`, `[pipe] DESTROY`, the `execve`
> PATH-probe miss, and `[FS] read_file`. Measured A/B on a real guest build, they
> were **91.9% of the console log** — see
> [`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) §13 for the numbers, the new
> `SIGNAL_TRACE_ENABLED` flag, and the two anti-patterns they exposed (a
> rate-limit budget is not a gate; a substring filter is not a gate, and
> `path.contains("git")` matched every path under a `github.com` checkout).
>
> Two distinctions this correction does NOT overturn: `[KTG]` (a progress line,
> now gated) is not `[KTG-STALE]`/`[KTG-STALE-CH]` (invariant tripwires, still
> ungated and correctly so), and `[HEAP]`/`[PSTATS]` stay ungated because they are
> periodic, not per-call.
>
> Read "closed" as "closed against the idioms it scanned for" — a weaker statement
> than the one this paragraph made.

## What the scan missed, three times

All three misses are worth recording, because each produced a confident wrong
number or a confident wrong all-clear.

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

**3. `sys_execve`'s own argv trace — found 2026-08-30, gated the same day.**
`src/syscall/proc.rs` printed

```rust
crate::tprint!(2048, "[syscall] execve(path=\"{}\", args={:?}) PID {}\n", ...)
```

**ungated**, one line per `execve`, with the buffer deliberately widened to 2048
bytes so a linker's full argv survived. This one is not an idiom gap: it is a
`tprint!` in `src/syscall/`, the exact macro and the exact directory the scan
covered, and it appears in none of Classes A–D nor in the deliberately-unconditional
table above. The function's *other* execve trace, twelve lines earlier, was already
behind `SYSCALL_DEBUG_INFO_ENABLED` — which is likely why a reader scanning
`sys_execve` saw a gate and moved on.

Cost, predicted from this document's own fitted ~2.4 µs/byte: a `cargo` build's
`rustc` command lines run several hundred bytes once `--extern` and `--check-cfg`
are expanded, so **~0.7–1.2 ms of serial write per compilation unit**, plus the
same again for each `cc`/`ld` exec. That is why an in-VM build's console is
unreadable, and it is a per-`execve` tax paid by every build, not a rare path.

Disposition: wrapped in `if crate::config::SYSCALL_DEBUG_INFO_ENABLED` — Class C
treatment, no new knob, and the pre-existing `[PROC-EXIT]` gate (2026-08-29) uses
the same flag, so the pid-correlation workflow the two lines exist for still works
in a `syscall-debug-info` build. `scripts/bkl_smp_regimen/analyze_workload.py`
parses this line and now carries a note that it needs such a build.

Measured after the fact, it was **44.4% of the console log** of a guest `cargo
build` — the single largest family, and six more followed it out of the same log.
[`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) §13 has the full A/B.

**What this one adds to the lesson:** misses 1 and 2 were scans looking in the
wrong place. This was a scan looking in exactly the right place at a function
that *contained a gate* — and a gate in the body is not evidence that the body is
gated.

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

## Are the gates themselves features?

Asked after the fact, and the answer was **mixed** — three different mechanisms
were in play, and only one of them mattered.

| gate | kind | codegen when off |
|---|---|---|
| `SYSCALL_DEBUG_INFO_ENABLED` (~19 sites) | `cfg!(feature = "syscall-debug-info")` | eliminated |
| `MEM_SYSCALL_TRACE_ENABLED` (4), `SYSCALL_DEBUG_NET_ENABLED` (1), `PIPE_TRACE_ENABLED` (1) | plain `const bool = false` | eliminated |
| `lifecycle_trace_on()` in `akuma-exec` (43 sites) | **runtime** — `config().syscall_debug_info_enabled` | **kept** |

The middle row is worth stating plainly rather than "fixing": a
`const bool = false` is dead-code-eliminated exactly as a feature is. The only
difference is ergonomic — a feature flips from the command line, a const needs a
source edit. There is no cost argument for converting those six.

The third row was different in kind. A runtime field read cannot fold, so all 43
call sites kept their format strings in `.rodata`, their formatting code in
`.text`, and paid a load and a branch each, in **every** build including
`extreme-size`. Fixed by putting the compile-time test first so the whole
expression folds:

```rust
pub(crate) fn lifecycle_trace_on() -> bool {
    cfg!(feature = "debug-info") && config().syscall_debug_info_enabled
}
```

With the feature on the runtime toggle still works, so a build can compile the
traces in and still switch them off.

### Measured

Matched A/B, same command, same profile:

| | baseline | folded | delta |
|---|---:|---:|---:|
| `release` `.text` | 2,832,240 | 2,827,780 | **−4,460** |
| `release` `.rodata` | 434,976 | 433,288 | **−1,688** |
| `release` total | 4,507,966 | 4,501,818 | −6,148 (−0.14%) |
| `extreme-size` `.text` | 536,376 | 534,648 | −1,728 |
| `extreme-size` `.rodata` | 75,168 | 73,584 | **−1,584 (−2.1%)** |
| `extreme-size` total | 1,015,642 | 1,012,330 | −3,312 |

The `.rodata` delta is the check on the whole measurement: the format strings
behind that gate were counted at **~1,617 bytes**, and `release` `.rodata` fell
by **1,688**. Within 4% of the prediction, which is what says the number is the
traces and not build noise.

**A measurement trap, hit and recorded.** The first attempt read
`.text 2,283,224` for the baseline and `2,827,780` after — a *544 KB increase*
from deleting code. The "baseline" was a stale binary left in the target
directory by the preceding `cargo clippy` runs, which use different feature sets.
`cargo clippy` does not rebuild the final binary, so the ELF on disk belonged to
some other configuration entirely. Both arms must be built with the same explicit
command, back to back, before either number is read — the same rule
`AKUMA_EXTRACT_MMAP.md` §10.3 records for `akuma.bin`.

## A false FAIL the suite produced, and the fix

`scripts/mem_suite.py`'s headline property is that it **refuses to score a silent
probe as a pass** — output must exist, or it fails. On 2026-08-29 that fired on
`smapsdirty`: `SILENT (rc=0) — probe printed nothing`. Run by hand immediately
after, the same binary on the same VM produced full output 3/3. The ssh
round-trip had dropped it.

The guard was right to refuse — but "probe died" and "transport hiccup" need
opposite verdicts and are indistinguishable from one sample. The suite now
retries **once, and only on SILENT**:

- Silent twice still fails, so the no-silent-pass rule is intact.
- A `FAIL` line or a bad exit code is *never* retried, so a probe cannot pass by
  being run until it gets lucky.

Also seen and worth writing down: `mprotectlb` failed once in the same run and
then passed 4/4 on a fresh boot. Between them, two of the ten probes produced a
spurious failure in a single suite run. **A red probe is worth one re-run before
it is worth an investigation** — but only one, and only for the flavours above.

## Verification: both platforms, and against Linux

Run 2026-08-29 on an idle host. Baseline is `f49ca08f` — the `akuma-syscalls-mem`
checkpoint, before any of the print or lock work.

### No regressions

| platform | result |
|---|---|
| QEMU, `SMP=4` (6 boots across two rounds) | **313–316 PASSED, 0 FAILED** every boot |
| Firecracker under Lima (KVM, nested virt) | **307 PASSED, 0 FAILED, 0 POISON** |
| `scripts/mem_suite.py` | **10/10 probes, 3 DIVERGE** (one run in six showed 9/10 — `cowstale`, the known flake) |
| in-VM `cargo build --release` of Akuma, devbox-smoltcp | **completes, 0 SIGSEGV** |
| clean builds after `cargo clean` (×2 rounds, 4 targets each) | all OK — release 14s, extreme-size 16s, devbox-smoltcp 5s, firecracker 8s |
| `test_mmap_madvise_hostile_length_is_refused` | PASS on both platforms |
| `[EFAULT]` / `[FORK-DBG]` / `[TRAMP]` / `[Cleanup] Thread` lines in a boot | **0** |
| `akuma-fc.bin` | 3,426,536 → **3,414,248** bytes (−12,288) |

### Gains, and where they are

Ratios to each kernel's **own** `getpid`. Absolute nanoseconds are not comparable
across the two rightmost columns — Akuma runs under QEMU and the Linux baseline
under Apple's `vz` — which is exactly why the comparison is normalised.

| arm | Akuma before | Akuma now | Linux | vs before | vs Linux |
|---|---:|---:|---:|---:|---:|
| `mremap_efault` | **1895.14x** | **1.00x** | 1.18x | **−100%** | 0.85x |
| `madv_unmapped` | 13.94x | 4.38x | 1.61x | **−69%** | 2.72x |
| `munmap_noent` | 3.17x | 2.99x | 1.12x | −6% | 2.66x |
| `mprotect_noop` | 2.93x | 2.76x | 1.34x | −6% | 2.06x |
| `madv_willneed` | 1.30x | 1.24x | 1.46x | −4% | 0.85x |
| `brk_query` | 1.26x | 1.22x | 1.02x | −3% | 1.20x |
| `mmap_einval` | 1.01x | 1.00x | 1.18x | −1% | 0.85x |
| `mremap_inplace` | 0.99x | 0.97x | 1.30x | −1% | **0.75x** |
| `membarrier` | 0.99x | 0.94x | 0.97x | −5% | 0.97x |

Two real gains and no regressions. Everything in the −1% to −6% band is inside the
control's own boot-to-boot drift and should be read as "unchanged".

### What the Linux column says

- **The decode paths are at or better than Linux.** `mremap_inplace` 0.75x,
  `mmap_einval` / `mremap_efault` / `madv_willneed` 0.85x, `membarrier` 0.97x.
  That is the part these extractions touched, and it is not where the work is
  left.
- **The remaining gap is exactly the three arms that do real page work**:
  `munmap_noent` 2.66x, `madv_unmapped` 2.72x, `mprotect_noop` 2.06x. TLB
  maintenance, region bookkeeping and the per-page walk — not decode, not
  logging. That is where a next round belongs, and the `madv_unmapped` lock hoist
  (13.94x → 4.38x) is the shape of what is available: it is still 2.72x Linux.
- **Syscall floor**: Akuma 152 ns vs Linux 105 ns, **1.45x** — under different
  hypervisors, so treat it as an order-of-magnitude statement, not a score.
- **`FastPath::Leaf` has no Linux counterpart.** `getuid` costs **0.72x** Akuma's
  own `getpid`; on Linux the same call is 1.00x its own. The tier is a real
  structural advantage, not just recovered overhead.

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
