# Go Build Hang — Root Cause Analysis

## Investigation Method

SSH into QEMU (port 2222), ran `ps` and read `/proc/<pid>/syscalls` while
`CGO_ENABLED=0 go build -x -v -o ./hello_go .` was hanging.

## What the Processes Are Doing

```
PID  PPID  SYSCALL  CMDLINE
 50     0   *101    /usr/lib/go/bin/go build -x -v -o ./hello_go .
 51–53  50    -     (CLONE_VM threads of go build)
 55    51    -
 63    50   *101    /usr/lib/go/pkg/tool/linux_arm64/compile ...
 64–66  63    -     (CLONE_VM threads of compile)
```

`*101` = currently in `nanosleep`. `-` = no completed syscall (blocked in
something that hasn't returned yet, most likely `futex_wait`).

### `go build` (PID 50) syscall pattern (from `/proc/50/syscalls`)

```
clock_gettime (113)  ~6µs
clock_gettime (113)  ~2µs
epoll_pwait   (22)   ~150–260µs
nanosleep     (101)  ~37–50ms
```

This is the Go netpoller / sysmon loop. `go build` is sitting in
`epoll_pwait` waiting for the `compile` child to exit (via a pidfd or pipe
event registered on its epoll fd=5). It never gets the event because
`compile` never finishes.

### `compile` (PID 63) syscall pattern (from `/proc/63/syscalls`)

```
clock_gettime (113)  ~6µs    ← very first syscall ever
clock_gettime (113)  ~2µs
nanosleep     (101)  ~37ms
… repeat forever …
sched_getaffinity (123)  ~60–120µs   (appears ~6 times total)
```

**`compile` never issues a single file I/O syscall (open, read, write).** It
produces the Go `sysmon` goroutine pattern from the moment it starts. The
main goroutine — the one that would actually open source files and compile —
never runs.

### Threads 64, 65, 66 (CLONE_VM children of compile)

`/proc/64/syscalls` → "No such file" (no completed syscalls recorded).

This means threads 64–66 are stuck inside a **blocking syscall that never
returns**. The only blocking call in the Go runtime that fits this profile is
`futex(FUTEX_WAIT)`. Futex calls are excluded from `SYSCALL_DEBUG_IO` prints,
and a futex_wait that never returns never appears in `/proc/<pid>/syscalls`
because the log only records **completed** syscalls.

### `compile`'s fd table

```
fd/0 = pipe:[36]   ← stdin is a pipe (write end held by go build)
fd/1 = (stdout)
fd/2 = (stderr)
fd/3+ = ENOENT     (no other fds)
```

---

## Key Update: Goroutine Scheduling Is Fine

`go version` (run on a fresh kernel, without `go build` consuming threads) works
correctly:

```
go version go1.25.8 linux/arm64
[clone] flags=0x50f00  new thread TID=50
[clone] flags=0x50f00  new thread TID=51
[clone] flags=0x50f00  new thread TID=52
[epoll] create1, eventfd2, epoll_ctl...
[signal] tkill(sig=23) + deliver SIGURG
[exit_group] code=0 after 0.49s
PSTATS: futex=13(334ms), clone=3, mmap=89, openat=12, read=7
```

- Futex used 13 times with 334ms total blocking → **goroutine park/unpark works**
- Signals delivered correctly
- 3 CLONE_THREAD M-threads, all working

The previous `go version` timeout during the SSH session was because `go build`
was occupying thread slots, not a kernel bug.

**This eliminates**: TGID/getpid() confusion, futex implementation bugs,
goroutine scheduling deadlock.

**The hang is specific to how `go build` coordinates with `compile`.**

---

## Root Cause: Demand-Paging Race Condition (v2026-03-21)

The Go `compile` processes are crashing with SIGSEGV (Instruction Abort) at kernel identity-mapped memory addresses and reporting "not the start of an archive file" during package imports. 

**Hypothesis: Concurrent Faulting on Shared Address Space**
The kernel's `is_translation_fault` handler is not atomic regarding the `UserAddressSpace` page table updates. In Go's `compile` (which uses `CLONE_VM` worker threads), multiple threads can fault on the same virtual page simultaneously (e.g., during the initial `mmap` of a large data segment). 

1. **Race Window**: Two threads fault on the same VA.
2. **Concurrent I/O**: Both threads trigger `crate::vfs::read_at` into the same physical frame (if race condition is severe) or trigger redundant mapping attempts.
3. **Inconsistent State**: The CPU fetches an instruction from a page while the PTE entry is being updated or before the I/O completion, reading garbage data or triggering an Instruction Abort because the page is still partially mapped or not yet fully flushed to I-cache.
4. **Go Compiler Corruption**: The "not the start of an archive file" error confirms that the compiler thread read an incomplete or corrupted file-backed page during this race.

### Proposed Fix: Demand-Paging Lock
- **Introduce a Per-Process Fault Lock**: Add a `Spinlock<BTreeSet<usize>>` to the `Process` struct to track `VAs` currently being serviced by the demand-paging handler.
- **Serialization**: Ensure that if a thread faults on a VA that is already being handled by another thread, the second thread waits or spins until the page is successfully mapped.
- **Cache Coherency**: Ensure `dsb ish` and `isb` are executed immediately after mapping the page in the `UserAddressSpace`, before returning execution to the user thread.

### Eliminated hypotheses (for the record)

**A. TGID/PID semantics** — Not the cause. Akuma already implements correct
TGID semantics: CLONE_THREAD children share `process_info_phys`, so
`getpid()` returns the parent's PID (the TGID) for all threads in the group.

**B. `futex_wake` not finding waiters across CLONE_VM threads** — Not the
cause. For CLONE_VM threads in the same process, the same VA key is used and
wakes reach the right TID. The problem was cross-PROCESS collisions.

**C. stdin pipe deadlock** — Not the cause. The goroutines never ran at all
(no file I/O syscalls), so they couldn't have blocked on stdin read.

---

## Minimal Reproducer

To isolate whether this is goroutine scheduling or something compile-specific,
build and run this on Akuma:

```go
// test_goroutine.go
package main

import "fmt"

func main() {
    c := make(chan int)
    go func() {
        c <- 42
    }()
    v := <-c
    fmt.Println(v)
}
```

If this **hangs**, the bug is in goroutine scheduling (futex or TGID).
If this **works**, the bug is specific to how `go build` communicates with
`compile` (stdin pipe protocol, fd inheritance, or a compile-specific path).

A second test to try regardless:

```go
// test_gomaxprocs.go
package main

import (
    "fmt"
    "runtime"
)

func main() {
    fmt.Println("GOMAXPROCS:", runtime.GOMAXPROCS(0))
    fmt.Println("NumCPU:", runtime.NumCPU())
}
```

This will reveal whether `sched_getaffinity` is returning a sensible CPU set
and whether GOMAXPROCS is being set to a non-zero value.

---

## Course of Action

### Step 1 — FUTEX_PRIVATE fix ✓ DONE (2026-03-21)

Fixed `src/syscall/sync.rs`: FUTEX_WAITERS keyed by `(tgid, uaddr)` instead
of `uaddr`. See "Root Cause" section above.

### Step 2 — Test with go build

Boot the updated kernel and run:
```
CGO_ENABLED=0 go build -x -v -o ./hello_go .
```
If goroutines now run but something else fails, check the next hypothesis.

### Step 3 — Raise thread cap if needed

If `go build` spawns many parallel `compile` instances and hits the 32-thread
limit (`MAX_THREADS` in `crates/akuma-exec/src/threading/types.rs`), raise it
to 64. Each extra slot costs ~32KB (default thread stack) + 300B of state.
Also update `ExecConfig::max_threads` in `src/main.rs`.

### Step 4 — Document in GOLANG_MISSING_SYSCALLS.md

Add entry #57 for FUTEX_PRIVATE fix.

---

## Note on `proposals/FIX_FUTEX_SUBSYSTEM.md`

That proposal is named misleadingly — it documents the **SA_RESTART ELR bug**,
not a futex implementation bug. Symptom: `[futex] EINVAL: uaddr=0x1` after a
FUTEX_WAKE returned 1, which got re-executed as `FUTEX_WAKE(uaddr=1)` due to
ELR being backed up unconditionally on SA_RESTART. Fix: gate the ELR-4 backup
on `ret_val == EINTR || ret_val == ERESTARTSYS`.

**Status: already implemented** in `src/exceptions.rs:800–805`. The current
hang is a completely different bug — goroutines not being scheduled, not a
futex-EINVAL crash.

---

## Known Working / Fixed (from GOLANG_MISSING_SYSCALLS.md)

- `CGO_ENABLED=0 go build -n` (dry run) — **Fixed** (2026-03-21)
- Signal delivery (`SA_ONSTACK`, `si_code`, re-entrant SIGSEGV) — Fixed
- Pipe infrastructure (broken pipe, quadratic slowdown, ChildStdout) — Fixed
- epoll (`EINTR`, `EPOLLET`, edge-triggered drain) — Fixed
- futex (basic `FUTEX_WAIT`/`FUTEX_WAKE`, `FUTEX_WAKE_OP`, `CMP_REQUEUE`) — Fixed
- `procfs` fd reads for non-stdio fds — Fixed (2026-03-21)

## Fix Applied

- `CGO_ENABLED=0 go build` — **FUTEX_PRIVATE fix applied (2026-03-21)**
  - Root cause: global VA-keyed futex table let `go build` steal `compile`'s
    M-thread wake, goroutines never ran
  - Fix: `(tgid, uaddr)` key in FUTEX_WAITERS, scoping private futex per process
  - Testing pending: boot updated kernel and run `go build`
