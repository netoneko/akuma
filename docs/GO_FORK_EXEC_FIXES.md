# Go Fork/Exec Fixes (forktest_parent)

## Date

2026-04-02 to 2026-04-03

---

## Summary

Made Go's `fork+exec` work end-to-end on Akuma. `forktest_parent -duration 30s
-combined_stress` now launches 3 children, each running 70k+ syscalls over 30 seconds,
all receiving SIGTERM gracefully, and the parent returning to the shell prompt.

Also fixed `go build` (the Go compiler toolchain) which successfully compiled 30/31
packages before hitting a timeout on the large `unicode/tables.go` file.

---

## Bugs Fixed (in order)

### 1. PROCESS_INFO_ADDR overwritten by cow_share_range

**Symptom:** Vfork child read `pid=49` (parent) instead of `pid=53` (child) from
PROCESS_INFO_ADDR.  Child never called execve, entered clone loop instead.

**Root cause:** For Go ARM64 binaries, `code_start = PAGE_SIZE = 0x1000 =
PROCESS_INFO_ADDR`.  `cow_share_range(code_start, brk_len)` copied the parent's PTE
for 0x1000 into the child, overwriting the child's process info mapping.

**Fix:** Re-map PROCESS_INFO_ADDR to the child's own frame AFTER all CoW sharing
(step 5 in fork_process).

**File:** `crates/akuma-exec/src/process/mod.rs`

### 2. clone(flags=0) routing and garbage flag cascade

**Symptom:** Vfork child called `clone(flags=0)` which created a fork bomb (each
fork child ran the Go scheduler).

**Root cause:** Go's `rawVforkSyscall` leaks register state.  The child calls
`clone(0)` due to leftover registers.  Routing clone(0) to fork_process was wrong.

**Fix:** Only route `CLONE_VFORK` or `SIGCHLD` (0x11 in low byte) to fork_process.
Everything else returns ENOSYS.  Added bits-32+ guard: `if flags >> 32 != 0 { return
ENOSYS; }` to catch negative error codes (-38, -11) that coincidentally have
CLONE_THREAD|CLONE_VM bits set.

**File:** `src/syscall/proc.rs`

### 3. clone_thread stack=0 crash

**Symptom:** Bogus threads created with SP=0, crashing at FAR=0x28.

**Root cause:** Garbage clone flags entered clone_thread with stack=0.  The bits-32+
guard (fix #2) prevents this now.  Additional stack=0 guard added as defense in depth.

**File:** `crates/akuma-exec/src/process/mod.rs`

### 4. sys_kill ignored signal argument

**Symptom:** All killed processes reported exit status 137 regardless of signal sent.

**Root cause:** `sys_kill(pid, _sig)` — the `_sig` parameter was unused.  It called
`kill_process()` which hardcoded `exit_code = 137`.

**Fix:** `sys_kill` now delivers the signal via `pend_signal_for_thread()` +
`interrupt_thread()`.  `kill_process` now uses `exit_code = -9` (negative = killed
by signal).  New `kill_process_with_signal(pid, sig)` for explicit signal kills.

**File:** `src/syscall/proc.rs`, `crates/akuma-exec/src/process/signal.rs`

### 5. exit/exit_group returned to userspace

**Symptom:** Processes marked as exited but threads kept running (epoll loops, futex
calls).  Parent hung waiting.

**Root cause:** `sys_exit` and `sys_exit_group` set `proc.exited = true` and returned
to EL0.  On Linux, exit() never returns.

**Fix:** After marking exited, terminate the calling thread
(`mark_thread_terminated` + yield loop).  Guard: only if `proc.thread_id ==
Some(current_tid)` (kernel helpers must not be terminated).  Also notify I/O channel
and call `unregister_process` to prevent zombies.

**File:** `src/syscall/proc.rs`

### 6. Zombie processes (no cleanup after exit)

**Symptom:** `ps` showed dozens of zombie processes from tests and forktest.

**Root cause:** `sys_exit` terminated the thread but skipped `unregister_process`.
The `on_thread_cleanup` callback couldn't reap processes created by
`spawn_process_with_channel` (which doesn't register in THREAD_PID_MAP).

**Fix:** Call `unregister_process(pid)` in sys_exit/sys_exit_group before
terminating the thread.

**File:** `src/syscall/proc.rs`

### 7. Added tgid (thread group leader ID)

**Symptom:** `kill(pid, SIGTERM)` only targeted one thread.  Go processes have
goroutine threads that need to be interrupted for exit synchronization.

**Fix:** Added `tgid` field to Process struct.  `from_elf`/`fork_process`: `tgid =
pid` (new group leader).  `clone_thread`: `tgid = parent.tgid` (same group).
`sys_kill` now interrupts all threads with matching tgid.  `kill_thread_group` uses
tgid instead of l0_phys matching.

**File:** `crates/akuma-exec/src/process/mod.rs`, `src/syscall/proc.rs`

### 8. futex EFAULT on unmapped address broke Go exit

**Symptom:** Go's exit coordination called `futex(0xfffffffffffffffc, op)`.
Returning EFAULT broke Go's exit path — goroutine threads stayed blocked.

**Root cause:** Address -4 is unmapped.  For FUTEX_WAKE, there can't be waiters.
For FUTEX_WAIT, there's no value to compare.  Go handles non-EFAULT returns but
not EFAULT.

**Fix:** For unmapped addresses: WAKE/WAKE_BITSET/WAKE_OP return 0 (no waiters),
WAIT/WAIT_BITSET return EAGAIN (value mismatch).  Other ops still return EFAULT.

**File:** `src/syscall/sync.rs`

### 9. is_interrupted flag never cleared

**Symptom:** After SIGTERM, every blocking syscall returned EINTR forever.  SSH
connections broke.

**Root cause:** `is_interrupted()` loaded the flag but never cleared it.  Once
`interrupt_thread()` set it, the flag stayed true permanently.

**Fix:** `is_interrupted()` now uses `swap(false)` — auto-clears on read.

**File:** `crates/akuma-exec/src/process/channel.rs`

### 10. copy_to_user_safe in clone_thread broke Go startup

**Symptom:** Go runtime crashed at FAR=0x88 (nil m-pointer) during goroutine
thread startup.

**Root cause:** `copy_to_user_safe`'s byte-by-byte `strb` through the fault
handler silently returned EFAULT for Go's `mp.procid` page, leaving it as 0.

**Fix:** Reverted to `core::ptr::write`.  The bits-32+ guard prevents garbage
flags from reaching clone_thread, so CoW-RO pages can't be an issue.

**File:** `crates/akuma-exec/src/process/mod.rs`

### 11. EL1 user-copy fault handler fast path (reverted)

**Symptom:** Kernel deadlock during test (hang at `test_parallel_processes`).

**Root cause:** Moving `get_user_copy_fault_handler()` check before the debug
dump in the EL1 handler caused a POOL lock deadlock when an EL1 data abort
fired while POOL lock was held.

**Fix:** Reverted.  The debug dump noise is acceptable.

**File:** `src/exceptions.rs`

---

## IA-DP Messages

`[IA-DP] file region: fault_va=0x10135390 seg_va=0x10010000 filesz=0x8bdee0`

**Instruction Abort - Demand Paging.**  Completely normal.  The kernel lazily loads
ELF text/data pages from disk on first access.  Each IA-DP = one 4KB page loaded from
the binary file.  The Go compiler binary is ~9.2 MB, so hundreds of IA-DP messages
are expected on first run.  Not a problem.

---

## Go Build (`go build -x -v -o ./hello.bin`)

Successfully compiled 30/31 packages.  The 31st (`unicode`) was killed after the
`go` tool's 150-second total build time elapsed.  `unicode/tables.go` is enormous
and the Go compiler was still running when the parent exited.

- **Not an Akuma bug** — performance limitation under QEMU emulation
- **VA space is fine** — 256 GB stack top, 128 GB mmap space; compile used only ~8.7 GB
- **Memory is fine** — RAM: 1517/2048 MB free, Heap: 137/256 MB free

---

## Current State

| Operation | Status |
|-----------|--------|
| Fork+exec chain | WORKS |
| Go goroutine thread creation | WORKS |
| SIGTERM delivery to Go processes | WORKS (all 3 children exit gracefully) |
| Process cleanup (no zombies) | WORKS |
| Go build (compiler toolchain) | 30/31 packages compiled |
| SSH stability after forktest | WORKS |

---

## Files Changed

| File | Changes |
|------|---------|
| `crates/akuma-exec/src/process/mod.rs` | PROCESS_INFO_ADDR re-map after CoW; tgid field; clone_thread stack=0 guard; kill_thread_group uses tgid; reverted copy_to_user_safe |
| `crates/akuma-exec/src/process/signal.rs` | exit_code=-9 (not 137); kill_process_with_signal() |
| `crates/akuma-exec/src/process/channel.rs` | is_interrupted() auto-clears via swap(false) |
| `src/syscall/proc.rs` | sys_kill delivers signals properly; sys_exit/exit_group terminate thread + unregister; clone flag routing + bits-32+ guard; tgid-based thread group interrupt |
| `src/syscall/sync.rs` | futex WAKE/WAIT on unmapped address: non-fatal returns |
| `src/exceptions.rs` | EL1 fault handler fast path attempted then reverted |
| `src/process_tests.rs` | ~20 new regression tests |
| `src/tests.rs` | tgid field in test Process structs |
