# Busybox Missing Syscalls

Status of running busybox applets on Akuma.

---

## `times` (NR 153) — FIXED

### Symptom

`busybox time <cmd>` reported wildly incorrect user and system times:

```
real    0m 9.06s
user    223916h 56m 32s
sys     38475748h 24m 48s
```

The same enormous `user`/`sys` values appeared for every command regardless of
actual runtime, and were identical across runs (the values came from uninitialized
or garbage memory, not from any real measurement).

### Root Cause

`times()` (syscall 153) was not implemented — it fell through to `ENOSYS`. musl's
`times()` returned `-1`, but `busybox time` treated the return value as a valid
monotonic tick count, then read the `struct tms` fields from whatever memory
happened to be at the pointer, producing nonsense values for user and sys time.

### Fix

Implemented `sys_times()` in `src/syscall.rs`:

- Zeroes the `struct tms` buffer (32 bytes: four `clock_t` fields at 8 bytes each).
  We don't track per-process CPU time, so `tms_utime`/`tms_stime`/`tms_cutime`/
  `tms_cstime` are all reported as zero.
- Returns elapsed uptime in USER_HZ ticks (100 ticks/second) as the function
  return value. This is what `busybox time` uses to compute "real" elapsed time.

### Remaining Limitation

The real time reported by `busybox time` for disk-I/O-heavy processes (e.g.,
`node`, `bun`) will still be significantly lower than wall-clock time. This is
because `CNTPCT_EL0` (the ARM physical timer counter) only advances while the
guest vCPU is actively executing. Time spent in QEMU VM exits for VirtIO block
device I/O is not counted. See `docs/LARGE_BINARY_LOAD_PERFORMANCE.md` for
details and potential fixes (larger block batches, page cache).

---

## Syscall Reference

| Syscall | NR | Status | Notes |
|---|---|---|---|
| `times` | 153 | Fixed | Returns zeroed `struct tms`; return value is correct uptime ticks |
