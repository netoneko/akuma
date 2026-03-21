# Proposal: Split syscall.rs into src/syscall/ module directory

## Problem

`src/syscall.rs` is 5,388 lines covering 108 syscall implementations plus
~2,900 lines of embedded kernel infrastructure (epoll, futex, pipes, eventfd,
timerfd). It is the hardest file to navigate in the codebase and is growing
with every new syscall. There are no subdirectories or modules — everything
lives in one flat file.

## Approach

Pure mechanical split. No semantic changes, no new abstractions, no crate
boundaries. Move code into a module directory, run `cargo check`, done.

### Target layout

```
src/syscall/
  mod.rs      — dispatch table, handle_syscall(), shared helpers
                (validate_user_ptr, errno mapping, syscall_counters)
  fs.rs       — read, write, readv, writev, pread64, pwrite64,
                openat, close, dup, dup3, fcntl, fstat, newfstatat,
                fstatfs, fchmod, fchmodat, getdents64, lseek,
                readlinkat, unlinkat, mkdirat, getcwd, chdir,
                faccessat2, rename, truncate, ftruncate
  mem.rs      — mmap, munmap, mremap, mprotect, madvise, brk
  proc.rs     — clone, clone3, execve, exit_group, exit,
                wait4, getpid, getppid, gettid, getpgid, setpgid,
                setsid, getuid, geteuid, getgid, getegid,
                prlimit64, uname, sysinfo, getrandom, set_tpidr_el0
  net.rs      — socket, bind, listen, accept4, connect,
                sendto, recvfrom, sendmsg, recvmsg,
                getsockname, getpeername, getsockopt, setsockopt,
                shutdown
  poll.rs     — epoll infrastructure (EpollSet, EpollEntry, global map),
                epoll_create1, epoll_ctl, epoll_pwait,
                ppoll, pselect6
  sync.rs     — futex wait queue infrastructure (FutexQueue, global map),
                futex syscall
  pipe.rs     — pipe infrastructure (KernelPipe, global pipe table),
                pipe2, plus read/write integration helpers used by fs.rs
  eventfd.rs  — eventfd infrastructure (EventFdState, global table),
                eventfd2, plus read/write integration helpers
  timerfd.rs  — timerfd state tracking (TimerFdState, global table),
                timerfd_create, timerfd_settime, timerfd_gettime,
                plus read integration helpers
  time.rs     — clock_gettime, clock_getres, nanosleep, times,
                time, getrusage, uptime
  term.rs     — ioctl, set_terminal_attributes, poll_input_event,
                set_cursor_position, terminal-related ioctl dispatch
  fb.rs       — framebuffer syscalls (sys_set_fb, sys_flip_fb, etc.)
  container.rs — register_box, kill_box, mount, umount2, mount_in_ns,
                 namespace-related syscalls
  signal.rs   — rt_sigaction, rt_sigprocmask, rt_sigreturn,
                kill, tgkill, sigaltstack, signal infrastructure
```

### Shared helpers stay in mod.rs

The following currently live at the top of syscall.rs and are used
across all submodules — keep them in `mod.rs`:

- `validate_user_ptr` / `validate_user_str` / `validate_user_slice`
- `syscall_counters` (per-syscall call count tracking)
- `current_syscall_nr` / `set_current_syscall_nr`
- errno constants re-exports

### Infrastructure modules

`poll.rs`, `sync.rs`, `pipe.rs`, `eventfd.rs`, and `timerfd.rs` each own
both the kernel data structures (global maps, state types) and the syscall
implementations that operate on them. This mirrors how the file is already
organised internally — the infrastructure is already co-located with its
consumers.

## Steps

1. `mkdir src/syscall`
2. `mv src/syscall.rs src/syscall/mod.rs`
3. Verify `cargo check` still passes (it will — nothing changed yet)
4. For each target module, working one at a time:
   a. Create the file (`src/syscall/fs.rs` etc.)
   b. Move the relevant function bodies and types into it
   c. Add `pub mod fs;` (or `mod fs;`) to `mod.rs`
   d. Add `use super::*;` or explicit imports in the new file
   e. Run `cargo check` after each file — fix any missing `use` items
5. After all files are created, remove dead code from `mod.rs`
6. Final `cargo check` / `cargo build --release`

### Order of operations (safest to hardest)

| Step | Module | Notes |
|------|--------|-------|
| 1 | `time.rs` | Pure delegation, no embedded state |
| 2 | `fb.rs` | Small, no shared state |
| 3 | `container.rs` | Mostly delegates to akuma_isolation |
| 4 | `signal.rs` | Self-contained infrastructure |
| 5 | `mem.rs` | Touches PMM/lazy regions but no embedded tables |
| 6 | `net.rs` | Delegates to akuma_net — clean boundary |
| 7 | `proc.rs` | Delegates to akuma_exec — already clean |
| 8 | `term.rs` | Delegates to akuma_terminal |
| 9 | `fs.rs` | Largest module, but delegates to VFS |
| 10 | `pipe.rs` | Has embedded KernelPipe state; extract carefully |
| 11 | `eventfd.rs` | Has embedded EventFdState table |
| 12 | `timerfd.rs` | Has embedded TimerFdState table |
| 13 | `sync.rs` | Futex wait queue is complex; do last |
| 14 | `poll.rs` | Epoll infrastructure is largest (1800 LOC); do last |

## What does NOT change

- No new abstractions or trait objects
- No function signatures change
- No crate boundary — everything stays in `src/`
- No feature flags added
- No behavior change of any kind
- Existing tests continue to compile unmodified

## Verification

```bash
cargo check --release          # must pass with same warnings, no new errors
cargo build --release          # must produce identical binary size (±linking noise)
cargo run --release            # boot smoke-test in QEMU
```

After splitting, a secondary cleanup pass can:
- Add `#[cfg(feature = "networking")]` around `net.rs` import
- Add `#[cfg(feature = "containers")]` around `container.rs` import
- Add `#[cfg(feature = "framebuffer")]` around `fb.rs` import

This is the point where feature-gating pays off — at the module level, not the
crate level — and only after the split makes it cheap to do so.

## Estimated scope

| Module | Approx. lines |
|--------|--------------|
| mod.rs (after split) | ~300 |
| fs.rs | ~700 |
| poll.rs (incl. epoll infra) | ~1,900 |
| sync.rs (incl. futex infra) | ~950 |
| proc.rs | ~450 |
| net.rs | ~400 |
| pipe.rs (incl. infra) | ~200 |
| mem.rs | ~350 |
| time.rs | ~200 |
| signal.rs | ~250 |
| term.rs | ~200 |
| eventfd.rs | ~150 |
| timerfd.rs | ~150 |
| container.rs | ~100 |
| fb.rs | ~50 |
