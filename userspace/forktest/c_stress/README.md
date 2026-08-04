# c_stress — C-only mmap/fault control binaries

Pure musl static ELFs (no Go runtime), so a failure is unambiguously the kernel's.

- `mmap_stress` — anon mmap/memset/munmap churn; mirrors `forktest_child`
  `runMmapStress` so you can tell kernel mmap faults from Go allocator bugs.
- `mmap_file` — file-backed mmap + touch every page (demand-paging/readahead driver;
  proves an over-RAM file SIGSEGVs the process, not the kernel).
- `mmapsum` — content integrity of file-backed mmap vs `read()`: hashes the same file
  via read, two mmap passes, an `madvise(MADV_WILLNEED)` pre-faulted mapping, and a
  2-thread concurrent mapping. The `madv:` line is the regression check for the
  2026-07-25 bug where WILLNEED installed ZEROED frames over file-backed lazy pages
  (llama.cpp garbage-with-mmap).
- `fpfault` — FP/NEON register integrity across demand faults (all 32 Q regs
  canaried over every faulting touch).
- `neonfault` — data integrity of NEON loads that cross a page boundary into an
  unmapped demand-paged page (the quantized-GEMM access shape).
- `futextest` — pthread/futex behaviour in 7 phases (spawn+join, tight
  spawn/join loop, fan-out, mutex+condvar, barrier, wake-before-wait,
  park/unpark). Pure-C control for `userspace/selfhost_repro/futextest.rs`:
  run both to tell a kernel-level thread/futex bug (fails in *both*) from a
  Rust-runtime one (fails only in the Rust binary). Phase 2 is the regression
  test for the 2026-08-03 `clone_thread` slot-reclaim fix.
- `futexops` — probes `sys_futex` op-by-op against Linux semantics
  (`FUTEX_WAKE_OP`'s `uaddr2` write and second wake, `WAKE_BITSET`
  selectivity, a bad `timeout` pointer, and a requeued waiter that times out).
  Prints PASS/FAIL per probe. **Calibrate it by running the same binary on
  real Linux** — every FAIL there means the probe is wrong, not the kernel:
  `docker run --rm --platform linux/arm64 -v "$PWD/futexops:/futexops:ro" alpine /futexops`.
  As of 2026-08-03: 5 FAIL on Akuma, 5 PASS on Linux — see
  `docs/reference/subsystems/syscalls/sync.md` §"Known divergences from Linux".
- `futexkey` — does a futex key leak between address spaces? Forks a waiter that
  parks on a `.bss` global, then issues the wake **from the parent**, i.e. a
  different address space at the identical VA (no ASLR). A correct kernel wakes
  0; a kernel keying by virtual address alone reports `woken=1` and has just
  stolen another process's wake. Deterministic — one fork, one wake, no stress
  loop — which is the point: the "8 concurrent copies of `futextest_rs`" repro
  it replaces passed 95/96 on **both** arms of the 2026-08-04 fix and could not
  detect it. Regression test for the musl `__thread_list_lock` collision;
  diagnosis in `docs/runbooks/debug-futex-lost-wakeup.md`.

## Build (host)

From repo root, with `aarch64-linux-musl-gcc` on PATH:

```bash
cd userspace/forktest/c_stress
aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mmap_stress mmap_stress.c
cp mmap_stress ../../../bootstrap/bin/
```

Or use `userspace/build.sh --with-forktest`, which builds Go forktest and this binary.

## Install on Akuma via `pkg install` (SSH)

`pkg` downloads `http://10.0.2.2:8000/bin/<name>` into `/bin/<name>` ([docs/PACKAGES.md](../../../docs/PACKAGES.md)).

On the **host**, serve the `bootstrap` directory (which contains `bin/mmap_stress`):

```bash
cd /path/to/akuma/bootstrap
python3 -m http.server 8000
```

In **SSH** to the guest:

```text
pkg install mmap_stress
```

Then run the parent with C children instead of Go:

```text
/bin/forktest_parent --use_c_child --duration 10s --mmap_test=true --mmap_alloc_mb=70
```

(`--mmap_test` only selects forwarded flags; the C binary always runs the mmap loop.)

If **this** crashes but plain Go children without mmap do not, the fault is likely in the kernel lazy-paging path. If **this** passes but Go **`--mmap_test`** fails, focus on the Go runtime / syscall errno paths.
