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
load-bearing had been dead since the narrowing. Anyone re-enabling this gets less
than the source appears to offer. **Fix that before trusting it.**

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

## 8. How to re-measure

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
