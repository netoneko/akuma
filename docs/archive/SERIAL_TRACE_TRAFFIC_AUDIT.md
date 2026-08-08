# The console was the bottleneck — serial trace traffic audit (2026-08-08)

Three per-event kernel traces were printing unconditionally. Under a parallel
workload they saturated the one shared UART and turned the console into the
system's throughput limit: an in-VM `-j4` self-host kernel build went from **not
completing in over an hour** to **green in 2m21s** once they were gated.

This was found by accident while verifying the fix in
[`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
§12 — the build campaign that fix needed as an oracle could not produce a single
completed build.

---

## 1. What it cost

| | before | after |
| --- | --- | --- |
| serial output during `-j4` build | **~270 KB/s** | **7.6 KB/s** (35× less) |
| console log, one build round | 47 MB in 4 min (68 MB by 20 min) | 960 KB for the whole round |
| clean 101-crate `-j4` self-host build | >1 h, never observed to finish | **2m21s, `EXIT=0`** |

The build is not 25× faster because the CPU was busy printing. It is faster
because every core that logs takes the console lock, so four cores compiling in
parallel serialise against each other on a device that moves bytes at a fixed
rate. That is a lock-contention cliff, not a formatting cost.

## 2. The three traces

| trace | lines in one `-j4` sample | site | now gated by |
| --- | --- | ---: | --- |
| `[IA-DP] file region:` | **34,731** | `src/exceptions.rs` (instruction-abort demand page) | `DEMAND_PAGE_LOG_ENABLED` |
| `[pipe] create` / `clone_ref` / `close_write` / `close_read` | **6,626** | `src/syscall/pipe.rs` | `PIPE_TRACE_ENABLED` (new) |
| `[mmap]` / `[mprotect]`, one line per call | unbounded (per-syscall) | `src/syscall/mem.rs` | `MEM_SYSCALL_TRACE_ENABLED` (new) |

All three default to `false`. `munmap` was already gated (`TRACE_MUNMAP`) and is
the pattern the others now follow.

### 2.1 `DEMAND_PAGE_LOG_ENABLED` was a dead flag

The const existed and was documented, and **had no reader anywhere in the tree**:

```
$ git grep -c DEMAND_PAGE_LOG_ENABLED f9ef0b7 --
f9ef0b7:docs/reference/subsystems/config-flags.md:1
f9ef0b7:src/config.rs:1
```

Two occurrences: the definition and its own documentation row. It gated nothing,
while the line it should have been gating was the single largest source of serial
traffic in the system. The docs row also pointed at `config.rs:268` for a const
that lives at 322.

Its docstring claimed to gate `[DA-DP]` / `[DP]` / `[DP-eager]`. Those lines do
exist — but every one of them is an **anomaly** line (readahead pool exhausted,
single-page fallback OOM, anon alloc failed, lazy/eager region miss, fault in the
kernel VA range). Wiring the flag to those would have been the wrong fix, because:

> **Gate success-path per-event traces. Never gate anomaly lines.**
> A trace that fires once per successful operation is a throughput problem and
> belongs behind a flag. A line that fires when something went wrong is why you
> have logs at all, and must not be switchable off. Every gate added here follows
> that split: `[pipe] WARN` and `DESTROY`, `[mprotect] EINVAL`, and the whole
> `[DA-DP]`/`[DP]` family stay unconditional.

## 3. How to measure it

Line-type histogram of a console log — the first thing to run when a VM feels slow
under load:

```bash
grep -ao "\[[A-Za-z-]*\]" console.log | sort | uniq -c | sort -rn | head -12
```

Growth rate, which is the number that actually matters:

```python
a = os.path.getsize(p); time.sleep(60); b = os.path.getsize(p)
print((b - a) / 60 / 1024, "KB/s")
```

Anything above a few KB/s under load deserves a look. For scale: 270 KB/s is
roughly a 115200-baud line saturated ~20× over.

## 4. Diagnostic trap this created

**A wedged VM prints nothing, so a chatty VM and a dead one are distinguishable
only by the log growing.** During this session a poll loop waited an **hour** on a
VM that had wedged 60 seconds in, because it tested ssh reachability — and ssh
also fails on a VM that is merely starved by console traffic, so the two were
indistinguishable from outside.

Liveness check that works: the kernel prints `PSTATS` every 30 s, so watch the
console log's **size**, not its contents, and not the ssh port:

```python
size = os.path.getsize(console)
time.sleep(240)
alive = os.path.getsize(console) != size
```

`scripts/`-side harnesses that run long in-VM builds should use this rather than
an ssh probe.

## 5. Follow-ups not done here

- Nothing audits this. A boot-suite or CI check that fails when a `-j4` build's
  console exceeds some KB/s would keep the next unconditional trace from landing.
- The remaining traffic is `[TERM]` (1.7k), `[Cleanup]` (920), `[signal]` (492) per
  build. Small next to what was removed, but all three are per-event and none is
  gated.
- `[BKL] stuck tag=511` storms print thousands of identical lines with no rate
  limit, which makes a storming VM's log useless and probably makes the storm
  worse. See [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md) §12.7.

## Background

- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  §12 — the fix whose verification campaign surfaced all of this.
- [`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md)
  — the flag rows, with defaults and source lines.
- [`../reference/subsystems/console.md`](../reference/subsystems/console.md) — the
  console/UART path itself.
