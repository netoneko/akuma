# Seccomp and no_new_privs

**Stability B**

Akuma exposes Linux seccomp filter mode on native AArch64 and amd64. The
`seccomp(2)` syscall number is 277 on AArch64 and 317 on amd64.

- `seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog)` and
  `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)` add a classic BPF filter.
  A caller needs `NO_NEW_PRIVS` or effective `CAP_SYS_ADMIN`. Akuma's
  capability model associates that capability with effective UID 0. An
  unprivileged caller without `NO_NEW_PRIVS` receives `EACCES`.
- `seccomp(SECCOMP_GET_ACTION_AVAIL, 0, &action)` takes a 32-bit action with
  no data bits and returns zero for supported actions or `EOPNOTSUPP`.
- `PR_GET_SECCOMP` returns 0 or 2. `PR_SET_NO_NEW_PRIVS` accepts 1 and zero
  trailing arguments. `PR_GET_NO_NEW_PRIVS` requires four zero arguments
  after the option and returns 0 or 1. Once set, the bit stays set. Akuma's executable loader preserves
  credentials across exec.
- Filters and `NO_NEW_PRIVS` belong to the calling thread. Fork, clone, and
  Akuma's spawn operations inherit a snapshot. Exec preserves the calling
  thread's state. Filters are immutable and accumulate on a thread. A
  later installation by one thread leaves its existing siblings unchanged.

Each syscall is checked before translation, compatibility rewrites or
dispatch, including `rt_sigreturn`. The filter sees the original syscall
number, six arguments, the userspace return instruction pointer, and the
native audit architecture. The validator accepts the Linux seccomp subset
of classic BPF, including aligned 32-bit loads from the 64-byte
`seccomp_data`. It rejects malformed programs, out-of-range jumps, and
reads of uninitialized scratch words. Each filter can contain up to
4096 instructions. The path may contain up to 32768 instructions, charging
four additional instructions for each existing filter. Akuma charges classic
BPF instructions, so the path limit can differ from Linux for instructions
that expand during conversion to its internal form.

The action with highest precedence wins. A tie uses the newest filter's
data. In descending precedence, supported actions have these effects.

| Action | Effect |
|---|---|
| `KILL_PROCESS` | End the thread group with SIGSYS. |
| `KILL_THREAD` | End the caller. A lone thread exits with SIGSYS. |
| `TRAP` | Skip the syscall and deliver synchronous SIGSYS with `SYS_SECCOMP`, the raw syscall, audit architecture, return IP, and action data in `si_errno`. |
| `ERRNO` | Skip the syscall and return the negative action data, capped at 4095. Data zero returns zero. |
| `TRACE` | Skip the syscall and return `ENOSYS`, as Linux does without a tracer. |
| `LOG` | Log the PID and syscall number to the kernel console and run the syscall. |
| `ALLOW` | Run the syscall normally. |

## Linux divergences

Akuma supports filter mode and installation flags of zero. Strict mode,
`TSYNC`, listener creation and `USER_NOTIF`, ptrace seccomp events, and audit
log controls are unsupported. An unsupported return action, including
`USER_NOTIF`, kills the thread group if the action wins precedence. `LOG`
writes to the kernel console. Seccomp deaths publish a signal exit status
without generating a core dump. When `KILL_THREAD` ends a group leader, Akuma
retains its kernel task and address space while workers finish. The ordinary
group exit path publishes the final status.

## Background

[Implementation, source provenance, and verification](../../../archive/SECCOMP.md)
