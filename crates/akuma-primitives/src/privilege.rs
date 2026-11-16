//! Proof that the caller is allowed to do something to the whole machine.
//!
//! Every privilege decision in the syscall layer used to be a `bool` (`caller_is_host_root()`), and the operation it
//! guarded took no evidence, so the check and the effect were only connected by the author remembering both. That is
//! how nine box checks stayed unconsulted while every test passed (`BOX_ISOLATION_SECURITY_FIXES.md` §1), and the
//! shape of the security audit's H2 and M9 (`SECURITY_AUDIT_2026_11_16.md`). A [`HostRoot`] is what the check returns
//! instead: the effects (`reboot`, mount/unmount, setting the wall clock) take one, so a missing check is a compile
//! error in the syscall that forgot it.
//!
//! # What this does not do
//!
//! The token is not unforgeable: kernel code can call [`HostRoot::kernel_boot`] or [`HostRoot::vouch_checked`]. Its
//! value is that *forgetting* the check stops compiling and *forging* one is a grep. There are exactly two mint sites
//! by name:
//!
//! - [`HostRoot::vouch_checked`], called from `akuma_exec::process::host_root()` after it has tested the current
//!   process's box and uid, and nowhere else;
//! - [`HostRoot::kernel_boot`], for kernel code with no caller to check (boot-time mounts, the NTP step at boot).
//!
//! The rule "an `akuma-syscalls-*` crate never names `kernel_boot`" is a review rule, not a type.

/// The caller is uid 0 in the host box (or is the kernel itself). Zero-sized; `Copy` so one check can cover the several
/// effects of one syscall.
#[derive(Clone, Copy, Debug)]
pub struct HostRoot {
    _private: (),
}

impl HostRoot {
    /// Mint after testing a caller's identity. **Only** `akuma_exec::process::host_root()` may call this.
    #[must_use]
    pub const fn vouch_checked() -> Self {
        Self { _private: () }
    }

    /// Mint for the kernel acting on its own behalf, with no caller to check.
    #[must_use]
    pub const fn kernel_boot() -> Self {
        Self { _private: () }
    }
}
