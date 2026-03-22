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

## Root Cause: Missing Executable Permission for Signal Handlers (v2026-03-21)

Recent logs indicate that signal delivery for Go's `compile` process often shows `SIGSEGV` with `Instruction Abort` while the **registered handler** is at a normal PIE address (e.g. `0x1009ee90`). The **faulting PC** in the log (`fault_pc` / saved ELR) may instead lie in ranges such as `0x6000_0000` — attempted execution in **non-executable** identity-mapped RAM — which is a different failure than “handler page not RX”.

**Hypothesis: Missing 'X' bit on handler page**
The kernel's `try_deliver_signal` sets `ELR_EL1` to the user-provided signal handler. However, if the handler's page (or the restorer trampoline) was demand-paged with `RW_NO_EXEC` (common for Go's stack and dynamic allocations) and the kernel does not explicitly grant executable permissions, the CPU triggers an `Instruction Abort` (Instruction Permission Fault).

**Fix applied (2026-03-21, `src/exceptions.rs`):**
- Updated `try_deliver_signal` to explicitly call `proc.address_space.update_page_flags(addr, RX)` for both the signal handler and the restorer trampoline before redirecting `ELR_EL1`.
- Sanitized `sigframe` initialization by zeroing the `mcontext_t` and `siginfo_t` regions before populating them to prevent potential pointer leakage or corrupted state.

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

## Exit code 137 (`compile` / `go build`)

On Linux, **`137 = 128 + 9`** is the usual encoding for a process killed by
**SIGKILL** (often the **OOM killer** when the parent reports
`compile: exit status 137`).

**On Akuma**, `exit_code == 137` is set in two places:

1. **`kill_process()`** — explicit SIGKILL-style teardown (`sys_kill`, shell
   `kill -9`, `kill_box`, cascading kill). See `crates/akuma-exec/src/process/signal.rs`.
2. **`kill_thread_group()`** — when the **non-shared** (address-space owner)
   thread exits, **sibling** pthread processes that share the same page table
   but have **different PIDs** are marked **zombie with 137** so they stop
   using freed page tables. See `crates/akuma-exec/src/process/mod.rs`
   (`kill_thread_group`).

Kernel **heap** OOM uses **`return_to_kernel(-12)`** (ENOMEM), not 137
(`src/allocator.rs`).

**What the logs usually mean here:** the build is not “Linux OOM” by default
on Akuma. A typical sequence is **fatal `SIGSEGV` (signal 11)** in `compile`
(e.g. `Instruction abort` / bad **ELR** such as `0x6006…`), then teardown; the
**137** is the kernel’s **SIGKILL-style** exit code, not `128 + 11` (which would
be **139** on Linux for uncaught SIGSEGV). The **`[JIT] IC flush + replay`**
lines point at **stale instruction cache** / generated code vs what the CPU
executes — related to the executable-mapping and coherency work in
`try_deliver_signal` / `sync_el0_handler`.

**LLM / review pitfall:** A line like
`[signal] deliver … handler=0x1009ee90 fault_pc=0x6006c15c …` is easy to misread.
The second address is **not** the handler: it is the **saved user PC at fault
time** (before the kernel overwrites `ELR_EL1` with `handler`). Values around
`0x6000_0000` are **kernel identity RAM** in the user page tables (typically
**execute-never**); a fault there means user code **branched to non-executable
memory**, not that the Go handler at `0x1009ee90` lacked `RX`. The kernel
already applies **`update_page_flags(…, RX)`** for both the handler page and
the sigreturn restorer page in `try_deliver_signal` after that fix. True
recursive delivery while already on the sigaltstack is handled separately
(re-pend / fail delivery — see `try_deliver_signal`).

**Separate issue (not 137):** errors like `could not import internal/cpu (not
the start of an archive file ("cpu.o …"))` mean the **Go build cache entry is
corrupted** (object bytes where a `.a` archive is expected). Fix on that host
with `go clean -cache` or by removing the bad `GOCACHE`/`~/.cache/go-build`
object.

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

---

## Post-success hang: parent epoll vs child exit (2026-03-22)

### Symptom

After the SIGSEGV and futex fixes, `compile` now runs to completion and calls
`exit_group` with code 0.  However `go build` hangs **after** a successful
`compile` — the parent `go` process never progresses to the next build step
(`asm`, `link`, or next `compile` invocation).

This is **distinct** from the stuck-compile symptom documented above: here the
child exits cleanly, but the parent is not notified in time.

### Capture checklist

To reproduce and diagnose, boot Akuma and run:

```
CGO_ENABLED=0 go build -x -v -o ./hello_go .
```

While hung, SSH in and capture:

1. `cat /proc/<go_pid>/syscalls` — expected: `epoll_pwait` (22) or
   `nanosleep` (101) in a loop.
2. `ps` output — note which PIDs exist and their states.
3. The last `-x` line printed before the stall (identifies which build step
   was being attempted).
4. `cat /proc/<go_pid>/fd/*` — check which fds are open (pipes, pidfds,
   eventfds).

### Root cause

`ProcessChannel::set_exited()` was called only in `return_to_kernel()` — after
`sys_exit_group()` had already closed all FDs (pipes) and called
`vfork_complete()`.  This left a race window where:

1. Pipes are closed → parent's epoll sees EPOLLIN/EOF on the pipe read end.
2. Parent calls `wait4(-1, WNOHANG)` or checks the pidfd — but
   `ch.has_exited()` is still false because `set_exited` hasn't been called yet.
3. `pidfd_can_read()` returns false → epoll EPOLLET edge never fires for pidfd.
4. Parent loops in epoll, polling every 10ms, but the pidfd transition from
   not-ready to ready may be missed if `return_to_kernel` runs between the
   snapshot and the readiness update in the same iteration.

### Fix (2026-03-22, `src/syscall/proc.rs`)

Call `set_exited()` on the child channel **in `sys_exit` and `sys_exit_group`**
immediately after marking the process as Zombie — before closing FDs and before
`vfork_complete`.  `set_exited` is idempotent, so the second call from
`return_to_kernel` is harmless.

This ensures `pidfd_can_read()` and `find_exited_child()` return the correct
result as soon as the process decides to exit, eliminating the race window.

### Kernel tests (in `src/process_tests.rs`)

- `test_pidfd_can_read_after_set_exited` — pidfd is not readable before
  `set_exited`, readable after.
- `test_two_child_sequential_exit` — two children registered to the same
  parent; exit in order; `find_exited_child` returns the correct one each time.
- `test_epoll_pidfd_readiness_on_exit` — synthetic `epoll_check_fd_readiness`
  on a `PidFd` entry: returns 0 before exit, EPOLLIN after.
- `test_notify_child_channel_exited_idempotent` — calling `set_exited` twice
  (as `sys_exit_group` + `return_to_kernel` now do) is safe.

### Verification

Boot updated kernel and run `CGO_ENABLED=0 go build -x -v -o ./hello_go .`.
The build should progress past the step where it previously hung.
