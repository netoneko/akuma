//! `membarrier(2)` command decode.

use akuma_primitives::errno::negated::EINVAL;

/// Commands this kernel recognises.
///
/// **Divergence 7.** Linux defines more, including `MEMBARRIER_CMD_GLOBAL` (1) and
/// the `*_SYNC_CORE` family; everything not listed here is `EINVAL`.
const CMD_QUERY: u32 = 0;
const CMD_PRIVATE_EXPEDITED: u32 = 8;
const CMD_REGISTER_PRIVATE_EXPEDITED: u32 = 16;

/// The bitmask `MEMBARRIER_CMD_QUERY` reports: `PRIVATE_EXPEDITED` (8) plus
/// `REGISTER_PRIVATE_EXPEDITED` (16).
pub const SUPPORTED: u64 = 0x18;

/// What `sys_membarrier` should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Return the supported-command bitmask.
    Query,
    /// Registration is a no-op here: this kernel needs no per-process state to
    /// serve an expedited barrier. Return 0.
    Register,
    /// Issue `dsb ish; isb`, then return 0.
    ///
    /// The barrier itself stays in the kernel — it is inline assembly, which this
    /// crate forbids, and that is the right split rather than an inconvenience.
    Barrier,
    /// Unrecognised command.
    Invalid,
}

impl Command {
    /// The return value for the commands that do not need the kernel to act.
    ///
    /// [`Command::Barrier`] deliberately has no answer here: its return value is
    /// only correct *after* the barrier has been issued, so the kernel returns 0
    /// itself once it has done so.
    #[must_use]
    pub const fn immediate_result(self) -> Option<u64> {
        match self {
            Self::Query => Some(SUPPORTED),
            Self::Register => Some(0),
            Self::Invalid => Some(EINVAL),
            Self::Barrier => None,
        }
    }
}

/// Decode a `membarrier` command number.
#[must_use]
pub const fn command(cmd: u32) -> Command {
    match cmd {
        CMD_QUERY => Command::Query,
        CMD_REGISTER_PRIVATE_EXPEDITED => Command::Register,
        CMD_PRIVATE_EXPEDITED => Command::Barrier,
        _ => Command::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_recognised_commands() {
        assert_eq!(command(0), Command::Query);
        assert_eq!(command(8), Command::Barrier);
        assert_eq!(command(16), Command::Register);
    }

    /// The query bitmask must name exactly the two commands that succeed without a
    /// barrier, or a caller will ask for something that returns `EINVAL`.
    #[test]
    fn query_reports_exactly_what_is_implemented() {
        assert_eq!(SUPPORTED, (1 << 3) | (1 << 4));
        assert_eq!(command(8), Command::Barrier);
        assert_eq!(command(16), Command::Register);
        for bit in 0..64u32 {
            if SUPPORTED & (1u64 << bit) != 0 {
                assert_ne!(command(1 << bit), Command::Invalid, "cmd {} advertised but invalid", 1 << bit);
            }
        }
    }

    /// **Divergence 7.** `MEMBARRIER_CMD_GLOBAL` is 1 in Linux and unimplemented
    /// here, so it takes the `EINVAL` arm along with everything else unrecognised.
    #[test]
    fn diverge_global_and_unknown_commands_are_einval() {
        assert_eq!(command(1), Command::Invalid);
        assert_eq!(command(2), Command::Invalid);
        assert_eq!(command(32), Command::Invalid);
        assert_eq!(command(u32::MAX), Command::Invalid);
    }

    /// Only the barrier arm needs the kernel to act before returning.
    #[test]
    fn only_the_barrier_defers_its_result() {
        assert_eq!(Command::Query.immediate_result(), Some(SUPPORTED));
        assert_eq!(Command::Register.immediate_result(), Some(0));
        assert_eq!(Command::Invalid.immediate_result(), Some(EINVAL));
        assert_eq!(Command::Barrier.immediate_result(), None);
    }
}
