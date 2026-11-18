// SPDX-License-Identifier: BSD-2-Clause
//! Decision at native syscall entry, before number translation and dispatch.

pub use akuma_seccomp::{AUDIT_ARCH_AARCH64, AUDIT_ARCH_AMD64, Data as SyscallData};
use akuma_seccomp::{Action, Verdict};

pub const SIGSYS: u32 = 31;
pub const SYS_SECCOMP: i32 = 1;

#[derive(Clone, Copy)]
pub struct Trap {
    pub call_addr: u64,
    pub syscall: i32,
    pub arch: u32,
    pub errno: u16,
}

#[derive(Clone, Copy)]
pub enum Decision {
    Dispatch,
    LogAndDispatch,
    Return(u64),
    KillThread,
    KillProcess,
    Trap(Trap),
}

#[must_use]
pub fn decide(data: &SyscallData, verdict: Verdict) -> Decision {
    match verdict.action() {
        Action::Allow => Decision::Dispatch,
        Action::Log => Decision::LogAndDispatch,
        Action::Errno(errno) => Decision::Return(0u64.wrapping_sub(u64::from(errno))),
        Action::Trace => Decision::Return((-38i64).cast_unsigned()), // ENOSYS, no tracer
        Action::KillThread => Decision::KillThread,
        Action::KillProcess => Decision::KillProcess,
        Action::Trap(errno) => Decision::Trap(Trap {
            call_addr: data.instruction_pointer,
            syscall: data.nr,
            arch: data.arch,
            errno,
        }),
    }
}
