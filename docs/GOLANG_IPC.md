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

## Root Cause: Goroutine Scheduler Deadlock

The Go M:N scheduler parks idle M-threads (OS threads) by calling:
```
futex(m.waitsema, FUTEX_WAIT, 0, timeout)
```
When a goroutine becomes runnable, the scheduler wakes an M with:
```
futex(m.waitsema, FUTEX_WAKE, 1)
```

**Threads 64–66 are parked in `futex_wait` and the wake never reaches them.**
The main goroutine is placed on the run queue by the Go runtime initializer,
but no M-thread is woken to run it. The sysmon goroutine (running on thread
63) keeps printing its `clock_gettime + nanosleep` heartbeat but cannot
unblock the scheduler because sysmon's job is just monitoring — it doesn't
directly run goroutines.

### Candidate bugs

**A. TGID/PID semantics (most likely)**

Linux `clone(CLONE_THREAD)` threads share a TGID with their parent:
- `getpid()`  → returns the TGID (same as parent)
- `gettid()`  → returns the individual thread's TID

In Akuma, each `clone` creates a new Process entry with its own PID. So for
thread 64: `getpid()` → 64, but the Go runtime expects `getpid()` → 63 (the
TGID). The Go runtime uses `getpid()` return values internally (e.g. to scope
signal delivery, to identify the thread group). If thread 64 calls `getpid()`
and gets 64 instead of 63, the runtime may fail to locate its own M-struct
and the goroutine run-queue, causing a silent scheduler failure.

**B. `futex_wake` not finding waiters across CLONE_VM threads**

Akuma's `FUTEX_WAITERS` is a global `BTreeMap<usize, Vec<usize>>` keyed by
virtual address, where the value is a list of kernel thread IDs. For CLONE_VM
threads sharing an address space, the same VA key is used, so wakes from
thread 63 should find thread 64's TID. This path looks correct in the code
(`src/syscall/sync.rs`). Less likely to be the root cause, but worth
verifying with a debug log.

**C. `compile` reading from stdin pipe blocks the main goroutine**

The Go build system may use a JSON-over-stdin protocol (`-json` compile mode)
where `go build` sends work requests to `compile` via the pipe. If `go build`
is supposed to write to `pipe:[36]` but hasn't yet (because it's waiting for
`compile` to exit first), this is a classic pipe deadlock. However, since
**no `read` syscall ever completes** for compile, this isn't blocking the main
goroutine at the syscall layer — the goroutine may not even be scheduled yet
to make the read call.

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

### Step 1 — Fix `getpid()` for CLONE_THREAD (TGID semantics)

Check what `sys_getpid` returns for CLONE_VM/CLONE_THREAD children:

```
src/syscall/mod.rs or src/syscall/proc.rs — sys_getpid
```

Linux contract:
- `getpid()` → TGID (the process group leader's PID; same for all threads in a group)
- `gettid()` → per-thread TID (unique)

If Akuma returns the thread's own new PID for `getpid()` instead of the
parent's PID, fix it by returning `parent_pid` for CLONE_THREAD children.
This is likely the primary scheduler bug.

### Step 2 — Add futex_wake debug logging temporarily

In `futex_do_wake`, add a print that shows `uaddr`, how many waiters were
found, and which TIDs were woken. Run `go build` and check whether wakes
are emitted. If wakes are emitted but threads don't wake up, the issue is
in `get_waker_for_thread`. If no wakes appear, the address being used for
wake doesn't match the address registered for wait.

### Step 3 — Run the minimal reproducer

Build and run `test_goroutine.go` on Akuma. If it hangs, add the futex
logging and trace through the goroutine park/unpark cycle for this simple
case before tackling `compile`.

### Step 4 — Understand the compile stdin pipe

Run `go build -x -v` on a host Linux system and observe:
- what flags are passed to `compile`
- whether `-json` or `-stdin` is used
- what `go build` writes to the pipe before waiting

Replicate correct pipe handling in Akuma's `clone`/`exec` path if needed.

### Step 5 — Document in GOLANG_MISSING_SYSCALLS.md

Once root cause is confirmed, add entry #57 with the fix details.

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

## Still Blocked

- `CGO_ENABLED=0 go build` — **hangs at goroutine scheduling stage**
  - `compile` tool spawns 3 M-threads, all stuck in `futex_wait`
  - Main goroutine never runs, no file I/O ever issued
  - Most likely cause: `getpid()` returning thread-local PID instead of TGID,
    breaking Go's runtime M-struct lookup
