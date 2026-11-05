//! The table: routes, the interest mask, [`Hooks`] and [`Middleware`] (arming, the exec policy).

use crate::calls::{check_calls, native, servable, Calls, Machine, NoCalls, Verdict, MASK_BITS};
use crate::ctx::KernelCtx;
use crate::exec::{ExecEvent, ExecKind, ExecPolicy, ExecVerdict};
use akuma_params::Params;
use akuma_primitives::OnceCopy;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// 512 bits, one per native syscall number, set when the tier wants to be asked.
pub struct Interest([AtomicU64; MASK_BITS / 64]);

impl Interest {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; MASK_BITS / 64])
    }
    fn set(&self, nr: u64) {
        if (nr as usize) < MASK_BITS {
            self.0[(nr >> 6) as usize].fetch_or(1 << (nr & 63), Relaxed);
        }
    }
    pub(super) fn clear(&self, nr: u64) {
        if (nr as usize) < MASK_BITS {
            self.0[(nr >> 6) as usize].fetch_and(!(1 << (nr & 63)), Relaxed);
        }
    }
    pub(super) fn test(&self, nr: u64) -> bool {
        (nr as usize) >= MASK_BITS || self.0[(nr >> 6) as usize].load(Relaxed) & (1 << (nr & 63)) != 0
    }
}

/// The hooks: all public, so a table is written `Hooks { pre: Some(guard), ..Hooks::new() }` from any crate.
///
/// `P` is the set of calls `pre` is asked about and `H` the set `handler` can serve ([`calls!`](crate::calls)); each hook is an
/// exhaustive `match` over its set. A tier with no hook is [`NoCalls`] and `None`.
pub struct Hooks<P: Calls = NoCalls, H: Calls = NoCalls> {
    /// Before the call, under the kernel lock: see the call, then pass, refuse or answer it.
    pub pre: Option<fn(&KernelCtx, P) -> Verdict>,
    /// After the kernel answered `-ENOSYS`: `Some(answer)` serves the call, `None` leaves the `-ENOSYS`.
    pub handler: Option<fn(&KernelCtx, H) -> Option<u64>>,
    /// The policy for new code (images, executable mappings): see [`ExecPolicy`]. `None`: everything allowed.
    pub exec: Option<ExecPolicy>,
    /// Runs once, at boot, after the command line is captured and before the first user instruction: read the
    /// parameters into the module's own statics. The view is already narrowed to `mw.` ([`Params::middleware`]); narrow it again per crate with
    /// [`Params::scoped`]. Single-threaded. No kernel context: there is no process yet.
    ///
    /// If the line could not be captured whole ([`params::failed`]) the view is empty; a policy that needs a value must read
    /// that as "refuse", not as "unset".
    pub init: Option<InitHook>,
    /// A suffix (`".sig"`): for an `Image` event the loader also reads `<path><suffix>` (at most 4 KiB) and passes its bytes as
    /// `sidecar`. `None`: no extra read, and `sidecar` is `None`. The policy stays a pure function of the event: it never touches
    /// the filesystem, and a swapped sidecar cannot make other bytes verify, because it is checked against the `image` it was shown.
    pub exec_sidecar: Option<&'static str>,
}

/// See [`Hooks::init`].
pub type InitHook = fn(&Params<'static>);

impl<P: Calls, H: Calls> Hooks<P, H> {
    /// No hooks.
    #[must_use]
    pub const fn new() -> Self {
        Self { pre: None, handler: None, exec: None, init: None, exec_sidecar: None }
    }
}

impl<P: Calls, H: Calls> Default for Hooks<P, H> {
    fn default() -> Self {
        Self::new()
    }
}

/// The table. Make it a `static`.
///
/// ```ignore
/// calls! { enum Serve { Hello = 1000 } }
/// static MW: Middleware<NoCalls, Serve> = Middleware::new(Hooks {
///     handler: Some(|_ctx, call| match call { Serve::Hello => Some(42) }),
///     ..Hooks::new()
/// });
/// ```
pub struct Middleware<P: Calls = NoCalls, H: Calls = NoCalls> {
    pub(super) hooks: Hooks<P, H>,
    pub(super) machine: OnceCopy<Machine>,
    pub(super) kp: Interest,
}

impl<P: Calls, H: Calls> Middleware<P, H> {
    /// A table of `hooks`. The rules on `P` and `H` are checked here, at compile time: no call twice, custom numbers from
    /// `CUSTOM_BASE`, and nothing in `H` that a handler can never serve (`exit`, `clone`, `execve`, ...).
    #[must_use]
    pub const fn new(hooks: Hooks<P, H>) -> Self {
        const { check_calls(P::ALL) };
        const { check_calls(H::ALL) };
        const { assert!(servable(H::ALL), "middleware: `handler` names a call it can never serve (exit, exit_group, clone, execve, rt_sigreturn)") };
        Self {
            hooks,
            machine: OnceCopy::new(),
            kp: Interest::new(),
        }
    }

    /// Decide, once, which numbers `pre` wants, and refuse a table that names a number twice in one tier.
    ///
    /// # Panics
    /// On a duplicate, with the number: overlap is an error, not an ordering question.
    pub fn arm(&self, machine: Machine) {
        fn fill(all: &[crate::calls::On], mask: Option<&Interest>, m: &Machine, tier: &str) {
            for (i, on) in all.iter().enumerate() {
                let nr = native(m, *on);
                // The one thing the types cannot see: a Linux call and a raw number that are the same call on this machine.
                assert!(!all[i + 1..].iter().any(|o| native(m, *o) == nr), "middleware: syscall {nr} is named twice in `{tier}`");
                if let Some(mask) = mask {
                    mask.set(nr);
                }
            }
        }
        self.machine.set(machine);
        fill(P::ALL, Some(&self.kp), &machine, "pre");
        fill(H::ALL, None, &machine, "handler");
    }

    /// Run the [`Hooks::init`] hook, if there is one, with `params`. Called once, by [`init`], after the command line is captured.
    pub fn init_params(&self, params: &Params<'static>) {
        if let Some(h) = self.hooks.init {
            h(params);
        }
    }

    pub(super) fn m(&self) -> Machine {
        self.machine.get().expect("middleware: used before it was armed")
    }

    /// Whether `pre` wants to be asked about `nr`: read once per call.
    #[must_use]
    pub fn kernel_wants(&self, nr: u64) -> bool {
        self.kp.test(nr)
    }

    /// Ask the table's [`ExecPolicy`] about `kind`. No policy: [`ExecVerdict::Allow`].
    #[must_use]
    pub fn exec_event(&self, kind: &ExecKind<'_>) -> ExecVerdict {
        match self.hooks.exec {
            Some(h) => h(&ExecEvent { pid: (self.m().pid)(), kind: *kind }),
            None => ExecVerdict::Allow,
        }
    }
}
