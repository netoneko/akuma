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
- `dynspawn` + `dynchild` — hammer vfork+exec of a **dynamically linked** binary
  and check the loader gets each child to `main`. Both binaries are dynamic on
  purpose: musl implements `posix_spawn` with `CLONE_VM|CLONE_VFORK`, so the
  child shares the parent's address space until it execs, and the parent's own
  relocated ld-musl data is what is at risk. After every spawn the parent
  re-checks its own relocated pointer and makes a PLT call. Point it at a large
  dynamic binary for demand-paging pressure:
  `dynspawn 25 4 /usr/local/bin/rustc 0 --version`. Written for the ld-musl
  instruction-abort class in `docs/runbooks/debug-thread-spawn-segv.md` §3;
  **does not reproduce it yet** (700 clean spawns), so that class still needs the
  real build load.
- `mprotectlb` — does `mprotect` take effect on a page that is already in the
  TLB? Three permission *downgrades* on touched pages: RW→PROT_NONE (musl's
  thread-stack guard page), RW→PROT_READ (a dynamic loader's RELRO), and a guard
  page inside a larger mapping (which also catches a flush that invalidates the
  wrong page). Deterministic — one mmap, one touch, one mprotect, one access.
  Regression test for the 2026-08-05 `flush_tlb_range` bug: it invalidated with
  `tlbi vale1is, va>>12`, whose ASID field is zero for every user VA, while user
  processes all run under a non-zero ASID — so the invalidation matched nothing
  and `sys_mprotect` could not downgrade a cached translation. Measured 3 FAIL
  before the fix, 3 PASS after, 3 PASS on Linux.
- `bssfork` — **the regression test for the fork-from-a-threaded-process SIGSEGV**
  (`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §12), and the narrowest
  statement of it: T threads incrementing adjacent `.bss` counters — one page, so
  they contend — while the main thread forks. No mmap, no patterns. Every write in
  it is legal at every instant on any OS; the failure is the kernel refusing one.
  The defect: `fork` demotes the address space read-only, all the threads fault on
  the same page, the first breaks CoW and consumes the CoW reference, and the ones
  behind it arrive holding a fault for a write that is now legal — which the kernel
  judged against the old state and answered with SIGSEGV, because an ELF
  `.data`/`.bss` page has no `mmap` region to fall back on. Measured **8/8 SEGV at
  `20 3` and 5/25 at `1 3` before the fix, 0 after**; PASSES on real Linux aarch64.
  `spread=1` is the control — same threads, same fork churn, one page per thread,
  so no two threads ever fault on the same page. Use it to tell "this load is too
  much for the machine" from "this load hits the contended-fault path": it is what
  proved the `[BKL] stuck tag=511` storm seen at high thread counts is load-driven
  and pre-existing (it storms identically on an unmodified kernel, and on the fixed
  one with `stale_write_faults=0`, i.e. the repair never firing).
  Needs **SMP>=2**: 8/8 SEGV at `SMP=4`, 10/10 PASS at `SMP=1` on the same pristine
  kernel, because the losing thread has to be executing its fault while the winner
  holds the page's slot. Note the workers are CPU-bound and never sleep, so more
  than ~3 of them on one core starve sshd and runs come back with no output — at
  `SMP=1` keep `threads` at or below the core count.
  Usage: `bssfork [rounds] [threads] [spread]`. Calibrate:
  `docker run --rm --platform linux/arm64 -v "$PWD/bssfork:/bssfork:ro" alpine /bssfork 20 8`.
- `cowstale` — **deterministic reproducer for the `EXIT=139` / `[WPF] cow_ref=0
  lazy_self=NONE` class** (proposals/COWSTALE_FORK_THREAD_SEGV.md). Forks
  repeatedly from a process that has live reader threads, so several cores hold
  translations for a range while fork demotes it to read-only; parent and child
  each write their own pattern and verify the other's is never visible. Written
  to catch a CoW break landing a write in the wrong frame; what it actually finds
  first is a fatal fault in ~0.01 s. **Minimal trigger: >=2 fork rounds AND >=2
  reader threads** — one round passes, one thread passes, both together SEGV.
  8/8 on Akuma (idle VM, no build load); PASSES on real Linux aarch64
  (`docker run --rm --platform linux/arm64 -v "$PWD/cowstale:/cowstale:ro" alpine
  /cowstale 40 32 3`). Usage: `cowstale [rounds] [pages] [reader_threads]`,
  exit 0 = clean. This replaces the ~1-in-5, ten-minute self-host build as the
  way to ask "is it fixed yet".
  **Fixed 2026-08-08** (`stale_write_fault_absorbed`, audit §12): 10/10 SEGV at
  `5 8 3` before, 0 after. Two corrections to the notes above, both of which cost
  time: the faulting address was never a corrupted pointer — it is
  `g_reader_checks`, a `.bss` global (`readelf -sW` says so) — and the minimal
  trigger is **>=2 threads**, not two rounds. One round fails too, just less often
  (`bssfork 1 3`: 5/25). Prefer `bssfork` for the regression; it isolates the same
  defect without the mmap machinery this probe carries for other reasons.
- `clonearg` — does a freshly cloned thread see the memory its parent wrote
  immediately before `clone()`? Clones raw (musl `__clone`'s exact register
  shape) so the child's first instructions are the ones the rustc thread-spawn
  crash dies on, then checks three things the child reads before its first
  syscall: the argument popped off its own stack, sentinel words the parent left
  below it, and a page the parent mmap'd microseconds earlier. Every check is
  range-checked before dereference, so a stale value is reported rather than
  crashed on. Built to answer `docs/runbooks/debug-thread-spawn-segv.md`; it
  found **no** divergence in 144k children, which is what rules the memory
  handoff out.
- `spawnalias` — does a freshly-spawned thread see *its own* address space? The
  sequel to `clonearg`, which asked the wrong question: `clonearg` verified the
  clone hand-off and would pass no matter what if the bug is that the child runs
  in someone else's page tables. So this one gives every process an identity —
  a 256 KiB canary region holding `nonce(pid) ^ page_index`, one word per page,
  plus copies in `.data`, in malloc'd heap and in a separate mmap — and has each
  new thread read all of them before doing anything else. They live in different
  pages, so a *partial* aliasing event makes exactly one disagree, and since the
  nonce is a pure function of the pid, the wrong value **names the process it
  came from** (`*** that is pid 431's nonce ***`). It also carries a
  Rust-shaped thread packet (first word read like `ldr x20,[x0]`, then an atomic
  fetch-add) and deliberately poisons the heap between `pthread_create` and
  `pthread_join` with the exact ASCII the real faults kept decoding to — ANSI SGR
  escapes and `+strict-align` — so a use-after-free comes back as printable text
  rather than silent garbage. Load shape is a fan of worker processes with
  `posix_spawn` churn underneath (musl → `CLONE_VM|CLONE_VFORK` → Akuma's vfork
  fastpath). `--ownstack` adds caller-allocated stacks with sentinels,
  `--fanout` adds thread-slot pressure, `--mapfile` adds demand-paging pressure:
  `spawnalias 2000 4 8 --mapfile /usr/local/lib/librustc_driver-*.so`.
  Calibrated: PASS on real Linux (300×4×8). Written for
  `docs/runbooks/debug-thread-spawn-segv.md` §3c — it decides T1 vs T2 vs T3.
- `tidflags` — does `clone(2)` honour its three tid flags separately? Linux keeps
  `CLONE_PARENT_SETTID` (write tid to `ptid` at clone), `CLONE_CHILD_SETTID`
  (write tid to `ctid`, **in the child's context**, so it is not observable the
  instant `clone` returns) and `CLONE_CHILD_CLEARTID` (write *zero* to `ctid` at
  child **exit**, plus a futex wake) strictly apart. Akuma conflated them: it
  wrote the child tid to `ctid` at clone time whatever the flags said, and
  cleared it at exit whatever the flags said. musl's `pthread_create` passes
  CLEARTID *without* CHILD_SETTID and the pointer it passes is
  `&__thread_list_lock` — a global mutex word — so every thread spawn stamped a
  live tid into musl's thread-list lock. `__tl_lock` then takes its
  `if (val == tid) { tl_lock_count++; return; }` fast path for exactly the one
  thread whose tid was written, so the new child ran `__pthread_exit` with no
  lock held and unlinked itself while its parent was still linking it:
  `ldp x0,x1,[x19,#8]; str x0,[x1,#8]` with a NULL `self->prev` → SIGSEGV
  writing to address `0x8`, plus a leaked `tl_lock_count` that wedges every
  later pthread call in the process. Deterministic — one clone and one load per
  check — which is the point: `spawnalias` reproduces the same bug only about
  one run in three, far too flaky to A/B a fix on. Measured **4 FAIL before the
  2026-08-06 fix, 8 PASS after, 8 PASS on Linux**. Calibrate with
  `docker run --rm --platform linux/arm64 -v "$PWD/tidflags:/tidflags:ro" alpine /tidflags`.
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
