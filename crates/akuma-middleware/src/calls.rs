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

/// A closed set of calls one tier is about: the variants of an enum made by [`calls!`](crate::calls).
///
/// The enum is the allow-list. Only its calls reach the tier's hook, and the hook is an exhaustive `match` over it, so adding a
/// variant breaks the build at the hook until it is handled. The interest mask is built from [`ALL`](Calls::ALL).
pub trait Calls: Copy + 'static {
    /// What each variant is about, in declaration order.
    const ALL: &'static [On];
    /// The variants, in the same order as [`ALL`](Calls::ALL).
    const VARIANTS: &'static [Self];

    /// The variant that names this call, if any: `call` is the machine's decode of `nr`.
    #[must_use]
    fn from_call(call: Option<Syscall>, nr: u64) -> Option<Self> {
        let mut i = 0;
        while i < Self::ALL.len() {
            let hit = match Self::ALL[i] {
                On::Call(c) => call == Some(c),
                On::Nr(n) => n == nr,
            };
            if hit {
                return Some(Self::VARIANTS[i]);
            }
            i += 1;
        }
        None
    }
}

/// The empty set: a tier that has no hook. `Hooks<NoCalls, ..>` has no `pre`, and no runtime check says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::empty_enums)] // uninhabited on purpose: no variant can reach a hook
pub enum NoCalls {}

impl Calls for NoCalls {
    const ALL: &'static [On] = &[];
    const VARIANTS: &'static [Self] = &[];
}

const fn on_eq(a: On, b: On) -> bool {
    match (a, b) {
        (On::Call(x), On::Call(y)) => x as u32 == y as u32,
        (On::Nr(x), On::Nr(y)) => x == y,
        _ => false,
    }
}

/// Build-time rules for one enum's calls: no call twice, and a custom number is at or above [`CUSTOM_BASE`].
///
/// # Panics
/// At compile time, when evaluated in a `const`.
pub const fn check_calls(all: &[On]) {
    let mut i = 0;
    while i < all.len() {
        if let On::Nr(n) = all[i] {
            assert!(n >= CUSTOM_BASE, "middleware: a custom call number is below CUSTOM_BASE (it would shadow a Linux call)");
        }
        let mut j = i + 1;
        while j < all.len() {
            assert!(!on_eq(all[i], all[j]), "middleware: a call is named twice in one enum");
            j += 1;
        }
        i += 1;
    }
}

/// Whether a `handler` enum may hold these calls: none of them is [`observe_only`] (`exit`, `clone`, `execve`, ...), which a
/// handler can never serve.
#[must_use]
pub const fn servable(all: &[On]) -> bool {
    let mut i = 0;
    while i < all.len() {
        let observed = match all[i] {
            On::Call(c) => observe_only(Some(c)),
            On::Nr(_) => false,
        };
        if observed {
            return false;
        }
        i += 1;
    }
    true
}

/// One closed enum of calls for a tier.
///
/// A bare variant is the Linux call of that name (`Openat` is [`Syscall::Openat`]); `Name = 1001` is a custom call with that fixed
/// number, the same on every architecture. A variant may carry attributes (`#[cfg(..)]`). Evaluated at compile time: a call named
/// twice, or a custom number below [`CUSTOM_BASE`], is a build error.
///
/// ```ignore
/// calls! {
///     pub enum Serve { Hello = 1000, Whoami = 1001, Memfd }   // Memfd is `Syscall::Memfd`
/// }
/// ```
#[macro_export]
macro_rules! calls {
    ($(#[$em:meta])* $vis:vis enum $name:ident { $( $(#[$vm:meta])* $v:ident $(= $n:expr)? ),* $(,)? }) => {
        $(#[$em])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        $vis enum $name { $( $(#[$vm])* $v ),* }

        impl $crate::Calls for $name {
            const ALL: &'static [$crate::On] = &[ $( $(#[$vm])* $crate::__on!($v $(= $n)?) ),* ];
            const VARIANTS: &'static [Self] = &[ $( $(#[$vm])* $name::$v ),* ];
        }

        const _: () = $crate::check_calls(<$name as $crate::Calls>::ALL);
    };
}

/// One `calls!` variant as an [`On`].
#[doc(hidden)]
#[macro_export]
macro_rules! __on {
    ($v:ident) => { $crate::On::Call($crate::Syscall::$v) };
    ($v:ident = $n:expr) => { $crate::On::Nr($n) };
}
