# Go Runtime — Missing / Incomplete Syscall Support

Tracked gaps and fixes required to run Go binaries on Akuma.

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

### Fix

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

### Fix

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

### Fix

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

### Fix

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

UDP is excluded because UDP sockets do not have a byte-stream; each `read()`
returns one complete datagram and draining semantics differ.

---

## 5. `restart_syscall` (nr=128) returns ENOSYS — Go runtime crash after signal

**Status:** Fixed (2026-03-15) in `src/syscall/mod.rs`
**Component:** `src/syscall/mod.rs` — syscall dispatch

### Symptom

After EPOLLET was fixed (section 4), Go programs that reach a code path where
signal delivery races with a blocking syscall crash with a data abort at a
near-zero address (e.g. `FAR=0x59`):

```
[ENOSYS] nr=128 pid=52 args=[0xa45fbf80, 0x27, 0x0]
[WILD-DA] pid=52 FAR=0x59 ELR=0x432e60 last_sc=128
  x0=0xffffffffffffffda ...
```

`x0=0xffffffffffffffda` = -38 = ENOSYS. The Go runtime at `ELR=0x432e60` does
not check for ENOSYS from `restart_syscall` and dereferences the return value
(or a struct reached via it) as a pointer.

### Root cause

On ARM64, syscall number 128 is `restart_syscall` — a kernel-internal mechanism
for restarting syscalls that were interrupted by a signal when the action has
`SA_RESTART` set. When the kernel delivers a signal mid-syscall and the
sigaction has `SA_RESTART`, it rewrites the interrupted context so that after
`rt_sigreturn` the process re-executes the original syscall via `restart_syscall`
(x8=128).

Akuma did not have SA_RESTART syscall-restart semantics, so `restart_syscall`
fell through to the default `ENOSYS` handler. Go's runtime does not check for
`ENOSYS` in this path and crashes.

### Fix

Add an explicit case for 128 (`restart_syscall`) that returns `EINTR` instead
of `ENOSYS`. Since Akuma does not track per-process restartable syscall state,
`EINTR` is the correct fallback — the caller retries the original operation:

```rust
// restart_syscall = 128 on ARM64
128 => EINTR,
```

---

## 6. `waitid` (syscall 95) — `go build` crashes waiting for child processes

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs` + `src/syscall/mod.rs`
**Component:** `src/syscall/proc.rs` — `sys_waitid`

### Symptom

`go build` crashes with a nil dereference shortly after spawning subprocesses
(`compile`, `link`, etc.):

```
[ENOSYS] nr=95 pid=92 args=[0x1, 0x60, 0xc431cbb8]  ← waitid(P_PID, 96, siginfo_ptr)
[WILD-DA] pid=96 FAR=0x90 ELR=0x100a65b8             ← nil deref in Go after ENOSYS
[signal] sig 11 re-entrant fault at 0x48             ← kills process
```

### Root cause

Syscall 95 (`waitid`) fell through to the default `ENOSYS` handler. Go's
`os/exec` calls `waitid(P_PID, child_pid, &siginfo, WEXITED)` to reap
spawned subprocesses. When `ENOSYS` is returned, Go reads uninitialized data
from the `siginfo_t` buffer and dereferences a null pointer.

### Fix

Added `sys_waitid` in `src/syscall/proc.rs` as a thin wrapper over the
existing child-channel infrastructure already used by `sys_wait4`. Instead of
writing a 4-byte encoded wait status, it fills a 128-byte `siginfo_t` struct
with `SIGCHLD` fields (`si_signo`, `si_code`, `si_pid`, `si_status`).

Added `nr::WAITID = 95` constant and dispatcher entry in
`src/syscall/mod.rs`.

---

## 7. `pidfd_open` (syscall 434) + `waitid(P_PIDFD)` — Go busy-polls with nanosleep, child crash

**Status:** Fixed (2026-03-15) in `src/syscall/pidfd.rs` + `src/syscall/proc.rs` + `src/syscall/poll.rs`
**Component:** new `src/syscall/pidfd.rs`; epoll readiness in `poll.rs`; `sys_waitid` in `proc.rs`

### Symptom

After the `waitid` fix (#6), `go build` still spins: PSTATS shows `nr101=287(6422ms)` —
287 calls to `nanosleep` consuming ~6.4 s of a 12.8 s run. A child process (the compiled
binary) crashes with `FAR=0x90` then `FAR=0x48` (nil deref pattern identical to #6).

```
[ENOSYS] nr=434 (pidfd_open) pid=46
...
[WILD-DA] pid=50 FAR=0x90 ELR=0x100a65b8 last_sc=101   ← nanosleep, not waitid
```

### Root cause

Go 1.22+ uses `pidfd_open` to obtain a file descriptor for each child process. The fd is
added to Go's netpoller epoll so that child exit triggers an epoll event (EPOLLIN) rather
than requiring a polling loop with `nanosleep`. When `pidfd_open` returns `ENOSYS`, Go
falls back to a polling loop: sleep 1 ms, check child status, repeat. This produces the
`nr101` tsunami and makes the overall build much slower. The child crash at `last_sc=101`
is the child's own nanosleep returning after the parent has given up and exited.

### Fix

**New module `src/syscall/pidfd.rs`:**

A global `PIDFD_TABLE` maps pidfd IDs to their target PID. Readiness is determined by
checking the existing `ProcessChannel` for the target PID (same infrastructure as
`sys_wait4` / `sys_waitid`).

```rust
pub(super) fn pidfd_can_read(id: u32) -> bool {
    let pid = match pidfd_get_pid(id) { Some(p) => p, None => return true };
    akuma_exec::process::get_child_channel(pid)
        .map_or(true, |ch| ch.has_exited())
}
```

**Epoll integration (`src/syscall/poll.rs`):**

```rust
FileDescriptor::PidFd(pidfd_id) => {
    if requested & EPOLLIN != 0 && super::pidfd::pidfd_can_read(pidfd_id) {
        ready |= EPOLLIN;
    }
}
```

**`waitid(P_PIDFD=3, fd, ...)` (`src/syscall/proc.rs`):**

When `idtype == P_PIDFD`, the `id` argument is a fd number. The fd is resolved to a
`PidFd(pidfd_id)`, then to the underlying PID via `pidfd_get_pid`, and finally waited
on with the same child-channel loop used by `P_PID`.

**Supporting changes:**
- `FileDescriptor::PidFd(u32)` added to the enum in `crates/akuma-exec/src/process/types.rs`
- `sys_close` / `sys_close_range` call `pidfd_close` to release table entries
- `src/vfs/proc.rs` /proc/pid/fd symlink shows `anon_inode:[pidfd:N]`

---

## 8. `CLONE_PIDFD` not handled — Go netpoller uses garbage fd, crashes at FAR=0x90

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs`
**Component:** `src/syscall/proc.rs` — `sys_clone3` / `sys_clone_pidfd`

### Symptom

After implementing `pidfd_open` (fix #7), `go build` still crashes with
`last_sc=22` (EPOLL_PWAIT) and `FAR=0x90`. All x-registers are zero at
the fault site `ELR=0x100a65b8`. The nanosleep counter `nr101` remains
high, indicating Go is still spinning even after the pidfd implementation.

### Root cause

Go 1.22+ uses `clone3(CLONE_PIDFD|CLONE_VFORK|CLONE_VM|SIGCHLD)` to
obtain the child's pidfd **atomically** in the same syscall that creates
the child. The `clone_args.pidfd` field is a pointer to where the kernel
should write the pidfd fd number after fork.

Our `sys_clone3` forwarded all clone_args fields to `sys_clone` but
silently ignored `CLONE_PIDFD` — it never wrote the fd number to
`cl_args.pidfd`. Go read an uninitialised value (often 0) from that
pointer and used it as a pidfd fd number. fd 0 (stdin) was then added to
epoll with `data = ptr_to_pollDesc`. When stdin returned EPOLLIN, Go
called `netpollunblock(pollDesc_ptr)`. Since `pollDesc_ptr` was actually
the stdin fd's data value — which could be 0 or some other invalid
pointer — accessing `pollDesc->rg` at offset `0x90` faulted at FAR=0x90.

### Fix

`sys_clone3` now passes `cl_args.pidfd` (the pointer) to a new internal
function `sys_clone_pidfd`. After `fork_process` succeeds, if
`CLONE_PIDFD` is set and the pointer is non-zero, the function calls
`sys_pidfd_open` for the new child and writes the resulting fd number
(4 bytes, i32) back to the pointer:

```rust
const CLONE_PIDFD: u64 = 0x1000;

if flags & CLONE_PIDFD != 0 && pidfd_out_ptr != 0 {
    if validate_user_ptr(pidfd_out_ptr, 4) {
        let pidfd_fd = super::pidfd::sys_pidfd_open(new_pid, 0);
        if (pidfd_fd as i64) >= 0 {
            let fd_i32 = pidfd_fd as i32;
            let _ = unsafe { copy_to_user_safe(pidfd_out_ptr as *mut u8,
                                               &fd_i32 as *const i32 as *const u8, 4) };
        }
    }
}
```

`sys_clone` (the old two-argument clone syscall) still delegates to
`sys_clone_pidfd` with `pidfd_out_ptr = 0`, so it is unaffected.

---

## 9. `MOUNT_TABLE` spinlock held during disk I/O — intermittent hang in parallel tests

**Status:** Fixed (2026-03-15) in `src/vfs/mod.rs`, `crates/akuma-vfs/src/mount.rs`, `crates/akuma-isolation/src/mount.rs`
**Component:** `src/vfs/mod.rs` — `with_fs`

### Symptom

The kernel intermittently hangs after printing `[TEST] Parallel process
execution` / `Spawning process 1...` and never returns. The hang is
nondeterministic (some runs pass, others hang forever).

### Root cause

`with_fs` (the VFS dispatch function) acquired `MOUNT_TABLE` (a plain
spinlock) and held it for the entire duration of the filesystem callback —
including the ext2 disk read inside `read_file`. On a single-core QEMU:

1. Thread A holds `MOUNT_TABLE` while reading a large ELF binary from
   ext2 (e.g. `/bin/hello`).
2. The 10 ms timer IRQ fires and the scheduler switches to thread B.
3. Thread B calls any VFS function (e.g. another `read_file`) and spins
   on `MOUNT_TABLE`.
4. Thread A never gets rescheduled while thread B is spinning → kernel
   deadlocks.

### Fix

Add `resolve_arc` to `MountTable` (akuma-vfs) and `MountNamespace`
(akuma-isolation) that returns `(Arc<dyn Filesystem>, String)` — owned
types — so the lock can be released before calling the filesystem
callback.

`with_fs` now releases the `MOUNT_TABLE` / namespace mount lock
**before** calling the I/O closure:

```rust
let global_arc = {
    let table = MOUNT_TABLE.lock();           // brief lock
    let table = table.as_ref()?;
    table.resolve_arc(&absolute).ok_or(FsError::NotFound)?
};                                            // lock released here
f(global_arc.0.as_ref(), &global_arc.1)     // I/O without any lock
```

`rename` received the same treatment since it also held the lock across
two path resolutions and the rename I/O call.

---

## 10. Signal state not reset on `execve` — child crash at FAR=0x90 with stale sigaltstack

**Status:** Fixed (2026-03-15) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `replace_image` / `replace_image_from_path`

### Symptom

After the `CLONE_PIDFD` fix (#8), `go build` still crashes in the child
compiler subprocess (`pid=50`, a freshly exec'd `/usr/lib/go/bin/go`) with:

```
[WILD-DA] pid=50 FAR=0x90 ELR=0x100a65b8 last_sc=101
[signal] Delivering sig 11 to handler 0x10093180 sp=0xc400bc70 on sigaltstack [0xc4004000,0xc400c000)
[signal] sig 11 re-entrant fault at 0x48
```

`last_sc=101` is `nanosleep` — not a wait syscall. `ELR=0x100a65b8` is the
same Go netpoller function seen in earlier crashes. The signal is delivered on
the sigaltstack `[0xc4004000, 0xc400c000)` even though pid=50 has only just
started and should have a clean signal state.

### Root cause

`replace_image` and `replace_image_from_path` did not reset signal state after
replacing the process image. The new child process (`pid=50`) inherited:

- `signal_actions`: parent pid=46's goroutine signal handlers (e.g. SIGSEGV →
  `0x10093180`) pointing into the **parent's** binary. After exec, pid=50 maps
  its own binary at the same addresses, so the handler address coincidentally
  looks valid — but the handler runs with the parent's signal setup, not the
  child's freshly initialised one.
- `sigaltstack_sp = 0xc4004000`, `sigaltstack_size = 0x8000`: the parent's
  goroutine-local gsignal stack. This address may or may not exist in the new
  address space.

Per POSIX, on `execve`:
- Signals with a custom handler (`SA_SIGACTION` / function pointer) must be
  reset to `SIG_DFL`.
- `SIG_IGN` dispositions are preserved.
- The alternate signal stack is cleared (`SS_DISABLE`).

Because this was not done, the child's first fault delivered a signal using the
parent's stale sigaltstack, landing `sp` at `0xc400bc70` inside the inherited
`[0xc4004000, 0xc400c000)` window. The handler then faulted at `FAR=0x90`
(netpoller nil deref) and the re-entrant check killed the process with a
secondary fault at `FAR=0x48`.

### Fix

In both `replace_image` and `replace_image_from_path`, after `reset_io()`:

```rust
// POSIX: on exec, custom signal handlers are reset to SIG_DFL; SIG_IGN is preserved.
// Also disable the alternate signal stack — it pointed into the old address space.
for action in &mut self.signal_actions {
    if matches!(action.handler, SignalHandler::UserFn(_)) {
        *action = SignalAction::default();
    }
}
self.sigaltstack_sp = 0;
self.sigaltstack_size = 0;
self.sigaltstack_flags = 2; // SS_DISABLE
```

---

## 11. `tgkill` (syscall 131) returns ENOSYS — spurious ENOSYS log noise

**Status:** Fixed (2026-03-15) in `src/syscall/signal.rs` + `src/syscall/mod.rs`
**Component:** `src/syscall/signal.rs` — `sys_tgkill`

### Symptom

The kernel log contains entries such as:

```
[ENOSYS] nr=131 pid=46 args=[0x2e, 0x2e, 0x0]
```

Go uses `tgkill(tgid, tid, sig)` to send signals to specific threads within a
thread group. With `ENOSYS`, signal delivery to specific threads fails silently.

### Root cause

`tkill` (syscall 130) was implemented as `sys_tkill(tid, sig)` but `tgkill`
(syscall 131) had no constant or dispatch entry, so it fell through to `ENOSYS`.

### Fix

Added `sys_tgkill(_tgid, tid, sig)` in `src/syscall/signal.rs` that forwards
to `sys_tkill`. The `tgid` argument is accepted but not validated (Akuma does
not track thread groups separately from PIDs):

```rust
pub(super) fn sys_tgkill(_tgid: u32, tid: u32, sig: u32) -> u64 {
    sys_tkill(tid, sig)
}
```

Added `nr::TGKILL = 131` constant and dispatch entry:

```rust
nr::TGKILL => signal::sys_tgkill(args[0] as u32, args[1] as u32, args[2] as u32),
```

---

## 12. `msgctl` / `msgget` / `msgrcv` / `msgsnd` (syscalls 186-189) — `go build` crashes after epoll

**Status:** Fully implemented (2026-03-16) in `src/syscall/msgqueue.rs`
**Component:** new `src/syscall/msgqueue.rs`; dispatch in `src/syscall/mod.rs`

### Symptom

`go build` crashes with a wild data abort at `FAR=0xffffffffffffffda` (which is −38 =
`ENOSYS`), indicating a syscall error code was used as a memory address:

```
[ENOSYS] nr=187 pid=48 args=[0xc42cc000, 0xc42a8460, 0x50]
[WILD-DA] *** FAR=0xffffffffffffffda is -38 (ENOSYS) - syscall error used as pointer! ***
[WILD-DA] pid=48 FAR=0xffffffffffffffda ELR=0x1009296c last_sc=187
```

### Root cause

Syscalls 186–189 (SysV message queue operations: `msgget`, `msgctl`, `msgrcv`, `msgsnd`)
fell through to the default `ENOSYS` handler.  Go's runtime calls `msgctl` during its
build process and has a code path (same pattern as `restart_syscall` / syscall 128) where
an `ENOSYS` return value is used as a pointer without first checking errno, crashing at the
address of the error code.

### Fix (phase 1 — 2026-03-15)

Added `nr::MSGGET/MSGCTL/MSGRCV/MSGSND` constants (186–189) and stub dispatch arms
returning `EINVAL` as a safe stop-gap to prevent the crash.

### Fix (phase 2 — 2026-03-16)

Replaced the stubs with a full SysV message queue implementation in the new module
`src/syscall/msgqueue.rs`.

**Design:** SysV message queues are opened by integer key via `msgget`, not by file
descriptor inheritance, so a process in one container could reach another container's
queue by guessing the key. Queues are therefore scoped per box: the backing store is
`static MSGQUEUE_TABLE: Spinlock<BTreeMap<(u64, u32), MsgQueue>>` keyed by
`(box_id, msqid)`. msqids are still allocated from a global atomic (so they are unique
across all boxes), but all lookups in `msgctl`/`msgsnd`/`msgrcv` are keyed by the
caller's `box_id`, preventing cross-container access. `box_id` is read from the kernel
`Process` struct — it cannot be spoofed from userspace as it is never writable from EL0.

**`sys_msgget(key, flags)`** — creates or opens a queue.  `IPC_PRIVATE` always creates a
new private queue.  A named key searches the global table; `IPC_CREAT`/`IPC_EXCL` behave
per POSIX.

**`sys_msgctl(msqid, cmd, buf)`** — supports `IPC_RMID` (remove), `IPC_STAT` (read a
112-byte `msqid_ds` to userspace), and `IPC_SET` (update mode bits from userspace).

**`sys_msgsnd(msqid, msgp, msgsz, flags)`** — reads `mtype` (i64) + `mtext` from
userspace and enqueues a `KernelMsg`.  Blocks (yields) if the queue is full; returns
`EAGAIN` if `IPC_NOWAIT` is set.  Enforces `MSGMAX` (8192) per-message and `MSGMNB`
(16384) total-bytes-queued limits.

**`sys_msgrcv(msqid, msgp, msgsz, msgtyp, flags)`** — dequeues a matching message and
copies it to userspace.  Supports all three `msgtyp` modes: 0 = any, >0 = exact type
match, <0 = lowest type ≤ |msgtyp|.  Blocks (yields) if no match and `IPC_NOWAIT` is
not set; returns `ENOTMSG` otherwise.  `MSG_NOERROR` truncates oversized messages;
without it `E2BIG` is returned and the message stays in the queue.

---

## 13. `CLONE_VFORK` does not block parent — two goroutine runtimes corrupt shared address space

**Status:** Fixed (2026-03-16) in `src/syscall/proc.rs`
**Component:** `sys_clone_pidfd`, `do_execve`, `sys_exit`, `sys_exit_group`

### Symptom

After the `tgkill` fix (#11), `go build` still crashes. The log shows pid=52 (a child
spawned via `clone(CLONE_VFORK|CLONE_VM|SIGCHLD)`) dying with `FAR=0x90` and
`FAR=0x48` — the same Go netpoller nil-dereference pattern as issues #7/#8 — even
though `pidfd_open` and `CLONE_PIDFD` are already working. The epoll log shows pid=48
(the parent) continuing to run after the clone instead of blocking.

### Root cause

`sys_clone_pidfd` handled `CLONE_VFORK` by forking the child and immediately returning
the child PID to the parent — no blocking. This violates vfork semantics: the parent
and child both ran the Go goroutine scheduler concurrently in the same address space
(`CLONE_VM`), trampling each other's heap, stack, and runtime state. The crashes at
`FAR=0x90` / `FAR=0x48` are consequences of this concurrent corruption, not missing
syscalls.

### Fix

Added a `static VFORK_WAITERS: Spinlock<BTreeMap<u32, usize>>` (child PID → parent
thread ID) and a `vfork_complete(child_pid)` helper in `src/syscall/proc.rs`.

**Before `fork_process`** — insert into `VFORK_WAITERS` _before_ the child thread is
marked READY. Inserting after `fork_process` returns races: the child can exec and call
`vfork_complete` (finding nothing in the map) before the parent inserts its TID,
leaving the parent blocked forever.

```rust
// Insert BEFORE fork so the child always finds the entry when it calls vfork_complete.
if flags & CLONE_VFORK != 0 {
    VFORK_WAITERS.lock().insert(child_pid, parent_tid);
}
// Now fork — child may exec immediately on the next scheduler tick.
match fork_process(child_pid, stack) {
    Ok(new_pid) => {
        // ... CLONE_PIDFD handling ...
        if flags & CLONE_PIDFD != 0 {
            let pidfd_fd = super::pidfd::sys_pidfd_open(new_pid, 0);
            if (pidfd_fd as i64) >= 0 {
                let fd_i32 = pidfd_fd as i32;
                let _ = unsafe { copy_to_user_safe(args[2] as *mut u8, &fd_i32 as *const i32 as *const u8, 4) };
            }
        }
        if flags & CLONE_VFORK != 0 {
            schedule_blocking(u64::MAX);  // unblocked by vfork_complete
        }
    }
    Err(_) => {
        // Clean up the pre-inserted entry on failure.
        if flags & CLONE_VFORK != 0 { VFORK_WAITERS.lock().remove(&child_pid); }
    }
}
```

**`do_execve` (after successful `replace_image`)** — wake the parent before entering
the new user image (which never returns):

```rust
let pid = proc.pid;
vfork_complete(pid);   // unblock vfork parent
proc.address_space.activate();
unsafe { enter_user_mode(&proc.context); }
```

**`sys_exit` / `sys_exit_group`** — wake the parent if the child exits before exec
(e.g. exec fails):

```rust
vfork_complete(pid);
```

### Race condition in original fix (2026-03-16)

After the initial fix landed, `go build` ran without crashing but never completed
(~85 s of nanosleep polling, 6848 `epoll_pwait` calls returning 0 events).

**Root cause:** a race between `fork_process` marking the child READY and the parent
inserting into `VFORK_WAITERS`:

```
Parent                            Child thread (newly READY)
──────────────────────────────    ──────────────────────────────
fork_process() → child READY  →  [scheduler picks child]
                                  run fork-child code
                                  call execve
                                  do_execve → vfork_complete(child_pid)
                                    VFORK_WAITERS.remove(child_pid) → None
                                    (no wake sent to parent!)
                                  enter_user_mode(new image)
VFORK_WAITERS.insert(child_pid, parent_tid)  ← TOO LATE
schedule_blocking(u64::MAX)
  WOKEN_STATES[parent_tid] == false → block forever  ← BUG
```

**Fix:** insert into `VFORK_WAITERS` **before** `fork_process` (before the child
thread is created).  `vfork_complete` then always finds the parent TID.  On fork
failure the pre-inserted entry is removed in the error path.

### Kernel tests (in `src/process_tests.rs`)

| Test | What it checks |
|---|---|
| `test_vfork_dispatch` | `clone(CLONE_VFORK)` is dispatched (not ENOSYS) |
| `test_vfork_waiters_clean_at_boot` | `VFORK_WAITERS` is empty at boot (no leaked entries) |
| `test_vfork_complete_removes_entry` | Pre-inserting into `VFORK_WAITERS` then calling `vfork_complete` removes the entry — directly exercises the race-fix mechanism |

---

## 14. `go build -x -n .` deadlocks — goroutine scheduler never starts

**Status:** Fixed (2026-03-16) in `src/syscall/sync.rs`
**Component:** `src/syscall/sync.rs` — `sys_futex` FUTEX_WAIT path

### Symptom

`go build -x -n .` in `/playground` starts four OS threads (PIDs 48–51) and
then freezes indefinitely without producing any output or reading any source
files. The process can run for 30+ minutes without making progress:

```
PID  PPID  BOX  STATE      SYSCALL  CMDLINE
 48     0  0         running         -  /usr/lib/go/bin/go build -x -n .
 49    48  0         running         -  /usr/lib/go/bin/go build -x -n .
 50    48  0         running         -  /usr/lib/go/bin/go build -x -n .
 51    48  0         running         -  /usr/lib/go/bin/go build -x -n .
```

`/proc/sysvipc/msg` shows no active message queues (header only) — the message
queue implementation from fix #12 is not implicated.

### Evidence from `/proc/<pid>/syscalls`

PID 48's ring buffer (500 entries, ~2 s of history at the time of capture) contains
**only three syscall numbers**, repeating without interruption from the very first
logged entry through the most recent:

```
NR 113  clock_gettime        (1–9 µs)
NR  22  epoll_pwait          (96–60 000 µs, result always 0 = timeout)
NR 101  nanosleep            (~12 ms)
```

`NR 113 → 22 → 101 → 113 → 22 → 101 → …`

This is Go's network poller thread: `clock_gettime` to compute the deadline,
`epoll_pwait` to wait for I/O events (times out every time — **result is always 0**),
then `nanosleep` to yield before polling again.

**Not a single file I/O syscall** (`openat`, `read`, `write`, `fstat`) appears in
the log. The build never read a single source file.

PIDs 49, 50, 51 have **no log entries at all**. Since they are CLONE_VM threads
they log under PID 48's entry (all share the same process info page and thus the
same `read_current_pid()` value). The absence of any record means each of those
threads entered a single blocking syscall at startup and has never returned from
it in 30+ minutes. The most likely candidate is `futex(FUTEX_WAIT)` — Go parks
idle OS threads this way.

### Diagnosis

The Go runtime successfully spawned OS threads but its **goroutine scheduler
deadlocked before running any goroutines**:

- Thread 48 (Go network poller): running, but `epoll_pwait` returns 0 on every
  call — no events are ever delivered.
- Threads 49–51 (Go worker OS threads): parked in `futex(FUTEX_WAIT)` from the
  moment they were created; never woken.
- No goroutine has been scheduled on any thread. No build work has started.

### Hypotheses (priority order)

**H1 — epoll never fires for Go's internal scheduler eventfd.**

Go's runtime creates an `eventfd` at startup and registers it with the epoll fd
(`EPOLL_CTL_ADD, EPOLLIN`). When a goroutine becomes runnable, the scheduler
writes to the eventfd to wake the poller, which in turn wakes a parked OS thread.
If our epoll implementation does not track `eventfd` readiness correctly (e.g.
`EventFd` is not handled in the epoll readiness check in `poll.rs`), the eventfd
write is never reflected as `EPOLLIN`, so the poller never wakes, and the parked
threads stay parked.

Check: `src/syscall/poll.rs` — does the `epoll_pwait` readiness loop handle
`FileDescriptor::EventFd(_)`? Compare with how `PidFd` readiness was added in
fix #7.

**H2 — `futex(FUTEX_WAKE)` does not correctly wake CLONE_VM sibling threads.**

The goroutine scheduler uses `futex(FUTEX_WAKE)` to unpark a sleeping OS thread.
If `sys_futex` in `src/syscall/sync.rs` looks up the futex address in the wrong
address space (e.g. using the calling thread's page table instead of the shared
CLONE_VM address space), the wake may silently do nothing.

Check: does `sys_futex` resolve the futex VA relative to the address-space owner,
or relative to the current thread? For CLONE_VM threads the physical page backing
the futex word is the same for all threads — but if the kernel translates the VA
through different page tables it may get different physical addresses for the same
VA and the waiter/waker never agree on a key.

**H3 — `epoll_pwait` with a signal mask (`sigmask` argument) returns immediately.**

Go passes a non-null `sigmask` to `epoll_pwait`. Our implementation may ignore
the mask (acceptable) but it must not return 0 events prematurely. If the handler
returns 0 when it should block, the poller spins at maximum speed (the ~60 ms
`epoll_pwait` durations visible in the log show it IS blocking, but something
could cause early returns).

This is lower priority — the durations look correct for a blocking wait.

### Fix

Root cause confirmed as **H2** (futex missed-wakeup race).

In `src/syscall/sync.rs`, the `FUTEX_WAIT` path read the futex value **outside**
the `FUTEX_WAITERS` lock:

```rust
// BEFORE (racy):
let mut current_val: u32 = 0;
copy_from_user_safe(&mut current_val, uaddr, 4); // outside lock
if current_val != val { return EAGAIN; }
let mut waiters = FUTEX_WAITERS.lock();
waiters.entry(uaddr).or_default().push(tid);   // inserted after value read
```

Race window: if `futex_wake` fires between the value read and the TID push, it
finds an empty queue (returns 0), sets no sticky flag, and the waiter calls
`schedule_blocking(u64::MAX)` — parking forever because the waker already ran.

Fix: move the value read **inside** the lock so the check and push are atomic
with respect to concurrent `futex_do_wake` calls:

```rust
// AFTER (race-free):
let mut waiters = FUTEX_WAITERS.lock();
let mut current_val: u32 = 0;
copy_from_user_safe(&mut current_val, uaddr, 4); // inside lock
if current_val != val { return EAGAIN; }
waiters.entry(uaddr).or_default().push(tid);     // atomic with wake
```

A concurrent wake now either:
- Runs **before** we lock → it already changed the futex word → we see the
  new value → return EAGAIN immediately (no park).
- Runs **after** we push → it finds our TID, calls `get_waker_for_thread(tid).wake()`
  → sticky flag set → `schedule_blocking` returns immediately.

Covered by `src/sync_tests.rs`: `test_futex_wake_before_wait` (race scenario)
and `test_futex_basic_wake` / `test_futex_wake_all` / `test_futex_requeue`
(multi-threaded wake paths).

---

## 15. Signal frame: complete `ucontext_t`, FPSIMD state, and `rt_sigreturn` mask restore

**Status:** Fixed (2026-03-16) in `src/exceptions.rs`
**Component:** `src/exceptions.rs` — `try_deliver_signal`, `do_rt_sigreturn`

### Symptom

Go binaries using SIGURG (goroutine preemption) or SIGSEGV handlers crashed
re-entrantly with `FAR=0x48` (nil goroutine pointer dereference inside the
signal handler) after the first signal was delivered. Log:

```
[signal] Delivering sig 23 to handler 0x10093180 (restorer=0x2000)
[WILD-DA] pid=48 FAR=0x48 ELR=0x100956f4
```

### Root cause

The signal frame written by `try_deliver_signal` left `uc_stack` and
`uc_sigmask` zeroed. Go's `sigtramp` reads `uc_stack.ss_flags` to determine
whether the signal arrived on the altstack; with `ss_flags=0` (SS_ONSTACK not
set), Go searched for its goroutine via `m.gsignal` but `g.m` was nil because
the goroutine struct page hadn't been copied to the child address space (see
fix #16). Additionally, FP/NEON registers were not saved/restored across signal
delivery, so signal handlers that use floating-point corrupted the goroutine's
FP state.

### Fix

Six changes in `src/exceptions.rs`:

1. **`uc_stack`/`uc_sigmask`** — populate at `new_sp + SIGFRAME_UCONTEXT`:
   - `uc_stack.ss_sp/flags/size` from `proc.sigaltstack_*` when on altstack
   - `uc_sigmask` from `proc.signal_mask`

2. **SA_NODEFER** — block the delivered signal during handler execution
   (`proc.signal_mask |= 1u64 << (signal-1)`) unless `SA_NODEFER` (0x40000000)
   is set.

3. **`rt_sigreturn` mask restore** — read `uc_sigmask` from the signal frame
   and restore to `proc.signal_mask` (clearing SIGKILL/SIGSTOP bits).

4. **FPSIMD extension record** — save all 32 V-registers + fpsr/fpcr from the
   kernel stack NEON save area into `__reserved` at `SIGFRAME_FPSIMD` (offset
   576); restore on `rt_sigreturn`. `SIGFRAME_SIZE` extended from 592 to 1112.

5. **`si_code` fix** — use `SI_TKILL=-6` for software-sent signals (tkill,
   SIGURG) instead of hardcoded `SEGV_MAPERR=1`.

6. **Extended register dump** — WILD-DA/WILD-IA handlers now print x8–x28
   (including x28 = Go's goroutine pointer) for crash diagnosis.

---

## 16. VFORK child gets zeroed goroutine struct — crash at FAR=0x90

**Status:** Fixed (2026-03-16) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `fork_process` — lazy region page copy

### Symptom

VFORK children (Go's `rawVforkSyscall` spawning compile/link tools) crashed
immediately with `FAR=0x90`, which is a null goroutine pointer dereference:
`ldr x0, [x28, #0x30]` (load g.m) → x0=0 → `ldr x?, [x0, #0x90]` → SIGSEGV.

### Root cause

`fork_process` copied the stack, code+heap, dynamic linker, and tracked
`mmap_regions`, then called `clone_lazy_regions` which copies only lazy-region
**metadata**. Go's goroutine struct lives in a demand-paged heap arena (e.g.
`0xc40021c0`) that is registered as a lazy region but not in `mmap_regions`.
When the VFORK child accessed this page, demand-paging gave it a zeroed frame
instead of the parent's goroutine data, so `g.m` read as nil.

### Fix

After copying `mmap_regions` and before `clone_lazy_regions`, iterate the
parent's lazy region list and copy any physically-mapped pages (up to 4096
pages = 16 MB cap to bound fork latency for large heaps):

```rust
const MAX_FORK_LAZY_PAGES: usize = 4096;
for (va, size) in lazy_ranges {
    for i in 0..pages {
        if lazy_pages_copied >= MAX_FORK_LAZY_PAGES { break 'lazy_copy; }
        if let Some(src_phys) = translate_user_va(parent_l0, page_va) {
            if let Ok(frame) = new_proc.address_space.alloc_and_map(...) {
                memcpy(src, phys_to_virt(frame.addr), PAGE_SIZE);
            }
        }
    }
}
```

`translate_user_va` returns `None` for unmapped (not yet demand-paged) pages,
so sparse lazy regions iterate quickly without copying.

---

## 17. VFORK parent hangs forever when child crashes

**Status:** Fixed (2026-03-16) in `src/syscall/proc.rs`, `src/exceptions.rs`
**Component:** fault exit paths — `vfork_complete` not called on fault

### Symptom

After a VFORK child crashed due to a fault (SIGSEGV, SIGILL, etc.), the parent
thread blocked forever in `schedule_blocking(u64::MAX)`. The kernel log showed
the last VFORK clone but no further output:

```
[T28.59] [clone] flags=0x4111 stack=0x0
[T28.62] [epoll] pwait enter: pid=48 epfd=5 timeout=0ms
[HUNG]
```

### Root cause

`vfork_complete(child_pid)` was only called from `sys_exit`, `sys_exit_group`,
and `do_execve`. When a VFORK child died via a hardware fault (data abort,
instruction abort, BRK, EL1 fault recovery), none of those paths were taken,
so `VFORK_WAITERS` still held the parent TID and `schedule_blocking` never
returned.

### Fix

Made `vfork_complete` `pub(crate)` and added calls at every fault-induced exit
path in `src/exceptions.rs`:

- `el1_fault_recovery_pad` path (EC=0x25 EL1 data abort)
- Data abort EL0 (`EC_DATA_ABORT_LOWER`) — before `return_to_kernel(-11)`
- Instruction abort EL0 (`EC_INST_ABORT_LOWER`) — before `return_to_kernel(-11)`
- `rt_sigreturn` failure — before `return_to_kernel(-11)`
- BRK / SIGTRAP — before `return_to_kernel(-5)`
- Unknown EC — before `return_to_kernel(-1)`

Each call is `if let Some(pid) = read_current_pid() { vfork_complete(pid); }`.
`vfork_complete` is a no-op if the PID has no VFORK waiter, so adding it to
all paths is safe.

---

## 18. `compile -V=full` produces empty output — argv rejected by user_va_limit

**Status:** Fixed (2026-03-16) in `src/syscall/mod.rs`
**Component:** `user_va_limit()` — too-low upper bound for user VA validation

### Symptom

```
go: parsing buildID from go tool compile -V=full: unexpected output:
```

`go build` failed immediately after the first VFORK child (the `-V=full` build
ID check) returned exit code 0 with empty stdout. The subsequent actual
compilation (1.70 s) succeeded (code 0) but go still exited with code 1.

### Root cause

`sys_execve` reads argv with `validate_user_ptr`, which calls `user_va_limit()`.
That function returned `proc.memory.stack_top` ≈ 0xa4508000 (the ELF fixed
stack top).

Go on AArch64 allocates goroutine stacks and M-structs from high virtual
arenas. On this binary the arenas sit at 0x203e000000 ≈ 130 GB, well above
both `stack_top` and the 4 GB limit. Every user pointer in that range fails
the `end > user_va_limit()` check.

For `sys_execve`: Go's `forkAndExecInChild1` places its argv array on the
current goroutine stack (heap memory at 0xc???_???? – 0x203?_????). With the
wrong upper bound the argv reading loop breaks immediately, `args` comes out
empty, and compile starts with `argc=0`, sees no `-V=full`, prints nothing,
exits 0 → "unexpected output:".

For other syscalls (e.g. `sigaltstack` called during runtime init): the
goroutine stack pointer passed as `ss_ptr` is at 0x203???_???? > limit →
EFAULT → Go runtime crashes dereferencing a null goroutine pointer.

### Fix

Return `0x0000_FFFF_FFFF_FFFFu64` (48-bit / standard Linux user VA limit)
from `user_va_limit()` instead of `stack_top`. The kernel's own addresses are
in the TTBR1 range (bit 63 = 1), so excluding those is the only necessary
cap. The real safety for arbitrary pointers is `is_current_user_range_mapped`
(the page-table walk immediately after) and the EL1 data-abort recovery path.

---

## 19. `compile -V=full` produces empty stdout — `go build` fails with "unexpected output:"

**Status:** Fixed (2026-03-17) — two refcount bugs found and patched
**Component:** `src/syscall/pipe.rs`, `src/syscall/fs.rs`

### Symptom

`go build` exits with:

```
go: parsing buildID from go tool compile -V=full: unexpected output:
[exit code: 1]
```

The compile tool runs for ~1.6 s, returns exit code 0, but Go sees empty stdout.

### Root cause: two pipe refcount bugs

#### Bug 1 — `sys_fcntl(F_DUPFD / F_DUPFD_CLOEXEC)` did not call `pipe_clone_ref`

When Go's runtime duplicates a pipe fd via `fcntl(fd_r, F_DUPFD_CLOEXEC, ...)`, the kernel copies the `PipeRead(id)` entry to a new fd slot but the pipe's `read_count` was NOT incremented.  Closing the original fd_r then decremented `read_count` to 0, even though the duplicate fd still referenced the pipe.  This made `pipe_can_write` return false (no readers), and could cause downstream logic to see a premature EOF on the write end.

**Fix:** `sys_fcntl` F_DUPFD / F_DUPFD_CLOEXEC now calls `pipe_clone_ref` before `alloc_fd`, mirroring `sys_dup` and `sys_dup3`.

#### Bug 2 — `sys_dup3` had a TOCTOU race on shared fd tables (CLONE_FILES)

The old implementation was:

```rust
// NOT atomic together:
let old_entry = proc.get_fd(newfd);   // 1. read under lock, then release
pipe_close(old_entry);                 // 2. close outside lock
pipe_clone_ref(new_entry);             // 3. bump refcount outside lock
proc.set_fd(newfd, new_entry);         // 4. write under lock
```

Between steps 1 and 4, a concurrent goroutine (Go uses `CLONE_FILES`/`CLONE_VM` — all goroutines share the same fd table Arc) could have `pipe2()`'d a new `PipeRead` into the `newfd` slot.  `set_fd` would then silently overwrite it via `BTreeMap::insert` **without** calling `pipe_close_read`, leaking the refcount downward: `read_count` stays at 1 but there is no longer any fd entry holding it. When Go later closes other fds that drive `read_count` to 0, the pipe is destroyed prematurely.

**Fix:** A new `swap_fd` method atomically replaces the fd table entry and returns the displaced old entry in a single `BTreeMap::insert` call under the spinlock.  `sys_dup3` now:
1. Calls `pipe_clone_ref` for the new entry (before swapping in, so the source fd cannot be freed underneath us)
2. Calls `swap_fd` (atomic get-and-replace under lock)
3. Calls `pipe_close` for the returned old entry (after the lock is released)

This closes the race window entirely.

### Pipe lifecycle (expected — after both fixes)

| Event | write_count | read_count |
|---|---|---|
| `pipe_create` | 1 | 1 |
| `fcntl(fd_r, F_DUPFD_CLOEXEC)` → `pipe_clone_ref` | 1 | 2 |
| `close(fd_r)` | 1 | 1 |
| `fork` → `clone_deep_for_fork` (bumps both) | 2 | 2 |
| child: `dup3(fd_w, 1, O_CLOEXEC)` → `pipe_clone_ref` | 3 | 2 |
| execve cloexec closes child fd_r duplicate | 3 | 1 |
| parent wakes from vfork, closes fd_w | 2 | 1 |
| **compile writes to fd=1** | 2 | 1 → succeeds |
| compile exits → fd=1 closed | 1 | 1 |
| parent reads pipe → closes fd_r_dup | 1 | 0 |
| parent closes fd_w | 0 | 0 → pipe destroyed |

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

### Fix

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

### Fix

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

### Fix

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

### Fix

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

### Fix

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

### Fix

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

### Fix

1.  Updated `sys_tkill` to correctly identify the target process via `find_pid_by_thread(tid)` and use that process's `signal_actions` and `signal_mask`.
2.  Ensured that fatal-by-default signals (like `SIGPIPE`) only trigger `exit_group` if they are **not** blocked in the target's mask. If blocked, they are pended for later delivery.

---

## 30. `rt_sigtimedwait` (137) unimplemented — Go signal forwarding hang

**Status:** Fixed (2026-03-19) in `src/syscall/signal.rs`
**Component:** `sys_rt_sigtimedwait`

### Symptom

Go binaries using signal-heavy synchronization (or musl-based binaries) would return `ENOSYS` or hang during signal-wait loops.

### Fix

Implemented `sys_rt_sigtimedwait`. It checks for pending signals and blocks the thread with a timeout if none are available. It correctly populates `siginfo_t` if requested.

---

## 31. Signal handlers not shared across threads — `CLONE_SIGHAND` violation

**Status:** Fixed (2026-03-19) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `SharedSignalTable`, `Process`

### Symptom

If one thread set a signal handler via `sigaction`, other threads in the same process would not see it, continuing to use the default disposition. This is non-compliant with Linux/POSIX thread semantics.

### Fix

Refactored `Process.signal_actions` into an `Arc<SharedSignalTable>`. `clone_thread` now performs an `Arc::clone`, ensuring all threads in a group share exactly one signal table protected by a `Spinlock`.

---

## 32. `SA_RESTART` ignored — spurious `EINTR` in binaries

**Status:** Fixed (2026-03-19) in `src/exceptions.rs`
**Component:** `try_deliver_signal`

### Symptom

Syscalls like `read` or `nanosleep` would return `EINTR` even when the signal handler was registered with `SA_RESTART`. This caused unnecessary retry loops or failures in binaries that expect the kernel to handle the restart.

### Fix

Implemented automatic restart logic. If `SA_RESTART` is set for a signal delivered during a syscall, the kernel now decrements the saved `ELR` by 4 bytes, causing the processor to re-execute the `SVC` instruction upon returning from the signal handler.

---

## 34. `rt_sigtimedwait` (137) unimplemented — Go signal forwarding hang

**Status:** Fixed (2026-03-19) in `src/syscall/signal.rs`
**Component:** `sys_rt_sigtimedwait`

### Symptom

Go binaries using signal-heavy synchronization (or musl-based binaries) would return `ENOSYS` or hang during signal-wait loops.

### Fix

Implemented `sys_rt_sigtimedwait`. It checks for pending signals and blocks the thread with a timeout if none are available. It correctly populates `siginfo_t` if requested.

---

## 35. Signal handlers not shared across threads — `CLONE_SIGHAND` violation

**Status:** Fixed (2026-03-19) in `crates/akuma-exec/src/process/mod.rs`
**Component:** `SharedSignalTable`, `Process`

### Symptom

If one thread set a signal handler via `sigaction`, other threads in the same process would not see it, continuing to use the default disposition. This is non-compliant with Linux/POSIX thread semantics.

### Fix

Refactored `Process.signal_actions` into an `Arc<SharedSignalTable>`. `clone_thread` now performs an `Arc::clone`, ensuring all threads in a group share exactly one signal table protected by a `Spinlock`.

---

## 36. `SA_RESTART` ignored — spurious `EINTR` in binaries

**Status:** Fixed (2026-03-19) in `src/exceptions.rs`
**Component:** `try_deliver_signal`

### Symptom

Syscalls like `read` or `nanosleep` would return `EINTR` even when the signal handler was registered with `SA_RESTART`. This caused unnecessary retry loops or failures in binaries that expect the kernel to handle the restart.

### Fix

Implemented automatic restart logic. If `SA_RESTART` is set for a signal delivered during a syscall, the kernel now decrements the saved `ELR_EL1` by 4 bytes, causing the processor to re-execute the `SVC` instruction upon returning from the signal handler.

---

## 37. Per-process current-syscall visibility — poor debugging

**Status:** Fixed (2026-03-19) in `src/syscall/mod.rs` and `src/shell/commands/builtin.rs`
**Component:** `Process`, `ps`

### Symptom

`ps` would show `SYSCALL=-` for all processes, making it difficult to debug stuck threads.

### Fix

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

**Fix:** Added per-kernel-thread-slot arrays `THREAD_SIGALTSTACK_{SP,SIZE,FLAGS}` in `crates/akuma-exec/src/threading/mod.rs`. `sys_sigaltstack` now reads/writes `threading::get/set_sigaltstack(current_thread_id())`. `try_deliver_signal` in `src/exceptions.rs` reads from the same per-thread array. Each thread slot is reset to SS_DISABLE when the slot is freed.

## 40. pend_signal_for_thread did not wake sleeping thread (FIXED 2026-03-20)

**Symptom:** SIGURG pended on a Go M that was blocked in `FUTEX_WAIT` was silently lost. The thread only woke on timeout or an explicit `FUTEX_WAKE`, so the Go preemption mechanism didn't work reliably.

**Root cause:** `pend_signal_for_thread` stored the signal number in `PENDING_SIGNAL[tid]` but never called `get_waker_for_thread(tid).wake()`, so a thread parked in `schedule_blocking` inside `sys_futex` would not be rescheduled.

**Fix:** Added `get_waker_for_thread(tid).wake()` call at the end of `pend_signal_for_thread`.

## 41. FUTEX_WAIT returned 0 not EINTR when woken by signal (FIXED 2026-03-20)

**Symptom:** When a signal woke a thread from `FUTEX_WAIT`, the syscall returned 0 (success) instead of -EINTR. Go ignores the futex return value in its `futexsleep` wrapper so this wasn't directly crashing, but it violates the Linux spec.

**Fix:** After `schedule_blocking` returns in the `FUTEX_WAIT` path, `peek_pending_signal(tid)` is checked; if non-zero the syscall returns EINTR before checking the timeout.

## 42. FUTEX_WAIT_BITSET + CLOCK_REALTIME absolute timeout mishandled (FIXED 2026-03-20)

**Symptom:** `FUTEX_WAIT_BITSET` with `FUTEX_CLOCK_REALTIME` passed an absolute wall-clock `timespec`. The kernel was treating it as a relative timeout and adding `uptime_us()`, causing threads to sleep far into the future (wall-clock seconds are much larger than uptime microseconds on a freshly-booted VM).

**Fix:** When `(op & FUTEX_CLOCK_REALTIME) != 0` and `cmd == FUTEX_WAIT_BITSET`, the `timeout_us` value is used directly as the deadline (treated as uptime microseconds — imprecise but prevents multi-hour sleeps). Also added: `FUTEX_WAIT_BITSET` with `val3==0` now returns EINVAL per spec.

## 43. uc_stack in signal frame still used process-level sigaltstack (FIXED 2026-03-20)

**Symptom:** After fix §39 (per-thread sigaltstack), `futexwakeup addr=0xc4047158 returned -22` and `SIGSEGV PC=0x20000000` persisted. New diagnostic log showed `[futex] EINVAL: uaddr=0x1 op=129` — Go calling `FUTEX_WAKE` with a clearly corrupted address (0x1). Registers at crash time contained ASCII bytes of the string `"futexwakeup addr="`, proving Go's goroutine stack data had been overwritten.

**Root cause:** `try_deliver_signal` was correctly placing the signal frame on the per-thread sigaltstack (using `get_sigaltstack(thread_slot)`) for the stack selection and re-entrancy check, but the `uc_stack` field written into the `ucontext_t` inside the frame still read from `proc.sigaltstack_sp` / `proc.sigaltstack_size` — which are always 0 for CLONE_VM threads (they only update the per-thread arrays). Go's runtime reads `uc_stack.ss_sp` and `uc_stack.ss_flags` from the signal frame to determine whether the signal was delivered on gsignal. With `uc_stack` showing zeros (SS_ONSTACK not set), Go concluded the signal landed on the goroutine stack and adjusted its internal stack-tracking state accordingly, triggering corruption of Go's goroutine and M state.

**Fix:** Changed the `uc_stack` population in `try_deliver_signal` (`src/exceptions.rs`) to use `alt_sp` / `alt_size` (already computed from `get_sigaltstack(thread_slot)`) rather than `proc.sigaltstack_sp` / `proc.sigaltstack_size`. The `on_altstack` predicate now checks `alt_sp != 0` instead of `proc.sigaltstack_sp != 0`.

**New tests:** `test_sigaltstack_syscall_roundtrip`, `test_rt_sigreturn_restores_registers`, `test_uc_stack_uses_per_thread_sigaltstack`, and `test_futex_wait_eintr_signal_preserved` were added to `src/sync_tests.rs` to exercise these paths in isolation.

## 44. u32 truncation underflow in `test_futex_wait_eintr_signal_preserved` (FIXED 2026-03-20)

**Symptom:** The kernel test `test_futex_wait_eintr_signal_preserved` panicked with `unexpected ret 0xfffc (65532)` immediately after being added.

**Root cause:** The test packed the futex return value and the pending signal number into a single `AtomicU32` using `(ret as u32) << 16 | sig`. `EINTR = -4 = 0xFFFF_FFFF_FFFF_FFFC` as a `u64`; casting to `u32` gives `0xFFFF_FFFC`. Shifting left by 16 in a 32-bit type wraps: `0xFFFF_FFFC << 16 = 0xFFFC_0000`. Reading back `>> 16` then yielded `0xFFFC` (65532) instead of the expected `-4`.

**Fix:** Replaced the single packed `AtomicU32` with a dedicated `AtomicU64` for the return value (`EINTR_RET`) and a separate `AtomicU32` for the signal (`EINTR_SIG`), each with independent sentinel values. The main thread now waits on the `EINTR_SIG` sentinel and reads `EINTR_RET` as a full `u64`, casting to `i64` for the sign check.

## 45. SIGURG delivered before Go M calls sigaltstack — goroutine stack corruption (FIXED 2026-03-20)

**Symptom:** `go build` crashed with `futexwakeup addr=0xc4047158 returned -22` and `SIGSEGV PC=0x20000000` even after all previous fixes were applied. Kernel logs showed `[futex] EINVAL: uaddr=0x1 op=129` immediately after a SIGURG delivery — a goroutine-local futex address variable had been overwritten with a byte from the signal frame.

**Root cause:** Go Ms register their SIGURG (preemption) handler with `SA_ONSTACK|SA_SIGINFO`, which tells the kernel to deliver the signal on the gsignal alternate stack. However, `sigaltstack` is called during `mstart` *after* the OS thread is created. If the Go scheduler sends SIGURG to a newly created M before it has executed `sigaltstack`, `alt_sp == 0` for that kernel thread slot.

When `alt_sp == 0` and `SA_ONSTACK` is set, the previous code fell through to the `else` branch and placed the signal frame at the goroutine's current SP. The signal frame (`rt_sigframe`, 1112 bytes) was written over live goroutine stack variables. In particular, Go's `asyncPreempt` handler calls `asyncPreempt2` (a regular Go function), which may grow the stack further downward — clobbering any goroutine variables that happened to lie just below the current SP. One of these overwritten values was a futex address, which became `0x1` (a byte from the signal frame zero-fill), producing the EINVAL.

The `PC=0x20000000` SIGSEGV that followed was a secondary crash: another goroutine had its saved `pc` field (in `uc_mcontext` at `ucontext+432`) clobbered similarly, causing `rt_sigreturn` to restore a garbage PC.

**Fix (`src/exceptions.rs` `try_deliver_signal`):** Added an early-return guard: if `(action.flags & SA_ONSTACK) != 0` and `alt_sp == 0`, the signal is re-pended via `pend_signal_for_thread(thread_slot, signal)` and `try_deliver_signal` returns `false`. The signal will be retried at the next syscall boundary. By that point `mstart` will have called `sigaltstack`, so `alt_sp` will be non-zero and delivery will succeed on the correct gsignal stack.

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

**Fix (`src/exceptions.rs`):** Immediately after `do_rt_sigreturn` succeeds, the kernel now runs the same pending-signal check that exists at the end of the normal syscall return path:

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

**Fix (`src/syscall/sync.rs`):** The `[futex] EINVAL` log now reads `elr_el1` and prints it alongside `uaddr` and `op`, making it straightforward to identify the faulting SVC site in future crashes:

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

## 48. `SA_RESTART` rewinds ELR for completed syscalls — `[futex] EINVAL uaddr=0x1` (2026-03-20)

**Status:** Fixed (2026-03-20) in `src/exceptions.rs`
**Component:** `src/exceptions.rs` — `try_deliver_signal`

### Symptom

After the fix in §46 (pending signal redelivery after `rt_sigreturn`), the `[futex] EINVAL: uaddr=0x1` crash still appeared intermittently:

```
[signal] Deliver SIGURG (23) to 0x10093180 on slot 1, alt_sp=0xc400c000 ... elr=0x10078238 new_sp=0xc400b860
[futex] EINVAL: uaddr=0x1 op=129 elr=0x10078238 (null or unaligned)
futexwakeup: futex(...) returned -22
```

The key evidence is that the `elr` in the EINVAL log matches the `elr` where the signal was delivered — `0x10078238`. This is the instruction **after** the `SVC` call.

### Root cause

A subtle but critical bug was found in the `SA_RESTART` implementation. When a
signal was delivered *after* a syscall completed but *before* the syscall's
return value was processed, the `SA_RESTART` logic would rewind `ELR` by 4 bytes,
assuming the syscall had been interrupted and needed to be restarted.

This was incorrect for syscalls that had already completed successfully. For
example, a `FUTEX_WAKE` syscall that wakes one waiter returns `1`. If a signal
arrived at this exact moment, the sequence was:
1. `sys_futex` returns `1`.
2. `try_deliver_signal` is called.
3. `SA_RESTART` logic sees the flag, assumes an interrupted syscall, and does
   `elr_el1 -= 4`, backing the PC up to the `SVC` instruction.
4. The signal handler runs and returns via `rt_sigreturn`.
5. Execution resumes at the `SVC` instruction, but with `x0` now holding the
   *return value* (`1`) from the first call, not the original `uaddr` argument.
6. `sys_futex` is re-executed with `uaddr=1`, which is unaligned, causing an
   `EINVAL` error.

### Fix

The fix, implemented in `try_deliver_signal` in `src/exceptions.rs`, is to
gate the `ELR` backup. The backup now only occurs if the syscall's return value
(in `frame.x0`) is `-4` (EINTR) or `-512` (ERESTARTSYS), indicating it was genuinely
interrupted. For any other return value (success or a different error), `ELR`
is not modified.

### New tests (`src/sync_tests.rs`)

- `test_sa_restart_not_applied_to_successful_futex_wake` — Directly asserts that the `elr -= 4` condition is only true for `EINTR`/`ERESTARTSYS`.
- `test_futex_sequential_wake_no_einval` — Regression test: a successful `FUTEX_WAKE` returning 1 followed immediately by another `FUTEX_WAKE` must not produce EINVAL.
- `test_pipe_epipe_for_nonexistent_pipe_id` — Secondary effect test: the crash log showed EPIPE from other goroutines after the futex goroutine died. This verifies our EPIPE return codes are correct.
- `test_rt_sigreturn_pending_redelivery` — Directly tests the `take_pending_signal` invariant from the §46 fix.
- `test_pipe_multi_process_lifecycle` — Regression test for #49: verify that a pipe survives when one process closes its FDs but another still has them open.
- `test_pipe_large_transfer` — Stress test for #50: transfers 1MB of data through a pipe to verify `VecDeque` performance and flow control.

## 49. Broken Pipe / Premature Pipe Destruction (2026-03-20)

**Status:** Fixed (2026-03-20) in `crates/akuma-exec/src/process/mod.rs` and `src/syscall/fs.rs`
**Component:** `crates/akuma-exec` — `SharedFdTable`, `src/syscall/fs.rs` — `sys_read`/`sys_write`

### Symptom

Go build processes would fail with `signal: broken pipe` and the kernel would log `[pipe] write WARN: pipe id=X not found (len=25)`. This indicated that the pipe object was being destroyed while processes still had file descriptors pointing to it. Additionally, some syscalls were returning incorrect error codes (e.g., `EPERM` instead of `EBADF`) when given invalid file descriptors.

### Root cause

The bug was in the file descriptor cleanup logic and error reporting:
1.  **Delayed Cleanup (Hangs):** When a process exited, its FDs remained "open" in the kernel's view until the process was reaped (the zombie dropped). For pipes, this meant readers would hang waiting for more data instead of seeing EOF immediately upon the writer's death.
2.  **Premature Cleanup (Shared Tables):** With `CLONE_FILES`, multiple processes share the same `SharedFdTable`. Closing resources when one process exited would break them for all others in the group.
3.  **Global Refcount Issues:** If multiple processes had independent FD tables pointing to the same pipe (e.g., after a `fork`), the global `KernelPipe` reference counts were being decremented prematurely if the cleanup logic was too aggressive.
4.  **Incorrect Error Codes:** `sys_read` and `sys_write` were returning generic errors instead of `EBADF` when a file descriptor was not found.

### Fix

The fix involved three parts:
1.  **Correct Error Codes:** `sys_read` and `sys_write` now return `EBADF` (-9).
2.  **Immediate & Automatic Resource Lifecycle Management:** 
    - `SharedFdTable` implements `close_all()` which iterates and explicitly closes all underlying kernel resources (pipes, sockets, etc.) and clears the internal table.
    - `SharedFdTable` implements `Drop`, which calls `close_all()`.
    - `cleanup_process_fds` calls `close_all()` *immediately* when the last process in a thread group (or an independent process) exits, even while it remains a zombie.
3.  **Correct Sharing Semantics:** `CLONE_VM` threads share the `Arc<SharedFdTable>`, so resources only close when the *entire group* is gone. `fork()`ed processes get a deep copy with incremented pipe refcounts, so they manage their own lifetime correctly.

## 50. Pipe Read Performance (Quadratic Slowdown) (2026-03-20)

**Status:** Fixed (2026-03-20) in `src/syscall/pipe.rs`
**Component:** `src/syscall/pipe.rs`

### Symptom

Large Go builds would hang or take extremely long (minutes) inside the kernel during `read` syscalls. Logs showed `in_kernel` times exceeding 100 seconds.

### Root cause

The `KernelPipe` implementation used a `Vec<u8>` for its buffer. `pipe_read` performed `pipe.buffer.drain(..n)`, which is an **O(N)** operation because it shifts all remaining elements to the front of the vector. For a process writing megabytes of data, every small read (e.g., 4KB) triggered a massive memory shift, leading to quadratic $O(N^2)$ performance.

### Fix

Replaced `Vec<u8>` with `VecDeque<u8>` in `KernelPipe`. `VecDeque::drain` is efficient (O(1) amortized for front removal), eliminating the memory shifting bottleneck.

This is verified by the `test_pipe_large_transfer` test in `src/sync_tests.rs`, which transfers 1MB of data in 1KB chunks.
