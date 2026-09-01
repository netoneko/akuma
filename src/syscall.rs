//! Shim over [`akuma_syscalls_glue`].
//!
//! `src/syscall/` — 23 files, ~17k lines — became a crate on 2026-09-01. It was
//! the largest thing left in the binary and the last big one:
//! `docs/archive/SRC_SYSCALL_EXTRACTION.md`.
//!
//! This file registers the seven callbacks the layer cannot reach from a crate
//! and re-exports the rest, so the ~578 `crate::syscall::` references in the boot
//! suite are spelled exactly as they were.

pub use akuma_syscalls_glue::*;

/// Install the binary's callbacks. Called once from `kernel_main`, before any
/// userspace runs.
pub fn register() {
    akuma_syscalls_glue::set_hooks(akuma_syscalls_glue::SyscallHooks {
        box_is_rump: rump::box_is_rump,
        mark_box_rump: rump::mark_box_rump,
        attach_server: rump::attach_server,
        intercept_box_syscall: rump::intercept_box_syscall,
        rump_socket_readable: rump::rump_socket_readable,
        utc_time_us: crate::timer::utc_time_us,
        probed_core_count,
    });
}

#[cfg(kernel_smp_shared)]
fn probed_core_count() -> usize {
    crate::smp_shared::probed_core_count()
}

/// Without real SMP there is one core.
#[cfg(not(kernel_smp_shared))]
fn probed_core_count() -> usize {
    1
}

/// The rump sysproxy five. `src/rump_proxy.rs` only exists under the `rump`
/// feature, so the inert versions below keep `register` one expression instead
/// of five `#[cfg]`s — and keep the hook struct's shape identical either way.
#[cfg(feature = "rump")]
mod rump {
    pub use crate::rump_proxy::{
        attach_server, box_is_rump, intercept_box_syscall, mark_box_rump, rump_socket_readable,
    };
}

#[cfg(not(feature = "rump"))]
mod rump {
    use akuma_exec::process::Pid;
    pub fn box_is_rump(_box_id: u64) -> bool {
        false
    }
    pub fn mark_box_rump(_box_id: u64) {}
    pub fn attach_server(_box_id: u64, _server_pid: Pid) {}
    pub fn intercept_box_syscall(_nr: u64, _args: &[u64; 6]) -> Option<u64> {
        None
    }
    pub fn rump_socket_readable(_rump_fd: i32) -> bool {
        false
    }
}
