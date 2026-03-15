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

## 6. Known remaining gaps (not yet fixed)

The following are likely to surface as Go workloads grow more complex:

| Syscall / feature | Notes |
|---|---|
| `rt_sigtimedwait` | Used by Go's signal forwarding; currently unimplemented |
| `tgkill` | Go uses this to send signals to specific threads; `tkill` is implemented but does not actually invoke userspace handlers |
| Signal mask during handler | Full `sa_mask` blocking during handler execution not implemented; re-entrant detection (fix #2) covers the common crash case |
| `clone(CLONE_SIGHAND)` | Shared signal tables across threads not implemented |
| `epoll` + goroutine scheduler | Go's netpoller uses `epoll_pwait`; this is implemented and capped at 10 ms polling interval (see issue #6 in `KNOWN_ISSUES.md`) |
| Unmapped runtime data above g0 stack | Go places `m` struct adjacent to `g0.stack.hi`; if that region is not covered by a lazy mmap region the handler loop fix masks the crash but the underlying missing mapping is unresolved |
| SA_RESTART semantics | `restart_syscall` (128) returns EINTR instead of actually restarting; programs relying on transparent syscall restart after signal may need to retry manually |
