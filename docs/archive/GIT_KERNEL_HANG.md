# Git Kernel Hang

Based on my thorough analysis of rustc.log, here's what I found:

## Root Cause Analysis

**The hang location:** `git maintenance run --auto --no-quiet --detach` (pid=87) freezes after its guard page mmap at T511.74. The kernel appears to completely stop (no further logs of any kind — no timer, no Thread0 heartbeat, no Mem logs, nothing).

**The key pattern:** Two git maintenance processes run in this log:
- pid=84 at T511.02 (spawned by git fetch) — **succeeds**, completes, forks its daemon child, exits
- pid=87 at T511.67 (spawned by git merge) — **hangs** after exactly the same guard page mmap that pid=84 got through fine

**What the EINVAL nr=134 entries mean:** These are NOT related to your filesystem test. `nr=134` is `rt_sigaction`. The kernel correctly returns EINVAL when git tries to set signal handlers for SIGKILL (sig 9) and SIGSTOP (sig 0x13=19) — those signals are uncatchable. This happens before execve in the pre-exec setup and is expected behavior.

**The PermissionDenied → EPERM test failure:** This is a separate issue. `src/syscall/fs.rs:13` maps `FsError::PermissionDenied → EACCES` (correct Linux semantics — filesystem permission errors should be EACCES, not EPERM). A test was written before this mapping was correct and still expects `-1` (EPERM). The test needs updating, not the kernel.

## The Actual Hang

After the guard page mmap, musl's startup code normally calls `mprotect` ~7ms later (consistent across pid=82, 83, 84, 86). For pid=87 this mprotect never happens. Since the `[mprotect]` log fires at the very start of sys_mprotect (before any locking), the hang is either:

1. **User space never reaching mprotect** — musl is stuck in an infinite loop (possible if a user-space spinlock or atomic is in a permanently-locked state)
2. **The syscall dispatch itself hanging** — some kernel state left by the first git maintenance run (pid=84) that affects the second one

The complete system freeze (no timer logs) starting right after T511.74 strongly suggests a scheduler or interrupt delivery issue triggered by this specific sequence. The timer fires every ~12ms; the next `[TMR]` log would have appeared at ~T513.69 (6 seconds after the last at T507.69) — the hang at T511.74 cuts that short.

## Practical Fix

The hang is in git's background maintenance. You can disable it:

```bash
# In the VM, after git clone/pull:
git config --global maintenance.auto false
```

Or configure the repo to skip post-operation maintenance in `/akuma-playground/.git/config`:
```ini
[maintenance]
    auto = false
```

This prevents `git merge` from spawning `git maintenance --detach` and hitting the hang. The actual kernel bug causing the complete freeze after the second git maintenance startup would need GDB or additional register-dump logging to pinpoint precisely.
