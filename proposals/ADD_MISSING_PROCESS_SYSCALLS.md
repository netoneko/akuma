# Add Missing Process Syscalls: wait4 process groups + get_robust_list

## Date

2026-04-07

## Context

Linux compatibility audit found two missing syscalls that could affect Go programs:

1. `wait4(pid < -1)` — wait for children in process group `|pid|`
2. `get_robust_list` (syscall 100) — query robust futex list head

## 1. wait4 process group support (pid < -1)

### Current behavior

`sys_wait4` in `src/syscall/proc.rs:664-789` handles:
- `pid > 0`: wait for specific child ✓
- `pid == -1 || pid == 0`: wait for any child ✓
- `pid < -1`: falls through to ECHILD ✗

### Linux semantics

When `pid < -1`, wait for any child whose `pgid == |pid|`.

### Implementation

Add a new branch in `sys_wait4` after the `pid == -1 || pid == 0` case:

```rust
} else if pid < -1 {
    let target_pgid = (-pid) as u32;
    // Same logic as pid==-1 but filter by pgid
    // Use find_exited_child_in_pgid(target_pgid) instead of find_exited_child(current_pid)
}
```

### New helper needed

In `crates/akuma-exec/src/process/children.rs`:

```rust
pub fn find_exited_child_in_pgid(pgid: Pid) -> Option<(Pid, Arc<ProcessChannel>)> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().iter().find_map(|(&child_pid, (ch, _parent))| {
            if ch.has_exited() {
                // Check if child's pgid matches
                if let Some(proc) = lookup_process(child_pid) {
                    if proc.pgid == pgid {
                        return Some((child_pid, ch.clone()));
                    }
                }
            }
            None
        })
    })
}

pub fn add_poller_to_pgid_children(pgid: Pid, poller_tid: usize) {
    // Same as add_poller_to_all_children but filters by pgid
}
```

### Files to change

- `src/syscall/proc.rs` — add `pid < -1` branch in sys_wait4
- `crates/akuma-exec/src/process/children.rs` — add pgid-based lookup helpers

### Tests

Kernel test in `src/process_tests.rs`:
- `test_wait4_pgid_finds_matching_child` — register parent + 2 children with different pgids, verify wait4 with pid < -1 finds correct child
- `test_wait4_pgid_no_match_returns_echild` — no children in target pgid → ECHILD

## 2. get_robust_list (syscall 100)

### Current behavior

Not implemented. Returns ENOSYS.
`set_robust_list` (99) IS implemented — stores head and len in Process struct.

### Linux semantics

```c
long get_robust_list(pid_t pid, struct robust_list_head **head_ptr, size_t *len_ptr);
```

- `pid == 0`: query current thread's robust list
- `pid > 0`: query another thread's robust list (requires ptrace permission)
- Returns 0 on success, -ESRCH if pid not found, -EPERM if no permission

### Implementation

In `src/syscall/proc.rs`:

```rust
pub(super) fn sys_get_robust_list(pid: u64, head_ptr: u64, len_ptr: u64) -> u64 {
    let target_pid = if pid == 0 {
        // Current thread
        match akuma_exec::process::current_process() {
            Some(p) => p.pid,
            None => return ESRCH,
        }
    } else {
        pid as u32
    };

    let (head, len) = match akuma_exec::process::lookup_process(target_pid) {
        Some(p) => (p.robust_list_head, p.robust_list_len),
        None => return ESRCH,
    };

    if head_ptr != 0 && validate_user_ptr(head_ptr, 8) {
        let _ = unsafe { copy_to_user_safe(head_ptr as *mut u8, &head as *const u64 as *const u8, 8) };
    }
    if len_ptr != 0 && validate_user_ptr(len_ptr, 8) {
        let len_u64 = len as u64;
        let _ = unsafe { copy_to_user_safe(len_ptr as *mut u8, &len_u64 as *const u64 as *const u8, 8) };
    }
    0
}
```

### Files to change

- `src/syscall/mod.rs` — add `pub const GET_ROBUST_LIST: u64 = 100;` and dispatch entry
- `src/syscall/proc.rs` — add `sys_get_robust_list` function

### Tests

Kernel test in `src/process_tests.rs`:
- `test_set_get_robust_list_roundtrip` — set_robust_list then get_robust_list returns same values
- `test_get_robust_list_pid_zero_is_current` — pid=0 queries current process

## Priority

- `get_robust_list`: LOW — Go may not call it directly; set_robust_list is the important one
- `wait4 pid < -1`: MEDIUM — Go's `cmd.Wait()` uses waitid (not wait4 with pid < -1), but shells and other tools might use it
