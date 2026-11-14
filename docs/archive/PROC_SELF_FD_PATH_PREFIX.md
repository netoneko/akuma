# Claude Code's Bash tool: `EIO: i/o error, mkdir '/tmp/claude-0/-root'` (`/proc/self/fd/N/<name>`)

Found 2026-11-10. **Status: FIXED** (amd64 verified in a 4-vCPU Firecracker guest on the ryzen's real
p3 userspace). Reported in [`CLAUDE_COMPATIBILITY_ISSUES.md`](CLAUDE_COMPATIBILITY_ISSUES.md) §1.

## Symptom

Every Bash-tool call in Claude Code running on Akuma failed before the command started:
`EIO: i/o error, mkdir '/tmp/claude-0/-<project>/<session>/tasks'`. Read/Write tools worked.

## Capture

`claude-glm -p "Use the Bash tool to run: echo hello"` in the guest booted with `strace_err`
(`ptrace` is `ENOSYS` here, so `strace(1)` cannot be used; the kernel's own `[sc!]` lines are the tool):

```
[sc!] nr=257 .../tmp/claude-0/-root                       -> -2
[sc!] nr=83  a2=0x1ff "/proc/self/fd/11/-root"            -> -5     <- EIO
```

Bun opens `/tmp/claude-0` as fd 11 and creates the child with `mkdir("/proc/self/fd/11/-root")`. On
Linux `/proc/self/fd/N` is a magic link, so the walk continues inside the directory fd 11 names.
Here the path fell through to procfs, which cannot create entries and answered `EIO`. Bun's own
`fs.mkdirSync(.., {recursive:true})` on plain paths worked, which is why the first probe did not
reproduce it.

## Fix

`expand_own_proc_fd_prefix` (`akuma-syscalls-glue/src/fs.rs`) rewrites `/proc/self/fd/N/<rest>` (or
`/proc/<own pid>/fd/N/<rest>`) to `<path of fd N>/<rest>`; used by `resolve_path_at` (every `*at`
resolver), `mkdirat` and `unlinkat`. A bare `/proc/self/fd/N` is untouched (`openat` reopens those
itself). After the fix the same run prints `hello-from-bash`, and no failing call returns `-5`.

## Other `ENOSYS` the capture found

| nr | call | per run | effect | status |
|---|---|---|---|---|
| 439 | `faccessat2` | 16 | Bun's `which`/`access` fall back | **added** (amd64 row; glue had the arm) |
| 436 | `close_range` | 10 | every spawn walked the fd limit: 9111 `fcntl` `EBADF` | **added**; 9111 -> 2 |
| 40 | `sendfile` | 11-15 | falls back to read/write | open |
| 294 | `inotify_init1` | 3-6 | file watching falls back to polling | open |

Linux numbers calls from 424 up identically on every architecture, so the two new rows have the same
digit in both tables; the ABI test now checks that instead of assuming every call differs.
`close_range` runs the amd64 close hooks (`/dev/fb0`, tap, wifi) per open fd before glue frees them.

## Open

- `sendfile`, `inotify` as above. `mremap` returned `ENOMEM` 4094 times in the run (not examined).
- Not deployed to the ryzen/trashcan metal kernels yet.
- Separate bug found on the way: [`MKDIR_UNDER_REGULAR_FILE.md`](MKDIR_UNDER_REGULAR_FILE.md).
