# Busybox Missing Syscalls

Status of running busybox applets on Akuma.

---

## `wait4` rusage pointer ignored — FIXED

### Symptom

`busybox time <cmd>` reported wildly incorrect user and system times:

```
real    0m 9.06s
user    223916h 56m 32s
sys     38475748h 24m 48s
```

The same enormous `user`/`sys` values appeared for every command regardless of
actual runtime, and were identical across successive runs of the same command
(pointing to fixed uninitialized memory, not accumulating measurement error).

### Root Cause

`busybox time` calls `wait3()`, which musl implements as
`wait4(-1, &status, options, &rusage)`. Akuma's `sys_wait4` accepted the rusage
pointer as its 4th argument but completely ignored it (the parameter was named
`_rusage`). The caller's `struct rusage` (144 bytes on AArch64) was never
written, so busybox read whatever happened to be on its own stack — producing
the same garbage values every run because the stack layout of the busybox `time`
applet is deterministic.

### Fix

`sys_wait4` now zero-fills the `struct rusage` when a non-null pointer is
provided:

```rust
const RUSAGE_SIZE: usize = 144; // struct rusage on aarch64: 18 × 8-byte fields
if rusage_ptr != 0 && validate_user_ptr(rusage_ptr, RUSAGE_SIZE) {
    unsafe { core::ptr::write_bytes(rusage_ptr as *mut u8, 0, RUSAGE_SIZE); }
}
```

We don't track per-process CPU time, so zeroes are the honest answer. After the
fix:

```
real    0m 2.80s
user    0m 0.00s
sys     0m 0.00s
```

---

## `times` (NR 153) — IMPLEMENTED

### Background

`times()` was not implemented — it fell through to `ENOSYS`. While this was not
the cause of the garbage user/sys output (that was the `wait4` issue above), an
unimplemented `times()` causes other problems: any program that calls it to
measure elapsed time gets `-1` back and may misinterpret it.

### Fix

Implemented `sys_times()` in `src/syscall.rs`:

- Zeroes the `struct tms` buffer (32 bytes: four `clock_t` fields at 8 bytes
  each on AArch64). We don't track per-process CPU time, so
  `tms_utime`/`tms_stime`/`tms_cutime`/`tms_cstime` are reported as zero.
- Returns elapsed uptime in USER_HZ ticks (100 ticks/second) as the function
  return value, which is the POSIX-required monotonic clock for the return value
  of `times()`.

---

## Observed timings after fixes

```
busybox time node --version   →  real 0m 2.80s,  user 0m 0.00s,  sys 0m 0.00s
busybox time bun --version    →  real 0m 58.28s, user 0m 0.00s,  sys 0m 0.00s
busybox time hello (10×1s)    →  real 0m 9.06s,  user 0m 0.00s,  sys 0m 0.00s
```

### Why "real" is lower than wall-clock time for node/bun

`CNTPCT_EL0` (the ARM physical timer counter used by `clock_gettime` and
`times()`) only advances while the guest vCPU is actively executing. Time spent
inside QEMU handling VirtIO MMIO exits (disk reads, device kicks) is invisible
to the guest timer. For a sleep-bound workload like `hello`, real ≈ wall-clock.
For disk-I/O-heavy binaries like `node` (47 MB) and `bun` (90 MB), most of the
wall-clock time is in QEMU, not the guest:

- `node --version` reported 2.80s but took ~30s wall-clock — ~90% of time in
  QEMU VM exits for VirtIO block reads.
- `bun --version` reported 58.28s — bun does JSC JIT compilation at startup
  (pure guest CPU work, fully counted), plus 2× the disk I/O of node.

See `docs/LARGE_BINARY_LOAD_PERFORMANCE.md` for potential fixes (larger block
read batches, page cache).

---

## Syscall reference

| Syscall | NR | Status | Notes |
|---|---|---|---|
| `wait4` rusage | 260 | Fixed | Now zero-fills `struct rusage` instead of leaving it uninitialized |
| `times` | 153 | Implemented | Returns zeroed `struct tms`; return value is correct uptime ticks |
