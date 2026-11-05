//! Vocabulary of a call: which syscall an entry is about, what a `pre` hook may decide, what a machine supplies.

pub use akuma_syscalls_abi::Syscall;

/// `-ENOSYS` as a syscall return value.
pub const ENOSYS_RET: u64 = (-38i64) as u64;
/// Numbers below this have a bit in a mask; the rest are always asked.
pub const MASK_BITS: usize = 512;
/// The first number a kernel author should use for a syscall of their own: above every number Linux assigns.
pub const CUSTOM_BASE: u64 = 1000;

/// Which syscall a table entry is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    /// A Linux call, by identity; the machine says which number that is on its architecture.
    Call(Syscall),
    /// A raw number, the same on every architecture (a custom call, from [`CUSTOM_BASE`]).
    Nr(u64),
}

/// What a `pre` hook decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Carry on to the call.
    Pass,
    /// Carry on, and never ask this hook about this number again.
    PassAndForget,
    /// Answer with this value; the call does not run.
    Return(u64),
    /// Answer `-errno`; the call does not run.
    Errno(i32),
}

/// What a machine half supplies: its numbering and its checked user copies. The copies are the kernel's own
/// (the ones every syscall uses); the middleware reaches memory only through them.
#[derive(Clone, Copy)]
pub struct Machine {
    /// A native number to the call it names.
    pub decode: fn(u64) -> Option<Syscall>,
    /// A call to its native number.
    pub encode: fn(Syscall) -> u64,
    /// Copy from the caller's memory; `false` if a page does not translate.
    pub copy_in: fn(dst: &mut [u8], src_va: u64) -> bool,
    /// Copy to the caller's memory; `false` if a page does not translate.
    pub copy_out: fn(dst_va: u64, src: &[u8]) -> bool,
    /// The calling process's id.
    pub pid: fn() -> u32,
}

/// Calls a hook may watch but whose result it may not change, and which `handler` never serves.
#[must_use]
pub const fn observe_only(call: Option<Syscall>) -> bool {
    matches!(
        call,
        Some(Syscall::Exit | Syscall::ExitGroup | Syscall::Clone | Syscall::Execve | Syscall::RtSigreturn)
    )
}

/// Whether a `pre` refusal (`Return`/`Errno`) is honoured for this call.
///
/// Every call that is not [`observe_only`], and `execve`, which fails cleanly before it touches the process (a
/// signed-binaries or environment policy needs exactly that). `exit`, `exit_group`, `clone` and `rt_sigreturn`
/// stay unrefusable.
#[must_use]
pub const fn may_refuse_before(call: Option<Syscall>) -> bool {
    !observe_only(call) || matches!(call, Some(Syscall::Execve))
}

pub fn native(m: &Machine, on: On) -> u64 {
    match on {
        On::Call(c) => (m.encode)(c),
        On::Nr(n) => n,
    }
}
