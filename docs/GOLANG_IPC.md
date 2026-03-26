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

**On Akuma**, `exit_code == 137` is set in these situations:

1. **`kill_process()`** — explicit SIGKILL-style teardown (`sys_kill`, shell
   `kill -9`, `kill_box`, cascading kill). See `crates/akuma-exec/src/process/signal.rs`.
2. **`kill_thread_group()`** — when the **non-shared** (address-space owner)
   thread exits, **sibling** pthread processes that share the same page table
   but have **different PIDs** are marked **zombie with 137** so they stop
   using freed page tables. See `crates/akuma-exec/src/process/mod.rs`
   (`kill_thread_group`).
3. **Forked-child teardown** — `kill_fork_subtree_recursive` /
   `teardown_forked_process_thread_group` force **137** when reaping a separate
   address space (orphan cleanup, same numeric convention as thread-group kill).

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

**Separate issue (not 137, now fixed):** errors like `could not import
internal/cpu (not the start of an archive file ("cpu.o …"))` were caused by
missing `O_APPEND` support in `sys_write` — see *"`O_APPEND` not honoured in
`sys_write`"* below.  If stale cache entries remain from a pre-fix kernel, run
`rm -rf $(go env GOCACHE)` to clear them.

**Regression (fixed): `go: error obtaining buildID for go tool compile: exit status 137`.**
If `return_to_kernel` called **`kill_child_processes_for_thread_group(l0)` on
every pthread exit**, the parent set included **all** thread PIDs (including
the main thread). Any forked child whose **`parent_pid` was the main `go` PID**
was then torn down whenever **any** short-lived worker thread exited — while
`go` still needed that **`compile -V=full`** subprocess. Fix: run the **full
thread-group** scan only when the **address-space owner** exits
(`!address_space.is_shared()`); on **pthread** exit, call
**`kill_child_processes(exiting_pid)`** only (children parented to that thread).

**Second cause (fixed): `wait4` vs `[exit_group] … code=0`.**  Forked children share one
**`ProcessChannel`** Arc via `register_child_channel` and `notify_child_channel_exited`
/`return_to_kernel`.  **`teardown_forked_process_thread_group`** used to always
**`set_exited(137)`** on that channel.  If **`exit_group` had already set the real
exit code (e.g. 0)**, teardown could **overwrite** it with **137**, so **`go`**
saw **exit status 137** while the serial log still showed **`code=0`**.  Fix:
only **`set_exited(137)`** when **`!ch.has_exited()`**.  With **`SYSCALL_DEBUG_INFO_ENABLED`**,
**`wait4`** logs **`exit_code=`** and **`wait_status=0x…`** (Linux encoding: `(code&0xff)<<8`).

---

## Known Working / Fixed (from GOLANG_MISSING_SYSCALLS.md)

- `CGO_ENABLED=0 go build -n` (dry run) — **Fixed** (2026-03-21)
- Signal delivery (`SA_ONSTACK`, `si_code`, re-entrant SIGSEGV) — Fixed
- Pipe infrastructure (broken pipe, quadratic slowdown, ChildStdout) — Fixed
- epoll (`EINTR`, `EPOLLET`, edge-triggered drain) — Fixed
- futex (basic `FUTEX_WAIT`/`FUTEX_WAKE`, `FUTEX_WAKE_OP`, `CMP_REQUEUE`) — Fixed
- `procfs` fd reads for non-stdio fds — Fixed (2026-03-21)
- `procfs` **`/proc/<pid>/cmdline`** (NUL-separated argv) and **`/proc/<pid>/status`**
  (`Name`, `State`, `Pid`, `PPid`, …) — Added (2026-03)
- **`wait4` / `ProcessChannel`:** teardown no longer overwrites a real exit code with **137**
  (see *Second cause* under *Exit code 137* above).

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

While hung, SSH in and capture (if the guest still accepts SSH — on a **full kernel freeze**,
SSH may be unreachable; use **serial** logging instead, see *Stall: `clone` (VFORK)* below):

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

### Stall: `clone` (VFORK) but no next `execve` (2026-03)

**Symptom:** `go build -x` prints the script for the **next** package (e.g. moving from
`internal/runtime/math` to `internal/runtime/strconv`), but the build **never advances** —
the shell appears hung.  Serial **`full.log`** may show **`[exit_group]`** for the previous
**`compile`** (e.g. **`b022/_pkg_.a`**, PID 150, **code=0**), then **`[clone] flags=0x4111`**
(VFORK) and **`[pipe] create` / `clone_ref`**, followed by **many** **`[epoll] pwait …`**
lines (see *Tracing: epoll* below) and **no** **`[syscall] execve(... compile ...)`** for the new action.

**Meaning:** the **vfork** child path started (pipes cloned), but the kernel **never logged**
**`execve`** for the next **`compile`**.  The parent **`go`** (often **PID** of **`/usr/lib/go/bin/go`**)
keeps running its **epoll** loop.  This is **not** the same as “compile stuck in user code”
(there would be **`mmap` / `epoll` on the `compile` PID**).  Here the next **`compile` never
replaces the address space.

**What to capture:** `ps` (is there a **new child PID** stuck in **Running** vs **zombie**?),
**`/proc/<pid>/status`** on **`go` and any new `compile` PID**, and whether **`full.log`** gains
an **`execve`** line if you wait longer (slow disk vs true hang).

**If SSH stops responding:** a hard hang can wedge scheduling, locks, or the network/SSH path so
**`ssh -p 2222 …` never connects** even though the VM is still running.  Do **not** rely on SSH
for post-mortem in that case.  Prefer:

- **Serial console** — QEMU is typically started with **`-serial mon:stdio`** (`scripts/run.sh`,
  `scripts/run_on_kvm.sh`), so kernel **`dmesg`-style** output and **`SYSCALL_DEBUG_INFO`** traces
  go to the **same terminal** as QEMU.  Redirect that stream to **`full.log`** (`tee`, or run QEMU
  under `script`) so you still have a trace when SSH is dead.
- **Reproduce with logging on from boot** — enable whatever serial logging you need *before* the
  hang; after a freeze you may not get a second chance to open a shell.
- **Optional:** a **second serial** (`-serial file:full.log` on a second `-chardev` if you split
  devices) keeps a dedicated log file without fighting the monitor — only worth it if you routinely
  lose the combined stdio capture.

**Related:** *Post-success hang* above (parent **epoll** vs **exit notification**); this pattern
is **earlier** in the pipeline (**before** the next **`execve`**).

### Observed stall point: `internal/cpu` `asm -gensymabis` (2026-03-22)

The build successfully completes `internal/goarch` (compile), `internal/abi` (compile + asm +
pack + buildid + cp), and `internal/unsafeheader` (compile).  It then starts `internal/cpu`:
`asm -gensymabis` is the last `-x` line printed before the **freeze**.

**What the serial log shows at the freeze:**

1. Previous `compile` (PID 93, `internal/abi`) exits cleanly: `[exit_group] pid=93 … code=0`.
2. Parent `go` (PID 60) processes the exit: `pwait ret … nready=1` (pipe EOF), thread cleanup
   fires (`[Cleanup] Thread 12/14/15/16 recycled`).
3. `go` opens many new fds for the next steps (epoll `ctl ADD` fd=2136..2155 — **fd numbers in
   the 2100s**).
4. VFORK clone: `[clone] flags=0x4111 stack=0x0`, pipes 48/49 created and clone_ref'd.
5. `pwait ret … nready=2` (one INOUT + one OUT).
6. Then **`pwait zero-sample#17 … nready=0 timeout=0ms`** — the parent is polling with no
   events ready.  The **vfork child never calls `execve`** (no `[syscall] execve` line).

**Notable: fd numbers are very high (2136–2155).** Root cause identified: the monotonic
`alloc_fd` counter never reused freed fd numbers.  See *"Fd number allocation: monotonic
counter → lowest-available"* below for the fix.  The high numbers are **not** a pidfd or
cloexec leak — `go` simply cycled through many short-lived pipe/epoll/eventfd fds over its
lifetime and the counter only went up.

**SSH was unreachable** during the hang; serial was the only output channel and it stopped
producing new lines after the `pwait zero-sample#17` line — consistent with the kernel itself
being wedged (no scheduling, or a spinlock deadlock).

**Update (2026-03-22):** The compile crashes (`exit status 137`, SIGSEGV, `index out of range
[-38]`) observed in the same session are explained by the icache invalidation bug — see
*"Demand-paging icache invalidation bug"* below.  The archive corruption (`"not the start of
an archive file"`) was caused by missing `O_APPEND` support — see *"`O_APPEND` not honoured in
`sys_write`"* below.  Both are now fixed.

**Current status (2026-03-22, post-O_APPEND fix):** With a clean Go cache, the build compiles
~35 packages successfully (zero archive errors, zero SIGSEGV), but the kernel still
crashes/hangs during the build, killing 4–5 compile processes (`exit status 137`) and dropping
the SSH session (`signal: hangup`).  The kernel serial log shows all completed compiles exiting
with `code=0` — the 137s come from processes that were killed when the kernel died (they never
reached `exit_group`).  The kernel hang remains under investigation; it may be a spinlock
deadlock or scheduling issue.

### Tracing: `epoll_pwait` + PSTATS

When **`SYSCALL_DEBUG_NET_ENABLED`** is on, each **`epoll_pwait`** logs **one return line**:
**`[epoll] pwait ret pid=… epfd=… timeout_ms=… nready=… iters=… dur_us=… interest_fds=…`**, and
up to **six** **`ev[i] data=…`** lines when **`nready>0`**.  The hot path **`timeout_ms=0`**, **`nready=0`**
is **sampled** every **`EPOLL_ZERO_SAMPLE_INTERVAL`** (see `src/config.rs`, default **64**) so
serial is not flooded.  **`[PSTATS]`** lines label syscall **101** as **`nanosleep`** (not **`nr101`**).

### Why you might see **no** `[epoll] pwait` lines (not a “broken logger”)

These lines are **`tprint!`** from the **kernel** (`src/syscall/poll.rs`) when
**`SYSCALL_DEBUG_NET_ENABLED`** is **`true`** in **`src/config.rs`**.  They are **not**
emitted through the Rust **`log`** crate or userspace logging — so there is nothing
special to “hook up” beyond **rebuilding and booting** a kernel that still has that
flag set.  If you run an **older** `akuma` binary, a **release** config that turned
the flag off, or capture only **userspace** output, you will not see **`pwait ret`**.

Because **`timeout_ms=0`** + **`nready=0`** returns are **sampled** (default every **4096**
such returns), a long stretch of **busy epoll** can produce few **`pwait`** lines even
when the flag is on — that is expected.  Raise or lower **`EPOLL_ZERO_SAMPLE_INTERVAL`**
when you need more or fewer samples (currently **64**; was **4096** which was too quiet).

### Fork: serial vs `log::debug` during long **`brk` copy**

While **`fork_process`** eagerly copies the parent **`brk`** range, progress uses
**`log::debug!`** for page/va detail — that output **often does not appear on QEMU
serial** unless you have a **`log`** backend wired to the console.  Separately,
**`FORK_BRK_SERIAL_PROGRESS`** (see **`src/config.rs`**, default **`true`**) prints a
short **`[fork] brk copy still running…`** line to **serial** every **8192** brk pages
via **`runtime().print_str`** so you can tell a **multi‑minute fork** from a **dead
hang**.  Turn it off with **`FORK_BRK_SERIAL_PROGRESS = false`** if the lines are too noisy.

### Capturing **`full.log`** across runs

If you redirect QEMU/serial to **`full.log`**, **each run overwrites** the file unless
you use **append** (**`tee -a full.log`**) or a **timestamped** name
(**`full-$(date …).log`**).  After a **freeze**, the file only reflects output **up to
the point the guest stopped flushing** — not a post‑mortem if the buffer never made it
to the host.

### Serial “stuck” with no SSH during a freeze

The guest may still be **working** (e.g. **long `brk` fork** with little serial output),
**wedged** in the kernel, or **flooded** so the terminal looks frozen.  Prefer **serial**
over SSH for diagnosis; see **If SSH stops responding** above.  After adding **`[fork] brk`**
lines, a **live** serial stream should show periodic progress during long forks.

---

## Demand-paging icache invalidation bug (2026-03-22)

### Symptom

Multiple `compile` processes crash with different manifestations during a single `go build`:

1. **`exit status 137`** on `internal/goarch`, `math`, `unicode` — killed by SIGSEGV.
2. **SIGSEGV at `PC=0x20000000`** (stack base, not code) in `unicode/utf8` compile —
   wild jump caused by executing stale instructions.
3. **`internal compiler error: index out of range [-38]`** in `sync/atomic` compile —
   `-38 = -ENOSYS`, likely a syscall return value treated as an array index due to
   executing the wrong code path.
4. **`could not import internal/cpu (not the start of an archive file ("cpu.o …"))`** —
   a previous `compile` was killed mid-write leaving a truncated `.a` cache entry.
5. **`[JIT] IC flush + replay #1 bogus nr=806400176`** (4 occurrences in `full.log`) —
   the CPU fetched stale bytes from a demand-paged code page and interpreted them as
   a syscall with a huge, invalid number.
6. **`[WILD-DA] pid=157 FAR=0x17a ELR=0x103a7e90`** — data abort at a near-NULL address;
   register dump shows `x0=0xffffffffffffffda` (`-ENOSYS`), consistent with corrupted
   control flow after an `inotify_add_watch` (nr=27) returned ENOSYS.

### Root cause

The Instruction Abort demand pager (`src/exceptions.rs`) used **`IC IVAU`** on the **kernel
identity-mapped VA** (`phys_to_virt(phys)`) instead of the **user VA** (`cur_va`) where the
process would fetch instructions.  `IC IVAU` invalidates the icache by virtual address.
On QEMU TCG, the translation block (TB) cache is keyed by VA — invalidating the kernel VA
left the user VA's stale TBs intact.  The CPU continued executing stale/zero bytes from
previously cached translations.

A secondary issue: `DC CVAU` and `IC IVAU` were interleaved in the same loop without a
`DSB ISH` barrier between them.  ARM ARM requires DC → DSB → IC → DSB → ISB ordering.

### Fix (2026-03-22, `src/exceptions.rs`)

Two demand-paging sites (Instruction Abort path and Data Abort path) were updated:

1. **`IC IVAU` now targets `cur_va`** (the user VA) instead of `kva`.
2. **`DC CVAU` loop and `IC IVAU` loop are separated by `DSB ISH`**, matching the
   ARM Architecture Reference Manual sequence.

The final `DSB ISH` + `ISB` after the readahead loop remains unchanged.

---

## Fd number allocation: monotonic counter → lowest-available (2026-03-22)

### Symptom

During Go build, fd numbers reach **2000+** (observed: `epoll fd=2447`, `eventfd fd=2507`
in `full.log`).  Each forked `compile` child inherits the parent's inflated fd counter,
so children also start allocating from high numbers.

### Root cause

`alloc_fd` in `crates/akuma-exec/src/process/fd.rs` used `next_fd.fetch_add(1)` — a
**monotonically increasing** counter that never reuses freed fd numbers.  POSIX requires
`open()`, `pipe()`, `dup()`, `socket()` etc. to return the **lowest available** fd number.

Additionally, `fcntl(F_DUPFD, arg)` and `fcntl(F_DUPFD_CLOEXEC, arg)` ignored `arg` and
used the same monotonic counter instead of finding the lowest available fd >= `arg`.

### Fix (2026-03-22)

- `alloc_fd` now scans the BTreeMap keys for the first gap starting from fd 0.
- Added `alloc_fd_from(min_fd, entry)` for `F_DUPFD` / `F_DUPFD_CLOEXEC`.
- Removed the `next_fd: AtomicU32` field from `SharedFdTable`.
- Updated `fcntl` in `src/syscall/fs.rs` to pass `arg` as `min_fd`.

---

## SIGSEGV after exit_group in CLONE_THREAD group

### Symptom

`compile` (pid 138) crashes with SIGSEGV at `fault_pc=0x6006c15c` (kernel
identity RAM, UXN) and exits with code 137.  The crash happens ~190 ms after a
sibling thread (pid 141) calls `exit_group(0)`.  The Go build reports
`exit status 137`.

### Root cause

Two bugs combine to let a killed thread continue running user code with its
demand-paging metadata removed.

**Bug 1 — premature `clear_lazy_regions` in `kill_thread_group`.**
`kill_thread_group` called `clear_lazy_regions(*sib_pid)` for every sibling,
including the address-space owner.  For CLONE_VM threads the lazy regions are
keyed by the AS owner PID (via `read_current_pid()` from the shared process
info page).  Clearing the owner's lazy regions removes ALL demand-paging
metadata for the entire shared address space while sibling threads are still
executing and may touch pages that haven't been faulted in yet.

**Bug 2 — `schedule_blocking` overwrites TERMINATED with RUNNING.**
When a thread is in `schedule_blocking` (WAITING state) and `kill_thread_group`
marks it TERMINATED then sets `WOKEN_STATES[tid] = true`, the
`schedule_blocking` loop unconditionally set `THREAD_STATES[tid] = RUNNING` —
overwriting TERMINATED.  The thread then returns from the syscall, re-enters
user mode, and runs Go code with no demand-paging metadata.  A page fault fails
(no lazy region), cascading into a wild branch to `0x6006c15c` and SIGSEGV.

### Fix (2026-03-22)

**Fix 1 (`crates/akuma-exec/src/process/mod.rs`):**
Removed `clear_lazy_regions(*sib_pid)` from the `kill_thread_group` sibling
loop.  Lazy regions are metadata in a BTreeMap — they don't consume physical
memory and are cleaned up later by each thread's `return_to_kernel` path.

**Fix 2 (`crates/akuma-exec/src/threading/mod.rs`):**
Added a TERMINATED guard in `schedule_blocking`'s wakeup path: before setting
`THREAD_STATES[tid] = RUNNING`, the code now checks whether the state was
changed to TERMINATED.  If so it breaks without overwriting, ensuring the
scheduler never re-schedules the thread.

### Kernel tests (in `src/process_tests.rs`)

- `test_kill_thread_group_preserves_lazy_regions` — registers owner + sibling
  with shared l0_phys, pushes lazy regions under the owner PID, calls
  `kill_thread_group`, asserts the owner's lazy regions survive.
- `test_kill_thread_group_marks_siblings_zombie` — verifies the owner is marked
  zombie(137) with `exited = true` after `kill_thread_group`.
- `test_schedule_blocking_respects_terminated` — sets a thread slot to
  TERMINATED and wakes it, verifies the state stays TERMINATED (wake does not
  overwrite to RUNNING).

### Verification

Boot updated kernel and run `CGO_ENABLED=0 go build -x -v -o ./hello_go .`.
The compile step should no longer crash with exit status 137.

---

## Process identity collision after kill_thread_group (SIGSEGV at 0x6006c15c)

### Symptom

After the lazy-region and schedule_blocking fixes above, the same
`fault_pc=0x6006c15c` crash still occurs.  The Go `compile` tool exits with
code 137 approximately 600 ms after a sibling thread calls `exit_group(2)`.
Thread recycling logs show the original thread slot was freed within ~80 ms,
yet the SIGSEGV fires much later — on a **new** process that was spawned on
the same recycled slot.

The build output also shows corrupted Go cache entries:

```
could not import internal/cpu (not the start of an archive file ...)
```

This is a downstream effect: the previous compile process was killed mid-write,
leaving a truncated `.a` file in the Go build cache.

### Root cause

`kill_thread_group` marks the address-space owner as `Zombie(137)` but does
**not** clear `proc.thread_id`.  The subsequent `return_to_kernel` for the
already-terminated thread sets `pid = None` (skip path) and never calls
`unregister_process`, so the zombie process **leaks** in `PROCESS_TABLE` with
a stale `thread_id = Some(slot)`.

When a new child is spawned on the same slot via `fork_process`, both the
leaked zombie and the new child have `thread_id = Some(slot)`.
`entry_point_trampoline` scans `PROCESS_TABLE` (a `BTreeMap`, iterated in
ascending PID order) and finds the leaked zombie **first** (lower PID).  The
new child thread calls `zombie.run()`, activating the **stale** address space
with stale user context.  The Go runtime resumes in a corrupted state and
branches to `0x6006c15c` (kernel identity RAM, UXN in user tables) — SIGSEGV.

The real child process (higher PID) never runs.  If the parent used
`CLONE_VFORK`, it blocks forever waiting for `vfork_complete` that never fires.

### Affected code paths

- `kill_thread_group` in `crates/akuma-exec/src/process/mod.rs` — sets
  `proc.exited`, `proc.state = Zombie(137)` but leaves `proc.thread_id` intact.
- `return_to_kernel` — the `already_terminated = true` branch unconditionally
  sets `pid = None`, skipping `unregister_process`.  Designed for `kill_process`
  (which unregisters before terminating), but `kill_thread_group` does not.
- `entry_point_trampoline` — linear scan of `PROCESS_TABLE.values_mut()` for
  `proc.thread_id == Some(tid)`.  BTreeMap order means lower (stale) PIDs win.

### Fix (2026-03-22)

**Fix 1 (`crates/akuma-exec/src/process/mod.rs` — `kill_thread_group`):**
Added `proc.thread_id = None` when marking each sibling as `Zombie(137)`.
This prevents `entry_point_trampoline` from matching the zombie when scanning
`PROCESS_TABLE` by `thread_id`.

**Fix 2 (`crates/akuma-exec/src/process/mod.rs` — `return_to_kernel`):**
Removed the `already_terminated` shortcut that unconditionally set `pid = None`.
Now `current_process()` is always called.  For `kill_process` (which
unregisters before terminating) it returns `None` naturally — no behaviour
change.  For `kill_thread_group` (zombie still registered) it returns the PID
so the process is properly unregistered, fixing the memory/identity leak.

### Kernel tests (in `src/process_tests.rs`)

- `test_kill_thread_group_clears_thread_id` — registers owner + sibling with
  shared `l0_phys`, calls `kill_thread_group`, asserts that the owner's
  `thread_id` is `None` afterward.
- `test_entry_point_trampoline_no_zombie_match` — registers a zombie (cleared
  `thread_id`) and a live child on the same slot.  Replicates the
  `entry_point_trampoline` scan and asserts only the child is found.
- `test_zombie_process_unregistered_after_return_to_kernel` — verifies a
  zombie left by `kill_thread_group` is still registered and can be
  unregistered (the precondition the `return_to_kernel` fix relies on).

### Verification

Boot updated kernel and run `CGO_ENABLED=0 go build -x -v -o ./hello_go .`.
The compile step should no longer crash with exit status 137, and the
`CLONE_VFORK` parent should not hang.

---

## Kernel hang and orphan children (2026-03-22)

Three separate bugs remain after the fixes above, all observable during a
full `go build` that spawns 300+ `compile` children.

### Bug 1: fd table spinlock deadlock (kernel hang)

`clone_deep_for_fork()` and `close_all()` in
`crates/akuma-exec/src/process/fd.rs` acquire `self.table.lock()` (and
`self.cloexec.lock()` / `self.nonblock.lock()` in `clone_deep_for_fork`)
**without** `with_irqs_disabled`.  Every other caller of these spinlocks
(`alloc_fd`, `get_fd`, `remove_fd`, `set_fd`, `set_cloexec`, etc.) wraps the
lock acquisition in `with_irqs_disabled`.

If Thread A holds the lock *without* IRQ protection and is preempted by the
timer, Thread B entering `alloc_fd` (which disables IRQs before spinning)
will spin forever with IRQs masked.  No further timer interrupts fire, no
context switches happen, and the entire kernel deadlocks.

```
Thread A (exit_group)              Thread B (sibling, alloc_fd)
─────────────────────              ────────────────────────────
close_all()
  table.lock() ← acquired
  ... iterating fds ...
  ── TIMER IRQ ── preempted ──►
                                   with_irqs_disabled {
                                     DAIF.I = 1  (IRQs masked)
                                     table.lock() ← SPINS forever
                                     ... no timer can fire ...
                                     ... no context switch ...
                                     ╳ KERNEL DEADLOCK ╳
                                   }
```

**Trigger**: `sys_exit_group` calls `proc.fds.close_all()` (no IRQ guard)
while a sibling CLONE_THREAD thread concurrently calls `alloc_fd` (with IRQ
guard).  Also triggered during `fork_process` → `clone_deep_for_fork`.

### Bug 2: orphan children not killed on parent exit

`sys_exit_group` calls `kill_thread_group(pid, l0_phys)` which only kills
CLONE_THREAD siblings (threads sharing the same L0 page table).  Forked
children — like Go's `compile` processes — have their own address spaces and
are not affected.

```
go build TGID / main thread PID 58  (shared L0 for all M-threads)
├── thread 53  (CLONE_THREAD, same L0)  — forked compile → parent_pid=53
├── thread 59  (CLONE_THREAD, same L0)  ← killed by kill_thread_group ✓
├── thread 60  (CLONE_THREAD, same L0)  ← killed by kill_thread_group ✓
├── thread 61  (CLONE_THREAD, same L0)  ← killed by kill_thread_group ✓
│
├── compile (PID 98, fork, own L0)      ← NOT killed by kill_child_processes(58) ✗
│   │   (kernel parent_pid = 53, not 58)
│   ├── thread 99   (CLONE_THREAD)      ← NOT killed ✗
│   ├── thread 100  (CLONE_THREAD)      ← NOT killed ✗
│   └── thread 101  (CLONE_THREAD)      ← NOT killed ✗
│
└── compile (PID 102, fork, own L0)     ← same class of leak
    └── ...
```

**Follow-up bug:** `fork_process` sets `parent_pid` to the **forking thread's**
PID (the value from `current_process()` for that M-thread), not the process
group leader / TGID.  So a `compile` forked by worker thread **53** has
`parent_pid = 53`, while `exit_group` on the main thread runs
`return_to_kernel` with `pid = 58`.  `kill_child_processes(58)` therefore
finds **no** children (none have `parent_pid == 58`), leaving compiles
orphaned until thread 53's `return_to_kernel` runs — which may not happen
reliably before the thread pool or `ps` state looks "stuck".

When `go build` exits, its forked children become orphans.  Akuma has no
`init` process to reparent and reap them, so they continue running (or sit as
zombies) consuming thread slots and memory.  After several `go build` runs
the 32-thread pool fills up and no new processes can be spawned.

### Bug 3: CLONE_PIDFD pidfds not marked O_CLOEXEC

In Linux, `clone3` with `CLONE_PIDFD` always creates the pidfd with
`O_CLOEXEC`.  In Akuma, `sys_clone_pidfd` (in `src/syscall/proc.rs` line
~287) calls `sys_pidfd_open(new_pid, 0)` and never calls
`proc.set_cloexec()`.  Every pidfd the Go parent creates for a child
therefore survives `exec` in subsequently forked children.

```
go build (parent fd table)         compile child #300 (after exec)
──────────────────────────         ────────────────────────────────
fd 0: stdin                        fd 0: stdin
fd 1: stdout                       fd 1: stdout (pipe to parent)
fd 2: stderr                       fd 2: stderr
fd 3: pidfd(child#1)  ← no CLOEXEC → fd 3: pidfd(child#1)   LEAKED
fd 4: pipe-r(child#1) ← CLOEXEC   (closed by exec)
fd 5: pipe-w(child#1) ← CLOEXEC   (closed by exec)
...                                ...
fd 302: pidfd(child#300)           fd 302: pidfd(child#300) LEAKED
                                   fd 303: epoll  ← next_fd = 303+
                                   ... or worse: next_fd = 2166
```

After 300 compile children, each new child inherits ~300 stale pidfds,
inflating `next_fd` into the thousands (observed: 2166), wasting memory
in `clone_deep_for_fork` copies, and slowing `close_all` on exit.

### Fix 1 (fd lock deadlock) — `crates/akuma-exec/src/process/fd.rs`

Wrapped all lock acquisitions in `clone_deep_for_fork` and `close_all` with
`with_irqs_disabled`, matching every other caller.  The per-fd cleanup calls
(pipe_close_write, etc.) remain outside the lock and outside the IRQ-disable
window.

### Fix 2 (orphan children) — `crates/akuma-exec/src/process/mod.rs`

Added **`kill_child_processes(parent_pid)`** for tests and single-parent kill.
**`kill_child_processes_for_thread_group(l0_phys)`** collects **every** PID in
`PROCESS_TABLE` that shares the same `l0_phys` (all pthreads in the group),
then reaps every forked child whose **`parent_pid` is any** of those PIDs —
needed because **`fork_process`** stores the **forking thread's** PID, not the
TGID.

**Critical:** that **full-group** scan must run only when the **address-space
owner** exits (`UserAddressSpace` with **`is_shared == false`**).  On **pthread**
exit (`is_shared == true`), `return_to_kernel` must call
**`kill_child_processes(exiting_pid)`** only — otherwise **any** worker exit
matches children whose `parent_pid` is the **main** thread and **live**
`compile` processes are torn down (**137**) while `go` still needs them (see
**Regression** above).

For each forked child to reap: depth-first nested forks, then
**`teardown_forked_process_thread_group`**: `kill_thread_group`, fd cleanup,
**`unregister_process`** (not only a zombie row — otherwise `ps` / `PROCESS_TABLE`
leaks).  Children already marked **`exited` / `Zombie(137)`** must still be
included in the scan (do **not** filter `!proc.exited` when collecting children),
or zombies never unregister.

```
return_to_kernel(exit_code)
  │
  ├── l0, is_shared = current process
  ├── if l0 != 0:
  │     if is_shared (pthread):
  │         kill_child_processes(pid)              ← only this thread's forks
  │     else (owner):
  │         kill_child_processes_for_thread_group(l0)   ← all pthread PIDs as parents
  │
  ├── if !is_shared && l0 != 0: kill_thread_group(pid, l0)
  ├── clear_lazy_regions(pid)
  └── unregister_process(pid)
```

Same fork-child logic is in **`return_to_kernel_from_fault`** (minus user-memory
cleanup).

### Fix 3 (pidfd cloexec) — `src/syscall/proc.rs`

Added `proc.set_cloexec(pidfd_fd as u32)` in `sys_clone_pidfd` after
`sys_pidfd_open` succeeds, matching Linux `clone3` + `CLONE_PIDFD` semantics.

### Kernel tests (in `src/process_tests.rs`)

- `test_fd_table_lock_consistency` — verifies `clone_deep_for_fork` and
  `close_all` complete without deadlocking and produce independent copies.
- `test_kill_child_processes_basic` — registers parent + child, calls
  `kill_child_processes`, asserts the child is **removed** from `PROCESS_TABLE`.
- `test_kill_child_processes_recursive` — parent → child → grandchild; asserts
  **both** nested fork children are gone after `kill_child_processes(parent)`
  (depth-first teardown).
- `test_kill_child_processes_thread_group_matches_fork_parent` — main + worker
  (shared L0) + compile with `parent_pid = worker`; `kill_child_processes(main)`
  misses compile, `kill_child_processes_for_thread_group(l0)` kills it.
- `test_pidfd_cloexec` — verifies `set_cloexec` + `is_cloexec` round-trip.

### Verification

Boot updated kernel and run `CGO_ENABLED=0 go build -x -v -o ./hello_go .`.
After `go build` exits, `ps` should show no orphan `compile` processes.  The
kernel should not hang during fork or exit.  The compile process's epoll fd
numbers should stay low (< 100 instead of 2000+).

For debugging: **`cat /proc/<pid>/status`** and **`cat /proc/<pid>/cmdline`**
(see **Known Working**); **`tr '\0' ' ' < /proc/<pid>/cmdline`** prints argv in a
readable form.

---

## `O_APPEND` not honoured in `sys_write` (2026-03-22) — **FIXED**

### Symptom

Go packages with assembly (e.g. `internal/cpu`, `internal/abi`, `sync/atomic`)
fail with:

```
could not import internal/cpu (not the start of an archive file ("cpu.o  0  0  0  644  238  `\n"))
```

Dependent packages cascade-fail.  The Go driver kills blocked compiles,
producing `exit status 137`.

### Root cause

Go’s `pack r` tool appends `.o` members to an existing `_pkg_.a` archive by
opening it with `O_WRONLY|O_APPEND|O_CLOEXEC` (`flags=0x80401`) and writing
the new ar member.  The kernel’s `sys_write` implementation ignored the
`O_APPEND` flag entirely — writes always started at the file descriptor’s
current `position` (typically 0 for a freshly opened file).  This overwrote
the `!<arch>\n` header with the `.o` member header (e.g. `cpu.o  0  0 ...`),
corrupting the archive.

### Fix (2026-03-22, `src/syscall/fs.rs`)

In the `sys_write` handler for `FileDescriptor::File`, when the `O_APPEND`
flag is set, the initial write position is now set to the current file size
(via `crate::fs::file_size`) instead of the fd’s stored `position`.  After the
write, the fd position is updated to the new end-of-file.

### Verification (runtime evidence from `[PACK-DBG]` instrumentation)

Before fix: `pack r` opened archives with `O_APPEND` but writes went to
`pos=0`, overwriting the `!<arch>\n` header.

After fix, all 8 packages with assembly correctly append:

```
[PACK-DBG] openat path=.../b011/_pkg_.a fd=6 flags=0x80401 size=96998 pid=59
[PACK-DBG] write  path=.../b011/_pkg_.a pos=96998 len=2990 first8=0x2020206f2e757063
                                         ^^^^^^^^ = file size (appending, not overwriting)
```

Subsequent reads show `first8=0x0a3e686372613c21` (`!<arch>\n`) — archive
header preserved.  57/57 compile and asm tool invocations exit with code=0.
Zero “not the start of an archive file” errors.  Zero “exit status 137”.

### Host-runnable tests

- `akuma-vfs::tests::memfs_tests::write_at_file_size_simulates_o_append` —
  simulates `pack r` append pattern on MemoryFilesystem.
- `akuma-vfs::tests::memfs_tests::write_at_zero_overwrites` — confirms
  offset-0 writes correctly overwrite (not append).
- `akuma-ext2::tests::write_at_file_size_appends_without_overwriting` —
  same pattern on ext2, confirming the filesystem layer was correct and the
  bug was in the syscall layer.

---

## `compile` SIGSEGV → corrupt cache → “not the start of an archive” (historical)

**Note:** The primary cause of “not the start of an archive file” errors was
the missing `O_APPEND` support documented above.  This section documents a
secondary path that can produce the same symptom when a `compile` process
crashes mid-write.

### Symptom chain

1. **`compile` crashes** with `SIGSEGV` while building an early std package.
2. A **partial or corrupt** archive is written under `GOCACHE`.
3. **Cascade**: dependent packages fail on the bad cached archive.

**Distinguish from orphan / pthread cleanup:** **`error obtaining buildID for go tool compile: exit status 137`**
right after a kernel change often means **`compile -V=full` was torn down while
still needed** (see **Regression** under *Exit code 137*), not a bad cache
archive.

### What to do

- Delete **`$(go env GOCACHE)`** and `rm -rf /tmp/go-build*` before rebuilding.
- **Root cause** of SIGSEGV crashes is a **kernel** issue (demand paging,
  icache coherency); see *Demand-paging icache invalidation bug* above.

### Why `rm -rf /.cache` only removes some files

The ext2 backend only allows **`rmdir` on empty directories** (only `.` and
`..` besides children).  POSIX `rm -rf` deletes **files first**, then removes
directories bottom-up; **any step that fails** leaves a non-empty parent, so
`rmdir` returns **DirectoryNotEmpty** and that subtree remains.

**Common reasons on Akuma:**

1. **Zombie or still-running `compile` / `go` processes** holding open file
   descriptors under `/.cache/go-build/`.  Unlink may fail or behave oddly
   until those processes are gone.  **Kill or wait for zombies** (see orphan
   fixes above), then retry `rm -rf`.
2. **Silent failures** from `rm` (dash): try `rm -rfv /.cache` to see which path
   errors.
3. **Partial run** after a crash — always delete the **same** directory
   `go env GOCACHE` prints (often `/.cache/go-build`).

**Recommended order:** ensure no `compile`/`go` processes are using the cache
→ `rm -rf /.cache/go-build` (or `$(go env GOCACHE)`) → run `go build` again.

## Use-after-free of page tables during `exit_group` (2026-03-22)

### Symptom

EL1 data abort (EC=0x25) with FAR=0x1000 during `go build`:

```
[Exception] Sync from EL1: EC=0x25, ISS=0xf
  ELR=0x402ca9dc, FAR=0x1000, SPSR=0x20402345
  Thread=21, TTBR0=0xb50000752f9000
  WARNING: Kernel accessing user-space address!
  EC=0x25 in kernel code — killing current process (EFAULT)
  Killing PID 328 (/usr/lib/go/pkg/tool/linux_arm64/compile)
```

The faulting instruction is `ldr w0, [x8]` where x8 = 0x1000 (`PROCESS_INFO_ADDR`).
This is `read_current_pid()` inlined into the syscall return path, attempting to
read the current process PID from the process info page.  The read faults because
TTBR0 points to page tables that have already been freed.

Killed compile processes cascade into Go build failures (`exit status 137`,
`signal: hangup`) and eventually stall the build entirely — Go waits in futex
for results that will never arrive.

### Root cause

When the Go runtime's `compile` subprocess creates CLONE_THREAD M-threads, each
gets a `UserAddressSpace` with `shared: true` (borrows the parent's L0 page
table).  The original process keeps `shared: false` (owns the page tables).

When any M-thread calls `exit_group`:

1. `sys_exit_group` → `kill_thread_group` marks all sibling threads as terminated
2. The calling thread returns through `return_to_kernel` → `deactivate()` (switches
   its own TTBR0 to boot tables) → `unregister_process` → `Drop<UserAddressSpace>`
3. If the **owner** (shared=false) drops, `Drop` frees all page table frames
4. Sibling threads still have their TTBR0 pointing to the now-freed page tables
5. When a sibling makes a syscall (or the scheduler resumes it), the kernel reads
   `PROCESS_INFO_ADDR` (0x1000) through TTBR0 → translation fault at EL1

The address 0x1000 is **not** a null pointer — it is the fixed address of the
process info page.  The fault occurs because the page table walk uses freed
physical pages that may have been recycled by the PMM.

### Fix (2026-03-22)

**Deferred page table freeing** (`crates/akuma-exec/src/mmu/mod.rs`):

Added a global `SHARED_L0_TABLE` (BTreeMap keyed by L0 physical address) that
tracks reference counts for shared address spaces:

- `new_shared(l0_phys)` increments the refcount for that L0 address
- Owner `Drop` (shared=false): if refcount > 0 (siblings still alive), **defers**
  freeing by storing the frame lists (`user_frames`, `page_table_frames`, `l0_frame`)
  in `SHARED_L0_TABLE` instead of freeing them
- Shared `Drop` (shared=true): decrements refcount.  If refcount reaches 0 **and**
  the owner already deferred, the last shared view frees all stored frames
- If the owner drops last (all shared views already gone), it frees immediately
  as before

This handles all drop orderings correctly:
- Owner drops first → defers; last sibling frees
- Siblings drop first → just decrement; owner frees normally

**Trampoline guard** (`crates/akuma-exec/src/process/mod.rs`):

Added a check in `entry_point_trampoline`: if the thread is already terminated
or the process is already marked `exited`, skip entering user mode entirely.
This prevents the race where `clone_thread` spawns a thread that gets scheduled
after `exit_group` has already killed the thread group.

## Kernel hang during `fork_process` mmap copy (under investigation)

### Symptom

During `go build`, the kernel freezes entirely — no heartbeat, no Thread0
cleanup, no timer ticks. The hang occurs **inside** `fork_process` during
the mmap region copy loop, at a non-deterministic page offset (different
region and page index each run). After the hang, no further kernel output
appears, indicating the timer interrupt has stopped firing.

Observable pattern from logs:
```
[FORK-DBG] mmap region 9/23 va=0xc2024000 pages=256
[FORK-PG] r9p0/256 va=0xc2024000
...
[FORK-PG] r9p160/256 va=0xc20c4000
<nothing further — kernel frozen>
```

Previous runs hung at different locations (e.g. region 20/22 page 64).
In all cases, `[TMR]` timer heartbeat stops appearing after the hang
point, confirming the timer interrupt itself has ceased.

### What's been ruled out

| Hypothesis | Evidence | Verdict |
|-----------|----------|---------|
| PMM exhaustion | PMM shows ~340K free pages (65% of 2 GB) at last stat | **Rejected** |
| EL1 data abort | No `EL1 SYNC EXCEPTION` during hang (only test-suite faults) | **Rejected** |
| Kernel panic | No panic output | **Rejected** |
| OOM in inner loop | `alloc_page_zeroed` never returns None | **Rejected** |
| Bad physical address | Bounds check (0x40000000–0xC0000000) never triggers | **Rejected** |
| Vec data race on `mmap_regions` | Snapshotting `.clone()` before iteration did **not** prevent hang | **Rejected** |

### Key observations

1. **Timer stops completely** — the `[TMR]` heartbeat (every 500 ticks /
   5 s) never prints again after the hang point. This means either IRQs
   are stuck disabled or the CPU is in an infinite loop inside an
   IRQ-disabled region.

2. **The hang always occurs during the alloc→copy→map inner loop** in
   `fork_process`, which calls:
   - `alloc_page_zeroed()` → `alloc_page()` → `with_irqs_disabled(|| PMM.lock())`
   - `copy_nonoverlapping` (4 KB memcpy)
   - `map_page` → `get_or_create_table` → may call `alloc_page_zeroed` again

3. **22 forks complete successfully** before the 23rd hangs, ruling out
   systematic bugs in the copy loop itself.

4. **Other threads are active during the fork** — epoll activity from
   Go worker threads appears interleaved with fork progress logs,
   confirming the scheduler is working up until the hang.

### Current hypotheses

- **Spinlock contention / IRQ masking**: `alloc_page()` acquires
  `PMM.lock()` inside `with_irqs_disabled`. If a timer fires just as
  IRQs are re-enabled and the scheduler switches to a thread that is
  somehow stuck (or another thread holds a contended lock), the fork
  thread may never be scheduled again.

- **Timer re-arm lost**: If the timer IRQ fires but `dispatch_irq`
  doesn't call the handler (e.g. `IRQ_HANDLERS` lock contention or
  spurious IRQ mishandling), the timer is not re-armed and no further
  preemption occurs.

- **Stack overflow during IRQ in fork context**: The fork thread is
  deep in kernel mode (syscall → clone → fork_process → inner loop).
  An 832-byte IRQ frame is pushed onto the kernel stack. If a nested
  SGI immediately follows (timer triggers SGI), two IRQ frames on
  the stack could overflow the 32 KB kernel stack under high nesting.

### Mitigations applied (testing)

- `mmap_regions.clone()` snapshot before iteration (protects against
  concurrent Vec modification, but did not fix the hang)
- `FORK_IN_PROGRESS` atomic flag with timer heartbeat every 50 ticks
  (500 ms) during fork — will reveal whether the timer stops during
  or before the hang

---

## 2GB RAM Identity Mapping Bug (2026-03-26) — **FIX APPLIED**

### Symptom

The kernel hung or crashed with SIGSEGV during `go build`'s memory-intensive operations. The hang occurred in `fork_process` when accessing memory in the second gigabyte of RAM (PA $\ge$ `0x8000_0000`).

### Root Cause

1.  **Identity Mapping Limit**: The kernel identity mapping in `TTBR0` was hardcoded to only cover the first 1GB of RAM. Accessing the second 1GB in EL1 (e.g., during `fork` memory copy) triggered unhandled exceptions.
2.  **VA Collision**: The `mmap` allocator could assign virtual addresses in the `0x8000_0000`–`0xC000_0000` range, which now conflicted with the 2GB identity mapping required for kernel operations.
3.  **IRQ Nesting Bug**: A flaw in `CriticalSection` (used by async timers) caused interrupts to be re-enabled prematurely during nested calls, leading to random freezes when high-frequency timers interleaved with IRQ-disabled regions.

### Fix Summary (2026-03-26)

1.  **Dynamic Identity Mapping**: Updated `crates/akuma-exec/src/mmu/mod.rs` to dynamically map the entire detected RAM range (up to 2GB or more) by calculating the required L1/L2 entries at runtime.
2.  **VA Range Reservation**: Updated `KERNEL_VA_END` to `0xC000_0000` in `crates/akuma-exec/src/process/types.rs`. This reserves the full 2GB identity-mapped range, preventing `mmap` from assigning conflicting user addresses.
3.  **CriticalSection Restoration**: Fixed the AArch64 `CriticalSection` implementation in `src/kernel_timer.rs` to use a nesting counter. This ensures `DAIF` (interrupt state) is only restored to its original value after the outermost `release()` call.
4.  **Enhanced Memory Monitoring**: Updated the `MemMonitor` heartbeat in `src/main.rs` to report both kernel heap usage and total system RAM (PMM) stats for better visibility into memory-intensive builds.
5.  **Heap Optimization**: Reduced the default kernel heap allocation from 1/4 of RAM to 1/16 (max 128MB), freeing up significantly more memory for user-space applications like the Go compiler.

### Verification

- **Test Suite**: Added `test_kernel_identity_mapping_full_ram` to `src/tests.rs` which verifies accessibility of high memory (PA `0x8000_0000`) from EL1.
- **Go Build**: Verified that `go build` no longer hangs in the fork loop or crashes with SIGSEGV when allocating beyond 1GB.
- **Stability**: Heartbeat logs now correctly show 2048MB RAM availability and stable system uptime without random freezes.
