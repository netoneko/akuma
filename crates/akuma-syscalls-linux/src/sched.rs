//! The sched-family wire answer: `getcpu(2)`.
//!
//! The decision lives here — pure, host-tested (R3). The user-pointer work is
//! glue's (`akuma-syscalls-glue::sched::sys_getcpu`) and the machine state —
//! the calling core's index — is the dispatching kernel's
//! (`ring3::cpu_index()`), passed in as a parameter.

/// The `getcpu(2)` answer for a task running on core `cpu`.
///
/// The core's own index and NUMA node 0 (the guest is single-NUMA; the taoxichuan aarch64
/// precedent wrote the same pair — `docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`).
///
/// The cpu is the **caller's** core, never a constant: a hardcoded cpu is the
/// BUG_FIX_LIST's hardcoded-256MB failure shape, and HotSpot sizes worker
/// pools from what this reports.
#[must_use]
pub fn cpu_node(cpu: u32) -> (u32, u32) {
    (cpu, 0)
}

#[cfg(test)]
mod tests {
    use super::cpu_node;

    /// The answer names the caller's own core — a constant would herd every
    /// thread's bookkeeping onto one cpu on an SMP guest.
    #[test]
    fn getcpu_answers_the_callers_core_never_a_constant() {
        for cpu in [0u32, 1, 3, 63, u32::MAX] {
            let (answered, node) = cpu_node(cpu);
            assert_eq!(answered, cpu);
            assert_eq!(node, 0);
        }
    }

    /// Node is always 0: the guest is single-NUMA, by decision.
    #[test]
    fn getcpu_node_is_always_zero_single_numa_guest() {
        assert_eq!(cpu_node(0).1, 0);
        assert_eq!(cpu_node(2).1, 0);
        assert_eq!(cpu_node(u32::MAX).1, 0);
    }
}
