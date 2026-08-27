//! Signal-family wire types: `stack_t`, the `siginfo_t` prefix, the kernel's
//! `struct sigaction`, and the `SIGCHLD` siginfo `waitid(2)` fills.
//!
//! Note the distinction the kernel `struct sigaction` carries in its name: the
//! *kernel's* layout is `{ handler, flags, restorer, mask }`, which is not the
//! order libc's `struct sigaction` declares. Userspace never sees this struct —
//! musl marshals into it — so getting the order wrong breaks every signal
//! handler in the system and nothing else.

/// Linux `stack_t` (`sigaltstack(2)`), 24 bytes on aarch64.
///
/// The `_pad` after `flags` is load-bearing: without it `size` would sit at
/// offset 12 and every `sigaltstack` read would return garbage.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct StackT {
    pub sp: u64,
    pub flags: i32,
    pub _pad: i32,
    pub size: u64,
}

/// The `siginfo_t` prefix `rt_sigtimedwait` fills. 128 bytes, matching Linux.
///
/// Only the three leading words are meaningful here; the tail is the union
/// Linux reserves, zeroed. It is a fixed 128 bytes because that is what
/// userspace copies out — a shorter struct leaves the caller's tail
/// uninitialised.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Siginfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub _pad: [i32; 29],
}

/// The `SIGCHLD` view of `siginfo_t`, as `waitid(2)` fills it.
///
/// A prefix of the 128-byte [`Siginfo`], not a separate struct in Linux — it is
/// declared separately here because `sys_waitid` writes only these fields, and
/// naming them is what keeps `si_status` at offset 24 where `waitid`'s caller
/// reads it.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SigChld {
    pub si_signo: u32,
    pub si_errno: u32,
    pub si_code: i32,
    pub __pad0: u32,
    pub si_pid: u32,
    pub si_uid: u32,
    pub si_status: i32,
}

/// The *kernel's* `struct sigaction` — the layout `rt_sigaction(2)` actually
/// takes, which is not libc's.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct KernelSigaction {
    pub sa_handler: usize,
    pub sa_flags: u64,
    pub sa_restorer: usize,
    pub sa_mask: u64,
}

/// `SIG_DFL` — a null handler pointer means "default action".
pub const SIG_DFL: usize = 0;
/// `SIG_IGN`.
pub const SIG_IGN: usize = 1;

/// `sigprocmask(2)` / `rt_sigprocmask(2)` `how` values.
pub mod sigmask_how {
    pub const SIG_BLOCK: u32 = 0;
    pub const SIG_UNBLOCK: u32 = 1;
    pub const SIG_SETMASK: u32 = 2;
}

/// `si_code` values for a `SIGCHLD` siginfo.
pub mod cld {
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
}

/// `SIGCHLD`, the signal number `waitid` reports in `si_signo`.
pub const SIGCHLD: u32 = 17;

/// `sizeof(siginfo_t)`.
///
/// Taken from the type, not restated as `128` — which is how
/// `src/syscall/proc.rs` spelled it (`SIGINFO_SIZE`) while
/// `src/syscall/signal.rs` spelled it as a literal in a `const _` assertion.
pub const SIGINFO_SIZE: usize = core::mem::size_of::<Siginfo>();

const _: () = assert!(core::mem::size_of::<StackT>() == 24, "stack_t is 24 bytes on aarch64");
const _: () = assert!(core::mem::offset_of!(StackT, size) == 16);
const _: () = assert!(core::mem::size_of::<Siginfo>() == 128, "siginfo_t is 128 bytes on aarch64");
const _: () = assert!(core::mem::size_of::<KernelSigaction>() == 32);
const _: () = assert!(core::mem::offset_of!(KernelSigaction, sa_flags) == 8);
const _: () = assert!(core::mem::offset_of!(KernelSigaction, sa_restorer) == 16);
const _: () = assert!(core::mem::offset_of!(KernelSigaction, sa_mask) == 24);
// `SigChld` must be a prefix of `Siginfo`: same first three words, then the
// `waitid`-specific tail. If these two ever disagree, `waitid` writes
// `si_status` where a portable caller looks for something else.
const _: () = assert!(core::mem::offset_of!(SigChld, si_pid) == 16);
const _: () = assert!(core::mem::offset_of!(SigChld, si_status) == 24);
const _: () = assert!(core::mem::size_of::<SigChld>() <= core::mem::size_of::<Siginfo>());

#[cfg(test)]
mod tests {
    use super::*;

    /// `sa_handler` first, then `sa_flags` — the kernel order, not libc's
    /// `{ handler, mask, flags, restorer }`. Getting this wrong makes every
    /// installed handler run with another handler's flags.
    #[test]
    fn kernel_sigaction_is_handler_flags_restorer_mask() {
        let a = KernelSigaction {
            sa_handler: 0x1111_1111_1111_1111,
            sa_flags: 0x2222_2222_2222_2222,
            sa_restorer: 0x3333_3333_3333_3333,
            sa_mask: 0x4444_4444_4444_4444,
        };
        let raw: [u8; 32] = unsafe { core::mem::transmute(a) };
        let word = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!(word(0), 0x1111_1111_1111_1111, "sa_handler");
        assert_eq!(word(1), 0x2222_2222_2222_2222, "sa_flags");
        assert_eq!(word(2), 0x3333_3333_3333_3333, "sa_restorer");
        assert_eq!(word(3), 0x4444_4444_4444_4444, "sa_mask");
    }

    /// The `_pad` in `stack_t`, demonstrated: `size` is the *third* 8-byte word,
    /// not the second-and-a-half.
    #[test]
    fn stack_t_size_is_the_third_word() {
        let s = StackT { sp: 1, flags: -1, _pad: 0, size: 0xDEAD_BEEF };
        let raw: [u8; 24] = unsafe { core::mem::transmute(s) };
        assert_eq!(u64::from_le_bytes(raw[16..24].try_into().unwrap()), 0xDEAD_BEEF);
    }

    /// `SigChld` overlays the front of `Siginfo`: the two must agree on
    /// `si_signo`/`si_errno`/`si_code`, because a caller that reads a `waitid`
    /// buffer as a plain `siginfo_t` reads them there.
    #[test]
    fn sigchld_overlays_the_siginfo_prefix() {
        assert_eq!(
            core::mem::offset_of!(SigChld, si_signo),
            core::mem::offset_of!(Siginfo, si_signo)
        );
        assert_eq!(
            core::mem::offset_of!(SigChld, si_errno),
            core::mem::offset_of!(Siginfo, si_errno)
        );
        assert_eq!(
            core::mem::offset_of!(SigChld, si_code),
            core::mem::offset_of!(Siginfo, si_code)
        );
    }

    #[test]
    fn siginfo_size_const_tracks_the_type() {
        assert_eq!(SIGINFO_SIZE, 128);
    }
}
