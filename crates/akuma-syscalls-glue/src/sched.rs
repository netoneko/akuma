//! `getcpu(2)` through glue: validate, copy the pair out, errno.
//!
//! The decision (`akuma-syscalls-linux::sched::cpu_node`) is pure and
//! host-tested; this module is the user-pointer half, exactly like every
//! other arm here (the `cred.rs` validate → copy → `EFAULT` shape). The
//! calling core's index arrives as a parameter from the machine half —
//! glue has no CPU notion of its own, and no hook (R7).

use super::*;

/// `getcpu(cpu, node, tcache)`: write the caller's CPU index and NUMA node.
///
/// The CPU index goes to `cpu` and the node to `node`; either pointer may be NULL (Linux allows that).
/// `tcache` has been ignored by Linux since 2.6.24 — ignored here too,
/// recorded rather than implemented. Returns 0, or `EFAULT` when a given
/// pointer is not writable.
pub fn sys_getcpu(cpu_ptr: UserAddr, node_ptr: UserAddr, current_cpu: u32) -> u64 {
    let cpu_ptr = cpu_ptr.get();
    let node_ptr = node_ptr.get();
    let (cpu, node) = akuma_syscalls_linux::sched::cpu_node(current_cpu);
    if cpu_ptr != 0 {
        if !validate_user_ptr(cpu_ptr, 4) {
            return EFAULT;
        }
        if copy_to_user(cpu_ptr, &cpu.to_le_bytes()).is_err() {
            return EFAULT;
        }
    }
    if node_ptr != 0 {
        if !validate_user_ptr(node_ptr, 4) {
            return EFAULT;
        }
        if copy_to_user(node_ptr, &node.to_le_bytes()).is_err() {
            return EFAULT;
        }
    }
    0
}
