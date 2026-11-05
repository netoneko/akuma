//! The table: routes, the interest mask, [`Hooks`] and [`Middleware`] (arming, the exec policy).

use super::calls::{native, Machine, On, MASK_BITS};
use super::ctx::{Fallback, KernelPre};
use super::exec::{ExecEvent, ExecKind, ExecPolicy, ExecVerdict};
use crate::params::Params;
use akuma_primitives::OnceCopy;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// One table entry: the calls it is about, and the hook.
pub struct Route<F: 'static> {
    pub on: &'static [On],
    pub hook: F,
}

impl<F> Route<F> {
    /// An entry for `on`.
    pub const fn new(on: &'static [On], hook: F) -> Self {
        Self { on, hook }
    }
}

/// 512 bits, one per native syscall number, set when the tier wants to be asked.
pub(super) struct Interest([AtomicU64; MASK_BITS / 64]);

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

/// The hook tables: all public, so a table is written `Hooks { fallback: &[..], ..Hooks::new() }` from any crate.
pub struct Hooks {
    pub kernel_pre: &'static [Route<KernelPre>],
    pub fallback: &'static [Route<Fallback>],
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

impl Hooks {
    /// No hooks.
    #[must_use]
    pub const fn new() -> Self {
        Self { kernel_pre: &[], fallback: &[], exec: None, init: None, exec_sidecar: None }
    }
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new()
    }
}

/// The table. Make it a `static`.
///
/// ```ignore
/// static MW: Middleware = Middleware::new(Hooks {
///     fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_| Some(42))],
///     ..Hooks::new()
/// });
/// ```
pub struct Middleware {
    pub(super) hooks: Hooks,
    pub(super) machine: OnceCopy<Machine>,
    pub(super) kp: Interest,
}

impl Middleware {
    /// A table of `hooks`.
    #[must_use]
    pub const fn new(hooks: Hooks) -> Self {
        Self {
            hooks,
            machine: OnceCopy::new(),
            kp: Interest::new(),
        }
    }

    /// Decide, once, which numbers `kernel_pre` wants, and refuse a table that names a number twice in one tier.
    ///
    /// # Panics
    /// On a duplicate, with the number: overlap is an error, not an ordering question.
    pub fn arm(&self, machine: Machine) {
        fn fill<F: 'static>(routes: &[Route<F>], mask: Option<&Interest>, m: &Machine, tier: &str) {
            for (i, r) in routes.iter().enumerate() {
                for (a, on) in r.on.iter().enumerate() {
                    let nr = native(m, *on);
                    let again = r.on[a + 1..].iter().any(|o| native(m, *o) == nr)
                        || routes[i + 1..].iter().any(|q| q.on.iter().any(|o| native(m, *o) == nr));
                    assert!(!again, "middleware: syscall {nr} is named twice in `{tier}`");
                    if let Some(mask) = mask {
                        mask.set(nr);
                    }
                }
            }
        }
        self.machine.set(machine);
        fill(self.hooks.kernel_pre, Some(&self.kp), &machine, "kernel_pre");
        fill(self.hooks.fallback, None, &machine, "fallback");
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

    pub(super) fn find<F: Copy>(&self, routes: &[Route<F>], nr: u64) -> Option<F> {
        let m = self.m();
        routes.iter().find(|r| r.on.iter().any(|o| native(&m, *o) == nr)).map(|r| r.hook)
    }

    /// Whether `kernel_pre` wants to be asked about `nr`: read once per call.
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
