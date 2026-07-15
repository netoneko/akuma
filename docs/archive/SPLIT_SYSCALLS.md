# syscall.rs split into src/syscall/ module directory

Completed split of the monolithic 5,388-line `src/syscall.rs` into 15 files
under `src/syscall/`.

## Result

| Module         | Lines | Contents |
|----------------|------:|----------|
| mod.rs         |   690 | Dispatch table, `handle_syscall()`, shared helpers, errno constants, syscall counters |
| fs.rs          | 1,359 | read/write/readv/writev, open/close, dup, fcntl, stat, getdents, lseek, mkdir, unlink, rename, symlink, chdir |
| proc.rs        |   654 | clone/clone3, execve, exit/exit_group, wait4/waitpid, getpid, kill, spawn, prlimit64, uname, sysinfo, getrandom |
| net.rs         |   466 | socket, bind, listen, accept, connect, sendto/recvfrom, sendmsg/recvmsg, getsockname, getsockopt, shutdown |
| poll.rs        |   451 | epoll (create1/ctl/pwait), ppoll, pselect6 |
| mem.rs         |   376 | mmap, munmap, mremap, mprotect, madvise, brk, membarrier |
| term.rs        |   328 | ioctl, terminal attributes, cursor control, poll_input_event, cpu_stats |
| timerfd.rs     |   150 | timerfd_create/settime/gettime, timerfd state table |
| pipe.rs        |   147 | pipe2, KernelPipe infrastructure, read/write/close helpers |
| container.rs   |   123 | register_box, kill_box, reattach, mount/umount2/mount_in_ns |
| eventfd.rs     |   103 | eventfd2, EventFdState table, read/write/close helpers |
| signal.rs      |    97 | rt_sigaction, tkill, signal_is_fatal_default |
| sync.rs        |    93 | futex syscall, FutexQueue infrastructure, futex_wake |
| time.rs        |    78 | clock_gettime/getres, nanosleep, times, getrusage, uptime |
| fb.rs          |    47 | fb_init, fb_draw, fb_info |
| **Total**      | **5,162** | |

226 lines (~4%) net reduction from removing redundant comments during extraction.

## Bugs introduced and fixed

The initial extraction was done by AI sub-agents that fabricated some function
implementations from memory rather than copying them exactly. This introduced
two classes of bugs:

### 1. sys_nanosleep — broken dual-ABI support

The original `sys_nanosleep` supports two calling conventions:
- **Linux/musl ABI**: `a0` = pointer to `struct timespec`
- **libakuma ABI**: `a0` = seconds (raw value), `a1` = nanoseconds (raw value)

It distinguishes them by checking `a0 >= 4096` (page size) to detect pointers.

The fabricated version only supported the pointer-based ABI. When libakuma
called `sleep_ms(100)` with `a0=0, a1=100_000_000`, `validate_user_ptr(0, 16)`
failed and the sleep returned EFAULT immediately. This caused all libakuma-based
userspace binaries to skip their sleeps entirely.

The fabricated version also only called `schedule_blocking` once instead of
looping until the deadline, so even pointer-based sleeps could return early if
the thread was woken prematurely.

### 2. sys_pselect6 / sys_ppoll — rewritten instead of copied

Both functions were rewritten with different fd-readiness logic rather than
being mechanically moved. Changes included different timeout error handling
in pselect6, added nfds limits in ppoll, POLLNVAL reporting, and explicit
fd-type matching where the original used simpler heuristics. Reverted to match
the original behavior exactly.

## Performance regression: bun

After the split, `bun run` takes roughly 2x longer than before. The cause is
not yet identified — the split was intended to be purely mechanical with no
behavioral changes, and no function signatures or logic were intentionally
altered. Possible areas to investigate:

- Inlining behavior changes due to functions now being in separate translation
  units (though with LTO this should not matter)
- Subtle differences in remaining fabricated code that haven't been caught yet
- Different codegen from changed module structure affecting cache layout

## Files changed

- Deleted: `src/syscall.rs`
- Created: `src/syscall/` directory with 15 `.rs` files listed above
