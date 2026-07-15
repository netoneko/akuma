# time syscalls

clock_gettime / clock_getres / nanosleep / times / getrusage / time / uptime.
Source: `src/syscall/time.rs`. For the blocking pattern `sys_nanosleep` uses
see [`../scheduler.md`](../scheduler.md) "Blocking & wait/wake".

> **Stability: A (stable).** The Go forktest clock-corruption bug (Mar–May
> 2026) is resolved and quiet since. The recurring lesson: **a leaked GPR
> looks exactly like a valid syscall argument** — Go's `nanotime1` left
> `x8=113` (clock_gettime's own syscall number) in the register file, and a
> QEMU DC-ZVA misroute replayed it with a heap pointer in `x0` as the
> `clock_id`; only a plausibility bound on the argument caught it.

## clock_gettime / clock_getres

`sys_clock_gettime` (`time.rs:11`, syscall 113) rejects `clock_id_arg >
0x1000_0000` with `EINVAL` **before** touching `tp_ptr` — a genuine Linux
clock_id is a small integer or a compact CPU-clock encoding
(`~(pid << 3) | CPUCLOCK_*`), never pointer-sized. This guard exists because
a pointer-sized value here is corruption (see Background): copying out a
timespec to a "clock_id" that's actually a stray pointer produced a
WILD-DA at `FAR=0x10`. On a large `clock_id`, the handler also decodes the
4 bytes before/after the trap-frame ELR and logs them (`time.rs:19-38`) to
identify which caller left the leaked register — pure diagnostics, no
behavioural effect.

Once past the guard: `clock_id == 0` (`CLOCK_REALTIME`) reads
`timer::utc_time_us()`; anything else falls back to `timer::uptime_us()`
(i.e. every non-zero clock ID is treated as monotonic). `sys_clock_getres`
(`time.rs:60`) always reports 1 ns resolution and ignores `clock_id`
entirely.

## nanosleep

`sys_nanosleep` (`time.rs:69`) accepts **two ABIs** on the same syscall
number: Linux/musl's `struct timespec *` in `a0`, or libakuma's raw
`(seconds, nanoseconds)` pair in `a0`/`a1`. It disambiguates by treating
`a0 >= PAGE_SIZE` as "looks like a user pointer" and attempting
`copy_from_user_safe`; a copy failure falls back to treating `a0`/`a1` as
raw values instead of faulting. Sleeping itself is the standard blocking
pattern: compute a deadline, then loop `schedule_blocking(deadline)` until
the deadline passes or `is_current_interrupted()` returns `EINTR`.

## times / getrusage

`sys_times` (`time.rs:94`) and `sys_getrusage` (`time.rs:105`) both zero-fill
their output struct (`struct tms` 32 B, `struct rusage` 144 B) rather than
tracking real per-process CPU accounting — good enough for callers that only
check the syscall succeeds. `sys_times` return value (`uptime_us / 10_000`,
i.e. clock ticks) is otherwise real.

## time / uptime

`sys_time()` and `sys_uptime()` (`time.rs:114-116`) are one-line wrappers
around `timer::utc_time_us()` / `timer::uptime_us()` — no argument
validation needed since neither takes a pointer.

## Background

- `archive/GO_FORKTEST_DEBUG.md` — the `clock_gettime` leaked-x8 /
  errno-as-pointer investigation (`nr=113` crash signatures, the
  `FAR=0xfffffffffffffffa` family).
