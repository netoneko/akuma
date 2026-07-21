# Forktest_parent Go Hanging Issue - Investigation Prompt

## Problem Statement

The Go-based `forktest_parent` stress test (from `userspace/forktest/`) hangs under SMP=2 and SMP=4, failing to complete within its specified duration despite starting successfully.

## Observed Behavior

**Test command:**
```bash
/bin/forktest_parent -num_children=1 -duration=1s
```

**Output:**
```
forktest_parent: Starting with 1 children, duration=1s (deadline 2026-07-21T20:36:00Z).
forktest_parent: Launching child 0...
# [hangs indefinitely, never exits]
```

**Process listing after hang:**
```
  11 0 0:00 /bin/forktest_parent -num_children=1 -duration=1s
  12 0 0:00 /bin/forktest_parent -num_children=1 -duration=1s
  13 0 0:00 /bin/forktest_parent -num_children=1 -duration=1s
  14 0 0:00 /bin/forktest_parent -num_children=1 -duration=1s
```

**Kernel diagnostics:**
- 0 `[BKL] RECOVERED` events
- 0 `[WATCHDOG]` events
- 0 crashes (no `WILD-DA`, `SIGSEGV`, `panic`)
- Process performs syscalls normally (futex, nanosleep, mmap, munmap, epoll_create1, epoll_ctl)
- SSH receives "Connection closed by remote host"

## Known Working vs Failing

**Working:**
- `busybox true` fork loops (both SMP=2 and SMP=4)
- 384 concurrent forks via `sshd_crash_hunt.py` (SMP=4, 3 boots, 0 crashes)
- CoW/TLB fixes confirmed working

**Failing:**
- Go `forktest_parent` with any duration (tested: 1s, 3s, 5s)
- Both SMP=2 and SMP=4
- Basic mode (no stress flags) hangs immediately

## Investigation Leads

### 1. Go Runtime Futex/Epoll Patterns
- Go uses futex for goroutine parking/unparking
- forktest_parent uses epoll for child process pipe monitoring
- Check if futex wake-ups are missed under SMP scheduler
- Verify epoll edge-triggered behavior with multiple cores

### 2. Parent-Child Pipe Communication
- forktest_parent creates pipes for each child's stdout/stderr
- Uses epoll to monitor pipe readiness
- Check if pipe read/write deadlocks occur under BKL
- Verify pipe close/destroy logic with concurrent access

### 3. Go Timer/Sleep Interaction
- Go's `time.Sleep` uses futex-based timers
- Kernel tick is 10ms (CNTV timer)
- Check if Go timer wake-ups are delayed or missed
- Verify monotonic clock vs kernel timer alignment

### 4. Goroutine Channel Deadlocks
- Go channels may block on futex internally
- Check channel send/receive under SMP scheduling
- Verify channel close/broadcast correctness

## Files to Investigate

**Kernel:**
- `src/syscall/futex.rs` — futex FUTEX_WAIT/FUTEX_WAKE implementation
- `src/syscall/epoll.rs` — epoll edge-triggered behavior
- `src/syscall/pipe.rs` — pipe close/destroy race conditions
- `src/timer.rs` — timer tick delivery to sleeping processes
- `crates/akuma-exec/src/threading/mod.rs` — scheduler wake decisions

**Userspace (forktest):**
- `userspace/forktest/parent/main.go` — epoll monitoring, child spawning
- `userspace/forktest/child/main.go` — stress test goroutines

**Reference:**
- `docs/archive/GO_FORK_EXEC_FIXES.md` — Go futex/epoll history
- `docs/archive/GOLANG_IPC.md` — Go IPC patterns

## Debugging Commands

```bash
# Boot and test
SMP=2 MEMORY=2048 overlays/devbox/run-smoltcp.sh

# Check if forktest_parent started
ssh -p 2222 root@localhost "/bin/forktest_parent -num_children=1 -duration=1s"

# In another terminal, monitor processes
ssh -p 2222 root@localhost "watch -n 1 'ps ax | grep forktest'"

# Check futex syscalls (if instrumented)
grep "futex" kernel_log

# Check epoll syscalls
grep "epoll" kernel_log
```

## Hypothesis Priority

1. **High:** Go futex wake-up missed under SMP scheduler (similar to historic Go futex issues)
2. **Medium:** Epoll edge-triggered race with multiple cores writing pipes
3. **Medium:** Pipe close sequence deadlock when parent terminates
4. **Low:** Go timer tick misalignment with kernel 10ms tick

## Success Criteria

- forktest_parent completes within specified duration
- No process leaks (multiple forktest_parent PIDs should not appear)
- Correct child process cleanup
- No kernel crashes or deadlocks

## Next Steps

1. Add futex/epoll instrumentation to trace wake-up patterns
2. Test with `-num_children=0` to isolate parent-only behavior
3. Test Go simple program that just sleeps (no fork/epoll)
4. Compare single-core (SMP=1) vs SMP=2/4 behavior
5. Check if issue exists in the C version (if available)