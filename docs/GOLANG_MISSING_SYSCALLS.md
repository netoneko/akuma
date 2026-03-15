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

## 12. Known remaining gaps (not yet fixed)

The following are likely to surface as Go workloads grow more complex:

| Syscall / feature | Notes |
|---|---|
| `rt_sigtimedwait` | Used by Go's signal forwarding; currently unimplemented |
| Signal mask during handler | Full `sa_mask` blocking during handler execution not implemented; re-entrant detection (fix #2) covers the common crash case |
| `clone(CLONE_SIGHAND)` | Shared signal tables across threads not implemented |
| `epoll` + goroutine scheduler | Go's netpoller uses `epoll_pwait`; this is implemented and capped at 10 ms polling interval (see issue #6 in `KNOWN_ISSUES.md`) |
| Unmapped runtime data above g0 stack | Go places `m` struct adjacent to `g0.stack.hi`; if that region is not covered by a lazy mmap region the handler loop fix masks the crash but the underlying missing mapping is unresolved |
| SA_RESTART semantics | `restart_syscall` (128) returns EINTR instead of actually restarting; programs relying on transparent syscall restart after signal may need to retry manually |
