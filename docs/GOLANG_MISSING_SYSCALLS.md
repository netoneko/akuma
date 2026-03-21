# Go Runtime — Missing / Incomplete Syscall Support

Tracked gaps and fixes required to run Go binaries on Akuma.

## Milestone Status

| Milestone | Status |
|-----------|--------|
| `CGO_ENABLED=0 go build -n` (dry run, no compilation) | **Fixed** (2026-03-21) |
| `CGO_ENABLED=0 go build` (actual compilation) | **In progress** — crashes during compilation |

---

## 1. Signal delivery ignores `SA_ONSTACK` — crash in Go runtime

**Status:** Fixed (2026-03-15) in `src/exceptions.rs`
**Component:** `src/exceptions.rs` — `try_deliver_signal`

### Symptom

Any Go binary that uses goroutines crashes immediately with:

```
signal 11 received but handler not on signal stack
mp.gsignal stack [0xa44a0000 0xa44a8000], mp.g0 stack [...], sp=0xa44e2ad8
fatal error: non-Go code set up signal handler without SA_ONSTACK flag

runtime.throw(...)
runtime.sigNotOnStack(...)
runtime.adjustSignalStack2(...)
runtime.adjustSignalStack(...)
runtime.sigtrampgo(...)
runtime.sigtramp()
```

### Root cause

`try_deliver_signal` in `src/exceptions.rs` always placed the signal frame on
the current goroutine stack (`sp_el0`):

```rust
let user_sp = frame_ref.sp_el0 as usize;
let new_sp = (user_sp - SIGFRAME_SIZE) & !0xF;
```

It never checked the `SA_ONSTACK` flag (`0x08000000`) in the registered signal
action, nor did it consult the process's `sigaltstack` fields
(`sigaltstack_sp`, `sigaltstack_size`).

Go's runtime startup sequence:

1. Calls `sigaltstack` to allocate a per-M (OS thread) alternate signal stack
   (the `gsignal` stack, ~32 KB).
2. Registers all its signal handlers (including SIGSEGV = 11) via
   `rt_sigaction` with `SA_ONSTACK | SA_SIGINFO | SA_RESTORER`.
3. When a signal fires, Go's `adjustSignalStack` verifies the current `sp`
   falls within the expected gsignal stack bounds. If not, it calls
   `sigNotOnStack` which throws a fatal error.

Because `sigaltstack` was stored (the kernel saved the fields) but never
*used* during signal delivery, every signal arrived on the goroutine stack
(e.g. `sp=0xa44e2ad8`) rather than the gsignal stack
(`[0xa44a0000, 0xa44a8000]`), triggering the fatal check.

### **Fix**

In `try_deliver_signal`, check `SA_ONSTACK` before choosing the stack to
deliver on:

```rust
const SA_ONSTACK: u64 = 0x08000000;

let stack_top = if (action.flags & SA_ONSTACK) != 0
    && proc.sigaltstack_sp != 0
    && proc.sigaltstack_size >= SIGFRAME_SIZE as u64
{
    (proc.sigaltstack_sp + proc.sigaltstack_size) as usize
} else {
    user_sp
};
let new_sp = (stack_top - SIGFRAME_SIZE) & !0xF;
```

`sigaltstack` was already correctly implemented (`sys_sigaltstack` in
`src/syscall/signal.rs` stores `sigaltstack_sp / sigaltstack_flags /
sigaltstack_size` in the process struct); the only missing piece was honouring
those fields at signal-delivery time.

---

## 2. Re-entrant SIGSEGV — infinite signal delivery loop

**Status:** Fixed (2026-03-15) in `src/exceptions.rs`
**Component:** `src/exceptions.rs` — `try_deliver_signal`

### Symptom

After fix #1, Go binaries that fault inside their own signal handler (e.g.
when the handler accesses an unmapped runtime data structure) produce an
infinite loop of kernel log lines:

```
[WILD-DA] pid=53 FAR=0xa2597bd8 ELR=0x48ff14 last_sc=98
[signal] Delivering sig 11 to handler 0x48fb20 (restorer=0x1a43a1c)
[DP] no lazy region for FAR=0xa2597bd8 pid=53 (pid has 21 lazy regions)
[WILD-DA] pid=53 FAR=0xa2597bd8 ELR=0x48ff14 last_sc=98
[signal] Delivering sig 11 to handler 0x48fb20 ...
```

The kernel re-delivers SIGSEGV indefinitely because `rt_sigreturn` restores
the context to the faulting instruction, which immediately faults again.

### Root cause

On Linux, signals are masked during handler execution (unless `SA_NODEFER` is
set), so a second delivery of the same signal goes to the default action
(process termination). Akuma did not implement this masking, so re-entrant
faults looped forever instead of terminating the process.

### **Fix**

At the top of `try_deliver_signal`, detect re-entrant delivery by checking
whether the current `sp_el0` already falls within the sigaltstack range. If it
does, we are already inside a signal handler, and delivering again would loop.
Return `false` instead, which causes the caller to kill the process:

```rust
if proc.sigaltstack_sp != 0 {
    let alt_lo = proc.sigaltstack_sp as usize;
    let alt_hi = alt_lo + proc.sigaltstack_size as usize;
    if user_sp >= alt_lo && user_sp < alt_hi {
        // re-entrant fault — kill process instead of looping
        return false;
    }
}
```

---

## 3. Kernel heap exhaustion — `go build` panics the kernel

**Status:** Fixed (2026-03-15) in `src/main.rs`, `src/allocator.rs`
**Component:** kernel heap sizing, `#[alloc_error_handler]`

### Symptom

Running `go build` exhausts the kernel heap, then panics the entire kernel:

```
[ALLOC FAIL] requested=4096 heap_total=16MB heap_used=15MB (99%) peak=15MB allocs=58906
!!! PANIC !!!
Message: memory allocation of 4096 bytes failed
```

### Root causes

Two independent issues:

**A — Heap too small.** The kernel heap was hardcoded to 16 MB regardless of
available RAM. `go build` spawns many processes and opens many files,
exhausting kernel metadata allocations quickly. Per `CLAUDE.md`, the intended
sizing is 1/4 of available RAM (e.g. 256 MB with 1 GB QEMU RAM).

**B — OOM panics the kernel.** When `GlobalAlloc::alloc` returns null, Rust's
default `handle_alloc_error` panics, taking down the entire kernel rather than
just the offending process.

### **Fix**

**A — Dynamic heap sizing** (`src/main.rs`):

```rust
// was: const KERNEL_HEAP_SIZE: usize = 16 * 1024 * 1024;
let heap_size = core::cmp::max(ram_size / 4, 64 * 1024 * 1024);
```

**B — OOM kills the process** (`src/allocator.rs`):

```rust
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    // print stats ...
    if akuma_exec::process::current_process().is_some() {
        akuma_exec::process::return_to_kernel(-12); // ENOMEM
    }
    panic!("kernel OOM: allocation of {} bytes failed", layout.size());
}
```

If there is a current userspace process, the kernel kills it and returns
normally. Pure kernel-context OOM (no current process) still panics, since
there is nothing else to do.

---

## 4. EPOLLET edge not reset after `read()` on TCP sockets — model response hang

**Status:** Fixed (2026-03-15) in `src/syscall/fs.rs`
**Component:** `src/syscall/fs.rs` — `sys_read` socket path

### Symptom

Go programs making streaming HTTP/TLS connections (e.g. crush waiting for a
model API response) stall indefinitely mid-stream. The kernel log shows
`epoll_pwait` being called repeatedly with `timeout=0ms` (returning 0 events)
even though data is buffered in the TCP socket.

### Root cause

The EPOLLET (edge-triggered) logic in `sys_epoll_pwait` only fires an `EPOLLIN`
event when `revents & !last_ready` has new bits. It records `last_ready =
revents` on every poll iteration. After the first event fires, `last_ready =
EPOLLIN`.

`recvfrom` and `recvmsg` already called `epoll_on_fd_drained()` after every
successful read to clear the EPOLLIN bit in `last_ready`. This was added earlier
to handle BoringSSL/bun which reads one TLS record at a time without draining to
EAGAIN.

Go uses `read()` (not `recvfrom`/`recvmsg`) for TCP sockets, and reads one
chunk, then goes back to epoll before draining. The `sys_read` socket path only
called `epoll_on_fd_drained()` on EAGAIN. So after the first read:

1. epoll fires EPOLLIN → `last_ready = EPOLLIN`
2. Go reads one chunk via `read()` — more data remains, no EAGAIN
3. Go's netpoller polls epoll again (timeout=0)
4. `revents = EPOLLIN`, `last_ready = EPOLLIN`, `new_bits = 0` → **no event**
5. Go thinks socket idle, stops reading → **hang**

### **Fix**

Call `epoll_on_fd_drained()` after every successful TCP `read()` in `sys_read`,
matching what `recvfrom`/`recvmsg` already do:

```rust
Ok(n) => {
    // ...copy to user...
    // Reset EPOLLET edge after every successful TCP read. Go does not
    // drain to EAGAIN before going back to epoll.
    if !socket::is_udp_socket(idx) {
        super::poll::epoll_on_fd_drained(fd_num as u32);
    }
    n as u64
}
```

UDP is excluded because UDP sockets do not have a byte-stream; each `read`
drains an entire datagram, so the edge logic is already correct.

---

## 5. `restart_syscall` (nr=128) returns ENOSYS — Go runtime crash after signal

**Status:** Fixed (2026-03-15) in `src/syscall/mod.rs`
**Component:** `src/syscall/mod.rs` — `handle_syscall`

### Symptom

Go binaries crash after receiving a signal, with a log line indicating
a restart syscall failed:

```
runtime: unexpected return value from restart_syscall: -38
```

`-38` is ENOSYS.

### Root cause

When a signal handler is invoked while a process is in a blocking syscall,
the syscall returns `-EINTR`. Some signal handlers can be marked with `SA_RESTART`,
which tells the kernel to automatically restart the interrupted syscall.

In Akuma, this is implemented by having the signal handler return a special
magic number `restart_syscall` (128). The kernel's syscall return path is
supposed to see this and re-issue the original syscall.

The `handle_syscall` match statement was missing an arm for 128, so it fell
through to the `_ => { ... ENOSYS ... }` case.

### **Fix**

Add an explicit case for 128 (`restart_syscall`) that returns `EINTR`. This
is what Linux does; the userspace `libc` wrapper is responsible for the actual
re-issuing of the syscall.

```rust
// in handle_syscall()
...
128 => { // restart_syscall
    -(EINTR as i64) as u64
}
...
```
*Correction*: The above fix is what was initially tried. The correct fix is
more nuanced. The kernel *should* restart it. The `-EINTR` return is what
happens when `SA_RESTART` is *not* set. The true fix involved modifying the
signal delivery logic to adjust the saved `ELR` to re-execute the `SVC`
instruction (see later section). However, explicitly handling 128 and
returning EINTR is a valid interim fix that unblocks Go.

---

## 6. `waitid` (syscall 95) is a stub — `go build` crashes waiting for child processes

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs` + `src/syscall/mod.rs`
**Component:** `sys_waitid`, `ProcessChannel`

### Symptom

`go build` spawns compiler processes (`compile`, `link`, `asm`), then crashes
when trying to wait for them to finish. The kernel log shows:

```
[ENOSYS] nr=95 (waitid) a0=... a1=... a2=...
```

### Root cause

`sys_waitid` was a stub that always returned `ENOSYS`. Go 1.14+ uses `waitid`
(not `wait4`) as its primary mechanism for waiting on child processes. It is
more flexible than `wait4` and allows waiting on specific PIDs, process groups,
or using `pidfd`s.

### **Fix**

Implement `sys_waitid` as a wrapper over the existing child-channel
infrastructure (used by `wait4`).

`sys_waitid` looks up the child's `ProcessChannel` and calls
`channel.wait_for_child_exit_with_pid(pid)`. This either returns the exit code
immediately (if the child is already a zombie) or blocks the parent until the
child calls `exit_group`. The `rusage` and `siginfo_t` arguments are ignored
for now but the call signature is honored.

---

## 7. `pidfd_open` (syscall 434) + `waitid(P_PIDFD)` — Go busy-polls with nanosleep

**Status:** Fixed (2026-03-15) in `src/syscall/pidfd.rs` + `src/syscall/proc.rs`
**Component:** `sys_pidfd_open`, `epoll` integration

### Symptom

After `waitid` was implemented, `go build` still exhibited high CPU usage and
slow performance. `strace` (or Akuma's equivalent `/proc/pid/syscalls`) revealed
a tight loop:

```
epoll_pwait(timeout=0) = 0
nanosleep(1ms)
epoll_pwait(timeout=0) = 0
nanosleep(1ms)
...
```

Go was using `epoll` to wait for child processes to exit, but the `pidfd`s it
was polling never became ready. Go fell back to a busy-poll with `nanosleep`.

### Root cause

Go 1.15+ uses a modern Linux pattern for waiting:

1. Call `clone` with `CLONE_PIDFD` to get a file descriptor for the child process.
2. Add this `pidfd` to an `epoll` set.
3. Call `epoll_pwait`. The kernel makes the `pidfd` readable when the child exits.
4. Call `waitid` on the `pidfd` to reap the exit code.

Three syscalls were missing or incomplete:

- `sys_clone3`: Did not handle `CLONE_PIDFD` flag.
- `sys_pidfd_open`: Was a stub (`ENOSYS`). This is an alternative to `CLONE_PIDFD`.
- `epoll`: Did not know how to check the readiness of a `pidfd`.

### **Fix**

- **`sys_clone3`** (`src/syscall/proc.rs`): If `CLONE_PIDFD` is set, create a `pidfd`
  (`FileDescriptor::PidFd`) and write it back to the user-provided pointer.
- **`sys_pidfd_open`** (`src/syscall/pidfd.rs`): Full implementation. Creates a `PidFd`
  pointing to the given PID, returning `ESRCH` if the PID doesn't exist.
- **`epoll` readiness** (`src/syscall/poll.rs`): In `epoll_check_fd_readiness`,
  add a case for `FileDescriptor::PidFd`. It checks if the underlying process
  is a zombie (`proc.is_zombie(pid)`). If so, it marks the fd as `EPOLLIN`.

---

## 8. `CLONE_PIDFD` not handled — Go netpoller uses garbage fd

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs`
**Component:** `sys_clone3`

### Symptom

After the `pidfd` fixes, Go's netpoller sometimes crashed with EBADF on `epoll_ctl`
or other fd-related syscalls.

### Root cause

The `CLONE_PIDFD` flag in `sys_clone3` takes a pointer argument where the kernel
is expected to write the new `pidfd`. The implementation was creating the fd
but *not* writing it back to the user's pointer. The Go runtime was left with
an uninitialized variable (a garbage fd number) in its netpoller state.

### **Fix**

In `sys_clone3`, after creating the `PidFd` and adding it to the parent's fd table,
copy the resulting fd number back to the user:

```rust
if (clone_args.flags & CLONE_PIDFD) != 0 {
    // ... create pidfd, get fd_num ...
    if copy_to_user(clone_args.pidfd, &[fd_num as u64]).is_err() {
        // ... handle error ...
    }
}
```

---

## 9. `MOUNT_TABLE` spinlock held during disk I/O — deadlock risk

**Status:** Fixed (2026-03-15) in `src/vfs/mod.rs`
**Component:** `src/vfs/mod.rs` — `resolve_path`

### Symptom

The kernel would occasionally deadlock under high I/O load, especially when
multiple processes were accessing files on different filesystems.

### Root cause

`resolve_path` took a lock on the global `MOUNT_TABLE` to find the correct
filesystem for a given path. It then called the filesystem's `open` or
`metadata` method *while still holding the lock*.

If the filesystem implementation performed any blocking I/O (e.g. reading
from the VirtIO block device), the CPU could be rescheduled to another process.
If that new process also tried to resolve a path, it would try to acquire the
`MOUNT_TABLE` lock and deadlock.

### **Fix**

Refactor `resolve_path` to release the mount table lock before calling into
the filesystem-specific code.

```rust
// Before
let fs = MOUNT_TABLE.lock().find_fs(path); // lock held
fs.open(path_suffix); // I/O inside lock

// After
let fs_clone = { MOUNT_TABLE.lock().find_fs(path).cloned() }; // lock released
if let Some(fs) = fs_clone {
    fs.open(path_suffix); // I/O outside lock
}
```
This required adding `Clone` support to the `FileSystem` trait object,
which is typically done via `dyn-clone`.

---

## 10. Signal state not reset on `execve` — stale handlers from shell

**Status:** Fixed (2026-03-15) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `execve` implementation

### Symptom

A Go binary launched from the shell would crash on certain signals, even though
it registered its own handlers. The crash signature suggested a default action
was being taken instead of the Go handler being invoked.

### Root cause

Per POSIX, `execve` must reset all signal dispositions to their default (`SIG_DFL`),
except for signals that are ignored (`SIG_IGN`), which remain ignored. The `sigaltstack`
must also be disabled.

The Akuma `execve` implementation was preserving the signal action table and
the `sigaltstack` state from the parent process (the shell). When the Go binary
ran, it inherited the shell's simpler signal handlers.

### **Fix**

In the `execve` path, after loading the new ELF binary but before running it,
explicitly reset the signal state:

```rust
// in akuma_exec::process::Process::execve
// ...
// Reset signal handlers to default
proc.signal_actions.lock().fill(Default::default());
// Disable alternate signal stack
proc.sigaltstack_sp = 0;
proc.sigaltstack_size = 0;
proc.sigaltstack_flags = 0;
// ...
```

---

## 11. `tgkill` (syscall 131) returns ENOSYS

**Status:** Fixed (2026-03-15) in `src/syscall/signal.rs`
**Component:** `sys_tgkill`

### Symptom

Go's runtime uses `tgkill` to send signals to specific threads within its own
process group (e.g. for goroutine preemption). This was failing with `ENOSYS`.

```
[ENOSYS] nr=131 (tgkill)
```

### Root cause

`sys_tgkill` was a stub. `tgkill(pid, tid, sig)` is equivalent to `tkill(tid, sig)`
but with an extra check that `tid` is in the thread group of `pid`.

### **Fix**

Implement `sys_tgkill`. It finds the process associated with `pid` and verifies
that the thread `tid` belongs to it. If so, it forwards the call to `sys_tkill`.

```rust
pub fn sys_tgkill(pid: u32, tid: u32, sig: i32) -> u64 {
    // Find process for pid ...
    // Verify tid is in pid's thread group ...
    sys_tkill(tid, sig)
}
```

---

## 12. `msgget`/`msgctl`/`msgsnd`/`msgrcv` (syscalls 186-189)

**Status:** Implemented (2026-03-16) in `src/syscall/msgqueue.rs`
**Component:** SysV Message Queues

### Symptom

`go build` running inside a container (with IPC namespace enabled) failed with
`ENOSYS` for syscalls 186-189.

```
[ENOSYS] nr=187 (msgctl)
[ENOSYS] nr=186 (msgget)
...
```
These are the System V message queue syscalls. They are an older form of IPC,
but are still used by some build tools and legacy applications.

### Root cause

These four syscalls were completely unimplemented.

### **Fix**

A full implementation of SysV message queues was added. This involved:

- A new `src/syscall/msgqueue.rs` module.
- A global `MESSAGE_QUEUES` table, keyed by queue ID.
- `sys_msgget`: Creates or opens a message queue.
- `sys_msgsnd`: Sends a message to a queue. If the queue is full, the process
  blocks until there is space.
- `sys_msgrcv`: Receives a message from a queue. If the queue is empty, the
  process blocks.
- `sys_msgctl`: Control operations (get/set queue properties, remove queue).
- Integration with process isolation: Message queues are associated with the
  `ipc_box` of the process that created them.

---

## 13. `CLONE_VFORK` does not block parent — race condition in `go build`

**Status:** Fixed (2026-03-16) in `src/syscall/proc.rs`
**Component:** `sys_clone3` VFORK path

### Symptom

`go build` would intermittently fail with file-not-found errors or other
races related to process creation. A child process would seem to execute
before the parent had finished its post-fork setup.

### Root cause

The `CLONE_VFORK` flag requires the parent process to be suspended until the
child either calls `execve` or `exit`. This is a performance optimization used
by shells and `posix_spawn` to avoid copying the parent's address space when
it's known the child will immediately replace it.

The Akuma `sys_clone3` implementation was ignoring `CLONE_VFORK`'s blocking
semantic. The parent and child ran concurrently, leading to races.

### **Fix**

A `VFORK_WAITERS` global `Spinlock<BTreeMap<u32, Waker>>` was added.

- **`sys_clone3` (parent):** If `CLONE_VFORK` is set, it registers the current
  thread's waker in `VFORK_WAITERS` with the child's PID as the key. It then
  blocks (`schedule_blocking`).
- **`execve` (child):** After successfully loading the new binary, it checks if
  it was a vforked child. If so, it finds the parent's waker in
  `VFORK_WAITERS` and calls `wake()`.
- **`exit_group` (child):** Similarly, on exit, it wakes the vfork parent.

This ensures the parent remains blocked until the child's fate is sealed.

---

## 14. `go build` deadlocks — goroutine scheduler eventfd event missing

**Status:** Fixed (2026-03-16) in `src/syscall/sync.rs`
**Component:** `sys_futex` (`FUTEX_WAIT` path)

### Symptom

`go build` would deadlock. `ps` showed multiple `compile` processes in `Blocked`
state, waiting on a futex. The parent `go` process was also blocked, waiting
on a futex.

### Root cause

A classic futex missed-wakeup race condition. Go's scheduler uses futexes to
park and unpark M's (OS threads).

The sequence was:
1. **Thread A (Go scheduler):** Decides to park Thread B. It checks the futex
   value at `uaddr`. Let's say it's 0.
2. **Thread B (worker):** Is about to go to sleep. It sets the futex value at
   `uaddr` to 1 (to indicate it's sleeping).
3. **Thread A (Go scheduler):** Sees the value is still 0 (a stale read). It decides
   no wakeup is needed and moves on.
4. **Thread B (worker):** Calls `sys_futex(FUTEX_WAIT, uaddr, 1)`. The kernel
   sees the value is indeed 1 and puts the thread to sleep.

The wakeup from Thread A happened *before* the sleep from Thread B. Thread B
is now asleep forever.

The kernel's `sys_futex` implementation had this same race:

```rust
// in sys_futex(FUTEX_WAIT)
let val = *uaddr_ptr; // Read value from userspace
if val != expected_val {
    return EAGAIN; // Value changed, don't sleep
}
// Now, block the thread... but a wakeup could happen right here!
schedule_blocking(...);
```

### **Fix**

The fix, standard in all futex implementations, is to perform the value check
*atomically* with registering the intent to sleep. This is typically done by
holding a lock across the check and the sleep registration.

The `futex_wait` function was refactored to take a lock on a hash bucket
(derived from `uaddr`). The value is read *inside the lock*. The thread is
added to a wait queue for that bucket. Then `schedule_blocking` is called.

The `futex_wake` function takes the same lock, wakes the threads in the wait
queue, and releases the lock. This prevents a `wake` from occurring between
the `wait`'s check and its sleep.

---

## 15–18. Signal frame corruption, VFORK race, user_va_limit

**Status:** Fixed (2026-03-16)

This was a batch of smaller fixes:

- **`rt_sigreturn` state restoration:** The `rt_sigreturn` syscall was not
  restoring the signal mask (`uc_sigmask`) or the floating-point/SIMD registers
  (`uc_mcontext.fpsimd_context`). This corrupted the state of signal handlers
  and any code they called. The fix was to correctly copy these fields from
  the `ucontext` on the stack back to the process state.

- **`fork` vs `vfork_complete` race:** A race existed where a parent could `fork`
  a new child while another child was completing its `vfork` (`execve`/`exit`),
  leading to inconsistent state in the `VFORK_WAITERS` map. This was fixed by
  pre-inserting a placeholder into `VFORK_WAITERS` during `clone` *before* the
  child runs, rather than having the child do it.

- **`user_va_limit` increase:** The user virtual address space limit was too small,
  causing `mmap` to fail for large Go binaries. This was increased to a 48-bit
  address space, matching modern ARM64 configurations.

---

## 19. Pipe refcount race in `dup3` / `fcntl` — premature pipe destruction

**Status:** Fixed (2026-03-17) in `src/syscall/fs.rs`, `src/syscall/pipe.rs`
**Component:** `sys_dup3`, `fcntl(F_DUPFD)`, `KernelPipe` refcounting

### Symptom

`go build` failed with broken pipe errors or hangs. The logs showed that
pipes used for communication between the `go` command and its `compile`
subprocesses were being destroyed prematurely.

```
[pipe] write WARN: pipe id=5 not found
```
The child `compile` process would try to write its output to stdout (which was
a pipe), but the pipe no longer existed in the kernel's global `PIPES` table.

### Root cause

Incorrect reference counting on `KernelPipe` objects when file descriptors
were duplicated. A `KernelPipe` has a `read_count` and a `write_count`. It is
destroyed when both drop to zero.

Go's `Command` setup does something like this:
1. `pipe2()` -> creates `fd_r`, `fd_w` (pipe refcounts = 1, 1)
2. `fork()` -> child inherits fds (refcounts still 1, 1 in kernel, but now two processes point to them)
3. **Parent:** `close(fd_w)` (refcount w=0). Sets up to read from `fd_r`.
4. **Child:** `dup3(fd_w, 1)` (to make it stdout), `close(fd_r)`, `close(fd_w)`.
   Then `execve("compile")`.

The problem was in step 4. `sys_dup3(old, new)` was implemented as:
- `close(new)` -> decrements refcount of whatever was at `new`
- `fds[new] = fds[old]` -> copies the `FileDescriptor` enum
- *Missing:* Increment the refcount of the underlying pipe.

When the child later closed the original `fd_w`, the `write_count` would drop
to 0. When the parent also closed its original `fd_w`, the pipe might be
destroyed, even though the child's `STDOUT` (fd 1) was still supposed to be a
valid writer. `fcntl(F_DUPFD)` had the same bug.

### **Fix**

- **Atomic `dup3`:** `sys_dup3` was rewritten to be atomic (with respect to other
  fd operations) using a spinlock on the `fd_table`. It now correctly handles
  the case where `oldfd == newfd` (it should be a no-op).
- **Explicit refcount bumps:** A `pipe_clone_ref(&FileDescriptor)` helper was
  added. `sys_dup3` and `fcntl(F_DUPFD)` now call this helper on the duplicated
  `FileDescriptor::PipeWrite` or `PipeRead` to correctly increment the underlying
  `read_count` or `write_count`.

The lifecycle now looks correct:

| Action | `read_count` | `write_count` |
|---|---|---|
| `pipe2` | 1 | 1 |
| parent forks | 1 | 1 |
| child `dup3(fd_w, 1)` → `pipe_clone_ref` | 1 | 2 |
| child closes fd_w | 1 | 1 |
| parent closes fd_w | 1 | 0 |
| parent closes fd_r_dup | 1 | 0 |
| parent closes fd_r | 0 | 0 → pipe destroyed |

### Kernel tests added

- `test_pipe_dupfd_bumps_refcount` — verifies F_DUPFD_CLOEXEC refcount semantics: writing to the pipe after closing the original fd (but not the duplicate) must succeed.
- `test_pipe_dup3_atomically_replaces_and_closes_old` — verifies the dup3 replacement path properly closes the displaced old entry and keeps the new entry alive.

---

---

## 20. Stale `PENDING_SIGNAL` on thread slot reuse — EL1 data abort in innocent process

**Status:** Fixed (2026-03-17) in `crates/akuma-exec/src/threading/mod.rs`
**Component:** `PENDING_SIGNAL` array, thread slot cleanup path

### Symptom

The `parallel_processes` threading test (which spawns multiple `/bin/hello` processes)
crashed with an EL1 data abort:

```
[Exception] Sync from EL1: EC=0x25, ISS=0x7
  ELR=0x403c29c0, FAR=0x0, SPSR=0x60000345
  Thread=8, ...
  Instruction at ELR: 0xa94007c0
  Likely: Rn(base)=x30, Rt(dest)=x0   ← LDP X0,X1,[X30] with X30=0 (null)
  EC=0x25 in kernel code — killing current process (EFAULT)
  Killing PID 20 (/bin/hello)
```

Process `/bin/hello` (PID 20) exited with code −14 (EFAULT).

### Root cause

`PENDING_SIGNAL[tid]` is an `AtomicU32` array (one slot per thread) used to defer
signal delivery from `sys_tkill` to the next syscall return of the target thread.

When a Go goroutine (e.g. running `go build`) received `SIGURG` (goroutine
preemption), `sys_tkill` stored the signal number in `PENDING_SIGNAL[tid]`.
If that goroutine's thread slot was later recycled (TERMINATED → INITIALIZING → FREE)
**without clearing `PENDING_SIGNAL[tid]`**, the next process that ran on the same
thread slot would see the stale signal at its first syscall boundary.

`take_pending_signal()` would return `SIGURG` (23). `try_deliver_signal` would be
called for `/bin/hello`, which has no registered signal handler. However, the
`ensure_sigreturn_trampoline` path triggered, reaching kernel code with X30 = 0 in
an unexpected code path, causing a null-pointer dereference → EL1 data abort → EFAULT.

### **Fix**

In the slot-recycling cleanup loop in `cleanup_terminated_threads()`, after zeroing the
thread context and clearing slot metadata, add:

```rust
// Clear any pending signal from the previous occupant of this slot.
PENDING_SIGNAL[i].store(0, Ordering::Release);
```

This prevents stale signals from crossing process boundaries when thread slots are reused.

### Lesson

Any per-thread kernel state that was set by one process must be explicitly cleared
during thread slot recycling, before the slot is made available (set to FREE) again.
The `PENDING_SIGNAL` array must be treated with the same discipline as `THREAD_CONTEXTS`.

---

## 21. `pipe_write` silently returns 0 on destroyed pipe — `compile -V=full` empty output regression

**Status:** Fixed (2026-03-17) in `src/syscall/pipe.rs`, `src/syscall/fs.rs`
**Component:** `pipe_write` return type, `sys_write` PipeWrite error handling

### Symptom

```
go: parsing buildID from go tool compile -V=full: unexpected output:
[exit code: 1]
```

The kernel log shows:

```
[pipe] write WARN: pipe id=12 not found (len=25)
[syscall] write WARN: PipeWrite fd=1 pipe_id=12 lost 25 bytes (pipe gone)
[exit_group] pid=52 name=compile code=0 after 1.85s
```

The compile child ran successfully (exit code 0), but its final stdout write
(the build ID output, 25 bytes) was silently discarded. The parent Go process
read the pipe and got empty output.

### Root cause

`pipe_write` returned `0usize` when the pipe was not found in the global
`PIPES` table (already destroyed). `sys_write` treated this as a successful
short write — zero bytes written, no error. On Linux, writing to a broken
pipe (no readers) delivers `SIGPIPE` and the `write()` syscall returns
`-EPIPE` (errno 32).

The pipe destruction itself may have resulted from the refcount races fixed
in §19, or from legitimate lifecycle ordering (all readers closed before the
writer's final flush). Regardless of the root cause of the pipe's absence,
the kernel must not silently discard data — it must report an error so the
caller can observe the failure.

### Debugging with `/proc/<pid>/syscalls`

The fix was diagnosed using the per-process syscall ring buffer at
`/proc/<pid>/syscalls`. This facility logs the last N syscalls per process
with their number, arguments, return value, and duration. To use it:

```
cat /proc/<pid>/syscalls
```

For the `compile -V=full` probe, the key evidence was:
- Pipe write syscalls (nr=64) returning 0 instead of the expected byte count
- The `[pipe] write WARN` kernel message correlating with the zero-return write
- `exit_group` (nr=94) immediately after, with code 0

### **Fix**

**`pipe_write` return type change** (`src/syscall/pipe.rs`):

Changed from `fn pipe_write(id, data) -> usize` to
`fn pipe_write(id, data) -> Result<usize, i32>`.

- If the pipe exists and has readers (`read_count > 0`): returns `Ok(n)`.
- If the pipe exists but has no readers (`read_count == 0`): returns
  `Err(EPIPE)` — matches Linux behavior where writing to a pipe with no
  readers produces `SIGPIPE`/`EPIPE`.
- If the pipe is not found (already destroyed): returns `Err(EPIPE)`.

**`sys_write` EPIPE propagation** (`src/syscall/fs.rs`):

The `PipeWrite` arm now matches on the `Result`:

```rust
match super::pipe::pipe_write(pipe_id, buf_slice) {
    Ok(n) => n as u64,
    Err(e) => {
        if total_written > 0 { return total_written as u64; }
        return (-(e as i64)) as u64;  // -EPIPE
    }
}
```

If some data was already written in a multi-chunk write, the partial count
is returned (matching Linux short-write semantics). Otherwise `-EPIPE` is
returned directly.

### Kernel tests added

Four new tests in `src/tests.rs`:

| Test | What it verifies |
|---|---|
| `test_pipe_write_to_destroyed_pipe_returns_epipe` | Writing to a fully destroyed pipe returns `Err(32)` (EPIPE), not `Ok(0)` |
| `test_pipe_write_no_readers_returns_epipe` | Writing to a pipe with `read_count=0` returns `Err(EPIPE)` |
| `test_pipe_write_with_readers_succeeds` | Writing to a pipe with active readers returns `Ok(n)` and data round-trips correctly |
| `test_pipe_cloexec_cleanup_preserves_live_writer` | Simulates the full fork→dup→cloexec→write→read lifecycle; verifies the dup'd writer survives cloexec cleanup |

Existing tests in `src/process_tests.rs` were updated to match the new
`Result` return type:

| Test | Change |
|---|---|
| `test_pipe_write_missing_returns_zero` | Renamed to `test_pipe_write_missing_returns_epipe`; now asserts `Err(EPIPE)` instead of `0` |
| `test_pipe_write_survives_read_close` | Renamed to `test_pipe_write_returns_epipe_after_read_close`; now asserts `Err(EPIPE)` when `read_count=0` |
| `test_pipe_dup3_atomically_replaces_and_closes_old` | Updated to expect `Err(EPIPE)` for pipe_a write (read_count=0) |
| Other pipe tests | Updated to unwrap `Result` from `pipe_write` |

---

## 23. Process unregistration race in `exit_group` — `ENOSYS` or crash in siblings

**Status:** Fixed (2026-03-19) in `crates/akuma-exec/src/process/mod.rs` and `crates/akuma-exec/src/threading/mod.rs`
**Component:** `kill_thread_group`, `on_thread_cleanup`

### Symptom

Go binaries crash or return `ENOSYS` for syscalls like `rt_sigaction` during process exit. The kernel log might show `[ENOSYS] nr=13` (rt_sigaction) or `[ENOSYS] nr=220` (clone) for a process that is supposed to be exiting.

### Root cause

`kill_thread_group` (used by `exit_group`) was immediately unregistering sibling processes and removing them from `THREAD_PID_MAP`. If a sibling thread was still running and attempted a syscall (e.g., Go's signal handler setup during exit), `current_process()` would return `None`, leading to `ENOSYS` or a crash.

### **Fix**

Implemented a deferred cleanup mechanism:
1. `kill_thread_group` now marks siblings as `Zombie` and wakes them, but does **not** unregister them.
2. Added a `CLEANUP_CALLBACK` in the threading module, invoked when a thread slot is recycled.
3. The process module registers `on_thread_cleanup` as this callback. It removes the thread from `THREAD_PID_MAP` and only unregisters the `Process` (dropping its `Box` and memory) when the **last** thread of that PID has exited.

---

## 24. `go build` hangs — `pipe_write` missing epoll notifications

**Status:** Fixed (2026-03-19) in `src/syscall/pipe.rs` and `src/syscall/poll.rs`
**Component:** `pipe_write`, `epoll_check_fd_readiness`

### Symptom

`go build` hangs indefinitely while waiting for `compile` or `link` subprocesses. `ps` shows processes in `Blocked` or `Running` state but no progress is made.

### Root cause

`pipe_write` only woke up threads explicitly stored in `reader_thread` (used for blocking `read` syscalls). It did not notify threads waiting via `epoll` or `poll`. Go's netpoller (which handles pipe I/O) uses `epoll_pwait` and was never woken up when data arrived in the pipe.

### **Fix**

1. Added a `pollers` set (`BTreeSet<usize>`) to `KernelPipe` to track threads interested in the pipe.
2. Updated `epoll_check_fd_readiness` for `PipeRead` to register the current thread via `pipe_add_poller`.
3. Updated `pipe_write` and `pipe_close_write` to wake all threads in the `pollers` set.

---

## 25. Missing `SIGPIPE` delivery — crash in Go on broken pipe

**Status:** Fixed (2026-03-19) in `src/syscall/pipe.rs` and `src/syscall/signal.rs`
**Component:** `pipe_write`, `send_sigpipe`

### Symptom

Go binaries crash with unexpected errors or WILD-DA when a pipe they are writing to is closed by the reader.

### Root cause

Linux requires that writing to a pipe with no readers delivers `SIGPIPE` to the calling thread. Akuma was returning `-EPIPE` but not sending the signal. Go expects the signal to be delivered to trigger its internal handler.

### **Fix**

1. Added `send_sigpipe()` helper in `src/syscall/signal.rs` to send signal 13 to the current thread.
2. Updated `pipe_write` to call `send_sigpipe()` when `read_count == 0`.

---

## 27. Signal delivery ignores signal mask — re-entrant crash in Go

**Status:** Fixed (2026-03-19) in `src/exceptions.rs` and `crates/akuma-exec/src/threading/mod.rs`
**Component:** `take_pending_signal`, `rust_sync_el0_handler`

### Symptom

Go binaries crash with `signal: broken pipe` or exit code -13 during high-frequency signal delivery (e.g., `SIGURG` preemption or `SIGPIPE`). Kernel logs show re-entrant delivery of the same signal to the same handler before the first one has finished.

### Root cause

`take_pending_signal()` blindly returned any pending signal regardless of whether it was blocked by the process's `signal_mask`. In Go, when a `SIGPIPE` handler is running, signal 13 is automatically blocked. If the handler itself triggered another `SIGPIPE`, the kernel would immediately re-deliver it, causing a re-entrant fault and process termination.

### **Fix**

1.  Updated `akuma_exec::threading::take_pending_signal(mask)` to accept a 64-bit mask and skip signals that are set in the mask (except for `SIGKILL` and `SIGSTOP`).
2.  Updated `rust_sync_el0_handler` in `src/exceptions.rs` to look up the current process's `signal_mask` and pass it to `take_pending_signal()`.
3.  Added regression test `test_signal_masking` in `src/process_tests.rs`.

---

## 29. `tkill` uses incorrect signal table — termination of Go binaries on `SIGPIPE`

**Status:** Fixed (2026-03-19) in `src/syscall/signal.rs`
**Component:** `sys_tkill`, `sys_tgkill`

### Symptom

Go binaries would occasionally exit with code -13 (SIGPIPE) even when a handler was registered and correctly masked. This was particularly visible when one process sent a signal to another (e.g., `go` sending `SIGURG` to `compile`).

### Root cause

`sys_tkill` was using the signal table of the *calling* process (e.g., `go`) to decide the action for the *target* thread (e.g., in `compile`). If the caller didn't have a handler for that signal but the target did, the kernel might incorrectly trigger the default fatal action (termination). Additionally, fatal-by-default signals were triggering an immediate `exit_group` even if they were blocked in the target's `signal_mask`.

### **Fix**

1.  Updated `sys_tkill` to correctly identify the target process via `find_pid_by_thread(tid)` and use that process's `signal_actions` and `signal_mask`.
2.  Ensured that fatal-by-default signals (like `SIGPIPE`) only trigger `exit_group` if they are **not** blocked in the target's mask. If blocked, they are pended for later delivery.

---

## 30. `rt_sigtimedwait` (137) unimplemented — Go signal forwarding hang

**Status:** Fixed (2026-03-19) in `src/syscall/signal.rs`
**Component:** `sys_rt_sigtimedwait`

### Symptom

Go binaries using signal-heavy synchronization (or musl-based binaries) would return `ENOSYS` or hang during signal-wait loops.

### **Fix**

Implemented `sys_rt_sigtimedwait`. It checks for pending signals and blocks the thread with a timeout if none are available. It correctly populates `siginfo_t` if requested.

---

## 31. Signal handlers not shared across threads — `CLONE_SIGHAND` violation

**Status:** Fixed (2026-03-19) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `SharedSignalTable`, `Process`

### Symptom

If one thread set a signal handler via `sigaction`, other threads in the same process would not see it, continuing to use the default disposition. This is non-compliant with Linux/POSIX thread semantics.

### **Fix**

Refactored `Process.signal_actions` into an `Arc<SharedSignalTable>`. `clone_thread` now performs an `Arc::clone`, ensuring all threads in a group share exactly one signal table protected by a `Spinlock`.

---

## 32. `SA_RESTART` ignored — spurious `EINTR` in binaries

**Status:** Fixed (2026-03-19) in `src/exceptions.rs`
**Component:** `try_deliver_signal`

### Symptom

Syscalls like `read` or `nanosleep` would return `EINTR` even when the signal handler was registered with `SA_RESTART`. This caused unnecessary retry loops or failures in binaries that expect the kernel to handle the restart.

### **Fix**

Implemented automatic restart logic. If `SA_RESTART` is set for a signal delivered during a syscall, the kernel now decrements the saved `ELR` by 4 bytes, causing the processor to re-execute the `SVC` instruction upon returning from the signal handler.

---

## 34. `rt_sigtimedwait` (137) unimplemented — Go signal forwarding hang

**Status:** Fixed (2026-03-19) in `src/syscall/signal.rs`
**Component:** `sys_rt_sigtimedwait`

### Symptom

Go binaries using signal-heavy synchronization (or musl-based binaries) would return `ENOSYS` or hang during signal-wait loops.

### **Fix**

Implemented `sys_rt_sigtimedwait`. It checks for pending signals and blocks the thread with a timeout if none are available. It correctly populates `siginfo_t` if requested.

---

## 35. Signal handlers not shared across threads — `CLONE_SIGHAND` violation

**Status:** Fixed (2026-03-19) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `SharedSignalTable`, `Process`

### Symptom

If one thread set a signal handler via `sigaction`, other threads in the same process would not see it, continuing to use the default disposition. This is non-compliant with Linux/POSIX thread semantics.

### **Fix**

Refactored `Process.signal_actions` into an `Arc<SharedSignalTable>`. `clone_thread` now performs an `Arc::clone`, ensuring all threads in a group share exactly one signal table protected by a `Spinlock`.

---

## 36. `SA_RESTART` ignored — spurious `EINTR` in binaries

**Status:** Fixed (2026-03-19) in `src/exceptions.rs`
**Component:** `try_deliver_signal`

### Symptom

Syscalls like `read` or `nanosleep` would return `EINTR` even when the signal handler was registered with `SA_RESTART`. This caused unnecessary retry loops or failures in binaries that expect the kernel to handle the restart.

### **Fix**

Implemented automatic restart logic. If `SA_RESTART` is set for a signal delivered during a syscall, the kernel now decrements the saved `ELR_EL1` by 4 bytes, causing the processor to re-execute the `SVC` instruction upon returning from the signal handler.

---

## 37. Per-process current-syscall visibility — poor debugging

**Status:** Fixed (2026-03-19) in `src/syscall/mod.rs` and `src/shell/commands/builtin.rs`
**Component:** `Process`, `ps`

### Symptom

`ps` would show `SYSCALL=-` for all processes, making it difficult to debug stuck threads.

### **Fix**

1.  Added `current_syscall` field to `Process`.
2.  Updated `handle_syscall` to set `current_syscall` at entry and reset it on exit.
3.  Updated `ps` to display the active syscall (marked with `*`) for threads currently in the kernel.

---

## 38. Known remaining gaps (not yet fixed)

| Syscall / feature | Notes |
|---|---|
| `epoll` + goroutine scheduler | Go's netpoller uses `epoll_pwait`; this is implemented and capped at 10 ms polling interval |
| Per-process current-syscall visibility | `ps` now shows `SYSCALL=*NR` for active syscalls; background threads in long-running syscalls are now visible |
| CLONE_VM sharing | VFORK+CLONE_VM creates a full copy of the address space instead of sharing page tables; this makes fork slow for large heap processes. True CLONE_VM would eliminate the copy |
| `compile -V=full` stdout pipe | Fixed in §19 (refcount bugs) and §21 (`pipe_write` EPIPE). Both patches are required for reliable `go build` |

## 39. Per-thread sigaltstack (FIXED 2026-03-20)

**Symptom:** `go build` crashed with `futexwakeup addr=0xc4047158 returned -22` followed by `SIGSEGV PC=0x20000000`. The garbage PC (0x20000000 is not a valid Go text address) indicated the `rt_sigreturn` was restoring from a corrupted signal frame.

**Root cause:** `sigaltstack_sp/size/flags` were stored in the `Process` struct, keyed by PID. All `CLONE_VM` threads in a Go process share the same `Process` struct (via the address-space owner PID). Each Go M (OS thread) calls `sigaltstack` to configure its own gsignal stack, but all writes collided in the same `proc.sigaltstack_sp`. Signal delivery on thread A used thread B's (overwritten) sigaltstack address, placing the signal frame in the wrong memory → frame corruption → garbage PC on `rt_sigreturn` → Go's `futexwakeup` received -22 (EINVAL) from the garbage address.

****Fix**:** Added per-kernel-thread-slot arrays `THREAD_SIGALTSTACK_{SP,SIZE,FLAGS}` in `crates/akuma-exec/src/threading/mod.rs`. `sys_sigaltstack` now reads/writes `threading::get/set_sigaltstack(current_thread_id())`. `try_deliver_signal` in `src/exceptions.rs` reads from the same per-thread array. Each thread slot is reset to SS_DISABLE when the slot is freed.

## 40. pend_signal_for_thread did not wake sleeping thread (FIXED 2026-03-20)

**Symptom:** SIGURG pended on a Go M that was blocked in `FUTEX_WAIT` was silently lost. The thread only woke on timeout or an explicit `FUTEX_WAKE`, so the Go preemption mechanism didn't work reliably.

**Root cause:** `pend_signal_for_thread` stored the signal number in `PENDING_SIGNAL[tid]` but never called `get_waker_for_thread(tid).wake()`, so a thread parked in `schedule_blocking` inside `sys_futex` would not be rescheduled.

****Fix**:** Added `get_waker_for_thread(tid).wake()` call at the end of `pend_signal_for_thread`.

## 41. FUTEX_WAIT returned 0 not EINTR when woken by signal (FIXED 2026-03-20)

**Symptom:** When a signal woke a thread from `FUTEX_WAIT`, the syscall returned 0 (success) instead of -EINTR. Go ignores the futex return value in its `futexsleep` wrapper so this wasn't directly crashing, but it violates the Linux spec.

****Fix**:** After `schedule_blocking` returns in the `FUTEX_WAIT` path, `peek_pending_signal(tid)` is checked; if non-zero the syscall returns EINTR before checking the timeout.

## 42. FUTEX_WAIT_BITSET + CLOCK_REALTIME absolute timeout mishandled (FIXED 2026-03-20)

**Symptom:** `FUTEX_WAIT_BITSET` with `FUTEX_CLOCK_REALTIME` passed an absolute wall-clock `timespec`. The kernel was treating it as a relative timeout and adding `uptime_us()`, causing threads to sleep far into the future (wall-clock seconds are much larger than uptime microseconds on a freshly-booted VM).

****Fix**:** When `(op & FUTEX_CLOCK_REALTIME) != 0` and `cmd == FUTEX_WAIT_BITSET`, the `timeout_us` value is used directly as the deadline (treated as uptime microseconds — imprecise but prevents multi-hour sleeps). Also added: `FUTEX_WAIT_BITSET` with `val3==0` now returns EINVAL per spec.

## 43. uc_stack in signal frame still used process-level sigaltstack (FIXED 2026-03-20)

**Symptom:** After fix §39 (per-thread sigaltstack), `futexwakeup addr=0xc4047158 returned -22` and `SIGSEGV PC=0x20000000` persisted. New diagnostic log showed `[futex] EINVAL: uaddr=0x1 op=129` — Go calling `FUTEX_WAKE` with a clearly corrupted address (0x1). Registers at crash time contained ASCII bytes of the string `"futexwakeup addr="`, proving Go's goroutine stack data had been overwritten.

**Root cause:** `try_deliver_signal` was correctly placing the signal frame on the per-thread sigaltstack (using `get_sigaltstack(thread_slot)`) for the stack selection and re-entrancy check, but the `uc_stack` field written into the `ucontext_t` inside the frame still read from `proc.sigaltstack_sp` / `proc.sigaltstack_size` — which are always 0 for CLONE_VM threads (they only update the per-thread arrays). Go's runtime reads `uc_stack.ss_sp` and `uc_stack.ss_flags` from the signal frame to determine whether the signal was delivered on gsignal. With `uc_stack` showing zeros (SS_ONSTACK not set), Go concluded the signal landed on the goroutine stack and adjusted its internal stack-tracking state accordingly, triggering corruption of Go's goroutine and M state.

****Fix**:** Changed the `uc_stack` population in `try_deliver_signal` (`src/exceptions.rs`) to use `alt_sp` / `alt_size` (already computed from `get_sigaltstack(thread_slot)`) rather than `proc.sigaltstack_sp` / `proc.sigaltstack_size`. The `on_altstack` predicate now checks `alt_sp != 0` instead of `proc.sigaltstack_sp != 0`.

**New tests:** `test_sigaltstack_syscall_roundtrip`, `test_rt_sigreturn_restores_registers`, `test_uc_stack_uses_per_thread_sigaltstack`, and `test_futex_wait_eintr_signal_preserved` were added to `src/sync_tests.rs` to exercise these paths in isolation.

## 44. u32 truncation underflow in `test_futex_wait_eintr_signal_preserved` (FIXED 2026-03-20)

**Symptom:** The kernel test `test_futex_wait_eintr_signal_preserved` panicked with `unexpected ret 0xfffc (65532)` immediately after being added.

**Root cause:** The test packed the futex return value and the pending signal number into a single `AtomicU32` using `(ret as u32) << 16 | sig`. `EINTR = -4 = 0xFFFF_FFFF_FFFF_FFFC` as a `u64`; casting to `u32` gives `0xFFFF_FFFC`. Shifting left by 16 in a 32-bit type wraps: `0xFFFF_FFFC << 16 = 0xFFFC_0000`. Reading back `>> 16` then yielded `0xFFFC` (65532) instead of the expected `-4`.

****Fix**:** Replaced the single packed `AtomicU32` with a dedicated `AtomicU64` for the return value (`EINTR_RET`) and a separate `AtomicU32` for the signal (`EINTR_SIG`), each with independent sentinel values. The main thread now waits on the `EINTR_SIG` sentinel and reads `EINTR_RET` as a full `u64`, casting to `i64` for the sign check.

## 45. SIGURG delivered before Go M calls sigaltstack — goroutine stack corruption (FIXED 2026-03-20)

**Symptom:** `go build` crashed with `futexwakeup addr=0xc4047158 returned -22` and `SIGSEGV PC=0x20000000` even after all previous fixes were applied. Kernel logs showed `[futex] EINVAL: uaddr=0x1 op=129` immediately after a SIGURG delivery — a goroutine-local futex address variable had been overwritten with a byte from the signal frame.

**Root cause:** Go Ms register their SIGURG (preemption) handler with `SA_ONSTACK|SA_SIGINFO`, which tells the kernel to deliver the signal on the gsignal alternate stack. However, `sigaltstack` is called during `mstart` *after* the OS thread is created. If the Go scheduler sends SIGURG to a newly created M before it has executed `sigaltstack`, `alt_sp == 0` for that kernel thread slot.

When `alt_sp == 0` and `SA_ONSTACK` is set, the previous code fell through to the `else` branch and placed the signal frame at the goroutine's current SP. The signal frame (`rt_sigframe`, 1112 bytes) was written over live goroutine stack variables. In particular, Go's `asyncPreempt` handler calls `asyncPreempt2` (a regular Go function), which may grow the stack further downward — clobbering any goroutine variables that happened to lie just below the current SP. One of these overwritten values was a futex address, which became `0x1` (a byte from the signal frame zero-fill), producing the EINVAL.

The `PC=0x20000000` SIGSEGV that followed was a secondary crash: another goroutine had its saved `pc` field (in `uc_mcontext` at `ucontext+432`) clobbered similarly, causing `rt_sigreturn` to restore a garbage PC.

****Fix** (`src/exceptions.rs` `try_deliver_signal`):** Added an early-return guard: if `(action.flags & SA_ONSTACK) != 0` and `alt_sp == 0`, the signal is re-pended via `pend_signal_for_thread(thread_slot, signal)` and `try_deliver_signal` returns `false`. The signal will be retried at the next syscall boundary. By that point `mstart` will have called `sigaltstack`, so `alt_sp` will be non-zero and delivery will succeed on the correct gsignal stack.

Diagnostic logging was also added at signal delivery time (printing `slot`, `alt_sp`, `alt_size`, `elr_el1`, `new_sp`) and in `do_rt_sigreturn` (printing the restored `sp`, `pc`, `pstate`) to make future regressions easier to diagnose.

## 46. Pending signal not delivered after `rt_sigreturn` — second SIGURG corrupts futex x0 (FIXED 2026-03-20)

**Symptom:** After all previous fixes, `go build` still intermittently produced:

```
[futex] EINVAL: uaddr=0x1 op=129 (null or unaligned)
futexwakeup addr=0xc4047158 returned -22
SIGSEGV fault=0x1006
```

`uaddr=0x1` is the `mutex_locked` sentinel (1 = locked), not a real address. The `SIGSEGV` at `0x1006` is Go's deliberate crash (`throw`) triggered by an unexpected futex failure.

**Root cause:** Linux delivers pending signals on **every** return to user mode, including after `rt_sigreturn`. Akuma only checked for pending signals at the end of the normal `handle_syscall` path. `rt_sigreturn` (NR 139) returned *early* — before that check — so any signal that arrived while the previous signal handler was executing was silently skipped until the next syscall.

The exact crash sequence:

1. `futexwakeup(addr=0xc4047158)` → `sys_futex(WAKE)` returns `1` (woke one waiter). The kernel sets `frame.x0 = 1`.
2. A pending SIGURG is found at syscall-return time. The kernel saves `frame.x0 = 1` in `mcontext.regs[0]` of the signal frame and redirects ELR to the SIGURG handler.
3. Go's `doSigPreempt` calls `pushCall`, which:
   - decrements `mcontext.sp` by 8 (pushes the original LR onto the goroutine stack),
   - sets `mcontext.regs[30]` to `pc_after_svc` (so `asyncPreempt`'s `RET` lands after the SVC),
   - sets `mcontext.pc` to `asyncPreempt`.
4. `rt_sigreturn` SVC: `do_rt_sigreturn` restores the modified frame. `frame.x0` is restored to `1` (the saved futex return value).
5. **Before this fix:** `return saved_x0` exited immediately — the pending-signal check was never reached. A second SIGURG that arrived during step 3 was left in the queue.
6. `asyncPreempt` runs with the goroutine stack shifted by -8 (from `pushCall`). The second SIGURG is deferred to the *next* syscall.
7. At the next syscall, the goroutine's stack is in the shifted state. A `FUTEX_WAKE` call is made with `x0` still holding the shifted/stale value `1` instead of the correct address. The kernel rejects it with EINVAL.

****Fix** (`src/exceptions.rs`):** Immediately after `do_rt_sigreturn` succeeds, the kernel now runs the same pending-signal check that exists at the end of the normal syscall return path:

```rust
if let Some(sig) = akuma_exec::threading::take_pending_signal(sig_mask) {
    unsafe { (*frame).x0 = saved_x0; }
    if try_deliver_signal(frame, sig, 0) {
        return sig as u64;
    }
}
return saved_x0;
```

`do_rt_sigreturn` has already restored the full register set in `*frame` (correct SP/PC), so `try_deliver_signal` sees the right context. `frame.x0` is set to `saved_x0` before delivery so that the nested signal frame correctly saves the original syscall return value, and `rt_sigreturn` from the nested handler restores it.

****Fix** (`src/syscall/sync.rs`):** The `[futex] EINVAL` log now reads `elr_el1` and prints it alongside `uaddr` and `op`, making it straightforward to identify the faulting SVC site in future crashes:

```
[futex] EINVAL: uaddr=0x1 op=129 elr=0x... (null or unaligned)
```

**New tests (`src/sync_tests.rs`):**
- `test_futex_einval_uaddr_one` — `FUTEX_WAKE` with `uaddr=1` (the exact Go `mutex_locked` value) returns EINVAL.
- `test_futex_wake_valid_addr_no_waiters` — `FUTEX_WAKE` with a valid aligned address and no waiters returns 0, not EINVAL (regression guard).
- `test_pending_signal_drained_by_take` — `take_pending_signal` consumes a pended SIGURG exactly once; a second call returns `None`. This is the critical invariant the `rt_sigreturn` delivery fix relies on.

**See also:** `docs/SIGNAL_DELIVERY.md` for a detailed explanation of the Go preemption / `rt_sigreturn` interaction.

## 47. Additional signal-pending test coverage for §46 crash paths (2026-03-20)

**Status:** Tests only — no kernel code change.

**Context:** The §46 fix handles the case where a SIGURG is pending when
`rt_sigreturn` is called. Several related invariants were untested, making it
hard to confirm whether the fix is complete or a related code path is still
wrong. Six new tests were added to `src/sync_tests.rs`.

**Tests added:**

- `test_take_pending_signal_sigurg_masked` — Verifies that when SIGURG (23,
  bit 22) is pending but the mask has bit 22 set, `take_pending_signal` returns
  `None` and the signal stays in `PENDING_SIGNAL`. With mask=0 the same call
  returns `Some(23)`. This is the mask state while `asyncPreempt` runs (SIGURG
  blocked by `proc.signal_mask` after first delivery, then unblocked by the
  `uc_sigmask` restore in `rt_sigreturn`).

- `test_take_pending_signal_sigkill_ignores_mask` — Verifies SIGKILL (9) and
  SIGSTOP (19) are returned by `take_pending_signal` regardless of mask,
  including `u64::MAX`. Guards against the unmaskable-signal logic being
  accidentally removed.

- `test_pending_signal_overwrite` — Pends SIGUSR1 (10) then immediately pends
  SIGURG (23). The second pend overwrites the first (single-slot limitation).
  `take_pending_signal(0)` must return `Some(23)`, not `Some(10)`. Documents
  the known limitation: if two signals arrive rapidly, only the last survives.

- `test_signal_mask_bit_numbering` — Asserts the exact bit position of
  SIGHUP/SIGKILL/SIGSTOP/SIGURG (signals 1/9/19/23 → bits 0/8/18/22).
  Prevents off-by-one errors in mask logic from going unnoticed.

- `test_futex_wake_sigurg_pending_x0_not_reused` — Regression test for the
  exact crash sequence: spawns one waiter, calls `FUTEX_WAKE(1)`, records the
  return value (0 or 1), pends SIGURG on the waker thread, verifies
  `peek_pending_signal` returns 23, and verifies `take_pending_signal(0)`
  returns `Some(23)` and drains the queue. Confirms the pending-signal
  machinery is consistent with the state just before the §46 crash.

- `test_futex_wake_returns_exact_count_three_waiters` — Spawns 3 waiters,
  calls `FUTEX_WAKE(max=1)`, and asserts the return value is ≤ 1. Documents
  that this return value equals Go's `mutex_locked` sentinel (1), which is
  why passing it directly as `uaddr` in a subsequent `FUTEX_WAKE` produces
  EINVAL.

**Signal mask bit-numbering (reference):**

| Signal | N | Bit | Mask value |
|--------|---|-----|------------|
| SIGHUP | 1 | 0 | `0x0000_0001` |
| SIGKILL | 9 | 8 | `0x0000_0100` |
| SIGSTOP | 19 | 18 | `0x0004_0000` |
| SIGURG | 23 | 22 | `0x0040_0000` |

---

## 49. Broken Pipe / Premature Pipe Destruction (2026-03-20)

**Status:** Fixed (2026-03-20)

### **Fix**
- **Error Codes**: `sys_read`/`sys_write` return `EBADF` for invalid FDs.
- **Cleanup**: `SharedFdTable` implements `Drop` to ensure resources (pipes, etc.) are closed only when the last reference to the FD table is gone.

## 50. Pipe Read Performance (Quadratic Slowdown) (2026-03-20)

**Status:** Fixed (2026-03-20)

### **Fix**
- Replaced `Vec<u8>` with `VecDeque<u8>` in `KernelPipe` for efficient $O(1)$ reads.

## 51. `ChildStdout` streaming hangs — parent busy-looping on non-blocking read (2026-03-20)

**Status:** Fixed (2026-03-20)

### **Fix**
- **Blocking Reads**: Added `reader_thread` to `ProcessChannel`, allowing reads to block until data is written.
- **`epoll` Readiness**: Updated `epoll_check_fd_readiness` to correctly check for `ChildStdout` data availability.

---

## 52. `find /proc` errors on dead-process `fd` directory (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/vfs/proc.rs`

### Symptom

```
/usr/bin/find: failed to opendir /proc/49/fd: No such file or directory
```

A dead process (PID retained in the syscall log) appeared in `ls /proc` but its `fd` subdirectory returned `ENOENT` when `find` tried to open it — an inconsistent directory listing that caused `find` to exit with code 1 and confused Go's build tooling.

### Root cause

`read_dir` for `<pid>/` always added a `"fd"` `DirEntry`, even when the process only existed via its retained syscall log (i.e. `process_exists(pid)` was false). Opening that directory then returned `NotFound` because the `<pid>/fd` handler also gated on `process_exists`.

### **Fix**

Gate the `"fd"` entry on `Self::process_exists(pid)`. Dead processes show only `"syscalls"` in their directory listing — consistent with what can actually be opened.

---

## 53. epoll EINTR + dup3 invariant — kernel regression tests (2026-03-21)

**Status:** Tests added (2026-03-21) in `src/process_tests.rs`, `src/sync_tests.rs`

### Background

Go crashed with `FAR=0xffffffffffffffea` (-22 = EINVAL used as a pointer) after SIGURG delivery. The crash indicated that either `sys_dup3` was returning EINVAL for a valid call (wrongly treating a non-matching fd pair as same-fd), or that `epoll_pwait` was not returning EINTR when a signal was pending — leaving Go's signal handler unsatisfied.

### Tests added

- **`test_dup3_no_einval_for_valid_args`**: Verifies the three `sys_dup3` invariants — `oldfd==newfd` → EINVAL, valid pair → `newfd`, bad `oldfd` → EBADF. Catches any regression where EINVAL leaks into valid dup paths.
- **`test_pipe_close_write_wakes_epoll_poller`**: Verifies `pipe_close_write` both drains pollers and sets `pipe_can_read` (EOF) simultaneously — the core of Go's parent-waits-for-compile-stdout workflow.
- **`test_epoll_eintr_when_signal_pending`**: Verifies `sys_epoll_pwait` returns `-EINTR` immediately when `is_current_interrupted()` is true, without blocking. Essential for Go's goroutine preemption via SIGURG.

---

## 54. si_code wrong for NULL dereferences — SIGSEGV treated as software signal (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/exceptions.rs`

### Symptom

Go crashed with `PC=0x20000000, sigcode=-6, addr=0x0`. Go's SIGSEGV handler checks `si_code` to distinguish memory faults (`SEGV_MAPERR=1`) from software-sent signals (`SI_TKILL=-6`). With `si_code=-6` on a NULL deref, Go treated it as a goroutine preemption signal and tried to preempt a goroutine at the bogus fault PC.

### Root Cause

`try_deliver_signal` used `fault_addr == 0` as a proxy for "software signal" to set `si_code`:
```rust
let si_code: i32 = if fault_addr == 0 { -6i32 } else { 1i32 };
```
NULL dereferences have `FAR=0` but are hardware faults (`is_fault=true`), so they got `si_code=-6` incorrectly.

### **Fix**

Added `is_fault: bool` parameter to `try_deliver_signal`. Hardware fault call sites pass `true`; software signal call sites pass `false`. The `si_code` is now `if is_fault { 1 } else { -6 }`.

---

## 55. procfs non-stdio fd reads returning ENOENT (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/vfs/proc.rs`, `src/syscall/fs.rs`

### Symptom

`cat /proc/<pid>/fd/<n>` for fd > 1 returned ENOENT. fd 0 and 1 worked.

### Root Cause

`read_symlink` returned virtual paths like `"pipe:[5]"` for non-File fds. `sys_openat` called `resolve_symlinks` which chased this to `crate::fs::exists("pipe:[5]")` → false → ENOENT. fd 0/1 accidentally worked because `get_fd(0/1)` returned `None` (stdin/stdout aren't in the fd table for old processes), so `read_symlink` returned `Err` and `resolve_symlinks` left the path unchanged.

Additionally, `exists`, `metadata`, and `read_at` all short-circuited at `fd_num <= 1`.

### **Fix**

- `read_symlink` now only returns a resolvable path for `File` fds. Other fd types return `Err` so `resolve_symlinks` leaves the path unchanged.
- `readlinkat` falls back to `proc_fd_description()` (new pub fn) for non-File fds, which returns the virtual description string (`"pipe:[5]"`, `"socket:[n]"`, etc.).
- `exists` now checks `proc.get_fd(fd_num).is_some()` for fd > 1.
- `metadata` returns metadata with size=0 for any valid fd > 1.
- `read_at` returns the fd description string for fd > 1.

---

## 56. `CGO_ENABLED=0 go build` crashes during compilation (2026-03-21)

**Status:** In progress

### Background

`CGO_ENABLED=0 go build -n` (dry run — resolves dependencies and prints commands without executing them) now works. `CGO_ENABLED=0 go build` (actual compilation — invokes the Go compiler and assembler) still crashes.

The `-n` path exercises: process spawning, pipes, epoll, signal delivery, `/proc` reads, `waitpid`. These are all fixed. The actual build path additionally runs the `compile` and `asm` toolchain binaries inside Go's build graph, which stress different kernel paths.

### Known current failure point

To be determined — need a kernel crash log from a `go build` run to identify the next failing syscall or kernel bug.

### Likely candidates

- **`clone3` / `clone` with new flags**: The Go toolchain spawns many compiler workers; any unhandled clone flag causes EINVAL.
- **`prlimit64` / `getrlimit`**: Compiler may query resource limits.
- **`fcntl(F_DUPFD_CLOEXEC)`**: Used during pipe setup for compiler subprocesses.
- **`/proc/self/fd` enumeration**: Compiler may walk its own fd table to close inherited fds.
- **`mmap` anonymous with `MAP_FIXED`**: Go's compiler allocates large arenas; partial unmaps or MAP_FIXED collisions may fault.
- **`sched_getaffinity`**: Some Go versions call this to determine GOMAXPROCS.
- **Signal mask inheritance across `clone`**: Child processes need the correct signal mask from the parent.

---

## 57. FUTEX_PRIVATE_FLAG stripping — cross-process wake theft (Fixed 2026-03-21)

**Status:** Fixed (2026-03-21) in `src/syscall/sync.rs`

### Symptom

`CGO_ENABLED=0 go build` hangs; the `compile` subprocess's goroutines never run. M-threads are stuck in `futex_wait` indefinitely.

### Root Cause

`FUTEX_PRIVATE_FLAG` was stripped and discarded in `sys_futex`. All processes shared a single global `BTreeMap<usize, Vec<usize>>` keyed by VA only. Without ASLR, `go build` and the `compile` subprocess both load the Go runtime at the same base address — their M-thread park futex VA is identical. A `FUTEX_WAKE_PRIVATE` from `go build` accidentally dequeued a `compile` thread (same VA key, different physical page), leaving `compile`'s own M-threads parked forever.

### Fix

Changed the futex waiter key from `usize` (VA only) to `(u32, usize)` — `(tgid, uaddr)`.

- Private ops (`FUTEX_PRIVATE_FLAG` set): `tgid = read_current_pid()`, scoping the futex to the process.
- Shared/non-private ops and kernel-internal wakes (`clear_child_tid`, robust futex): `tgid = 0`.

This prevents cross-process VA collisions when different processes share the same virtual address due to no ASLR.

### Tests Added

Three kernel-level tests in `src/sync_tests.rs`:
- `test_futex_private_flag_basic_wake` — FUTEX_WAIT_PRIVATE / FUTEX_WAKE_PRIVATE end-to-end.
- `test_futex_private_flag_wake_one_of_two` — FUTEX_WAKE_PRIVATE(1) with 2 waiters wakes exactly 1.
- `test_futex_private_tgid_isolation` — waking with wrong tgid (99) finds no waiters; correct tgid (0) wakes the thread.
