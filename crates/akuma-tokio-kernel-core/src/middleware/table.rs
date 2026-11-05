//! The table: routes, the per-tier interest masks, the autolock, [`Hooks`] and [`Middleware`] (arming, latches, `own` hooks).

use super::calls::{native, Machine, On, MASK_BITS};
use super::ctx::{Fallback, KernelPost, KernelPre, OwnCtx, OwnPost, OwnPre};
use super::exec::{ExecEvent, ExecKind, ExecPolicy, ExecVerdict};
use crate::params::Params;
use akuma_primitives::{OnceCopy, PreemptGuard};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use spinning_top::Spinlock;

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

/// Which hooks a call will run, read once at entry and used again at exit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Latch {
    pub pre: bool,
    pub post: bool,
}

/// The autolock around `own` state: a spinlock held with preemption off.
pub(super) struct OwnLock<S>(Spinlock<S>);

impl<S> OwnLock<S> {
    const fn new(s: S) -> Self {
        Self(Spinlock::new(s))
    }
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let _preempt = PreemptGuard::new();
        let mut g = self.0.lock();
        f(&mut g)
    }
}

/// The hook tables: all public, so a table is written `Hooks { fallback: &[..], ..Hooks::new() }` from any crate.
pub struct Hooks<S: 'static> {
    pub kernel_pre: &'static [Route<KernelPre>],
    pub kernel_post: &'static [Route<KernelPost>],
    pub fallback: &'static [Route<Fallback>],
    pub own_pre: &'static [Route<OwnPre<S>>],
    pub own_post: &'static [Route<OwnPost<S>>],
    /// The policy for new code (images, executable mappings): see [`ExecPolicy`]. `None`: everything allowed.
    pub exec: Option<ExecPolicy>,
    /// Runs once, at boot, after the command line is captured and before the first user instruction: set up `state` from the
    /// parameters. The view is already narrowed to `mw.` ([`Params::middleware`]); narrow it again per crate with
    /// [`Params::scoped`]. Single-threaded, so it takes `&mut S` without the autolock. No kernel context: there is no process yet.
    ///
    /// If the line could not be captured whole ([`params::failed`]) the view is empty; a policy that needs a value must read
    /// that as "refuse", not as "unset".
    pub init: Option<InitHook<S>>,
    /// A suffix (`".sig"`): for an `Image` event the loader also reads `<path><suffix>` (at most 4 KiB) and passes its bytes as
    /// `sidecar`. `None`: no extra read, and `sidecar` is `None`. The policy stays a pure function of the event: it never touches
    /// the filesystem, and a swapped sidecar cannot make other bytes verify, because it is checked against the `image` it was shown.
    pub exec_sidecar: Option<&'static str>,
}

/// See [`Hooks::init`].
pub type InitHook<S> = fn(&Params<'static>, &mut S);

impl<S> Hooks<S> {
    /// No hooks.
    #[must_use]
    pub const fn new() -> Self {
        Self { kernel_pre: &[], kernel_post: &[], fallback: &[], own_pre: &[], own_post: &[], exec: None, init: None, exec_sidecar: None }
    }
}

impl<S> Default for Hooks<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// The table plus its private `own` state. Make it a `static`.
///
/// ```ignore
/// static MW: Middleware<u64> = Middleware::new(0, Hooks {
///     fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_| Some(42))],
///     ..Hooks::new()
/// });
/// ```
pub struct Middleware<S: Send + 'static> {
    pub(super) hooks: Hooks<S>,
    pub(super) state: OwnLock<S>,
    pub(super) machine: OnceCopy<Machine>,
    pub(super) kp: Interest,
    pub(super) kq: Interest,
    pub(super) op: Interest,
    pub(super) oq: Interest,
}

impl<S: Send + 'static> Middleware<S> {
    /// A table of `hooks`, with `state` as the `own` hooks' private state.
    pub const fn new(state: S, hooks: Hooks<S>) -> Self {
        Self {
            hooks,
            state: OwnLock::new(state),
            machine: OnceCopy::new(),
            kp: Interest::new(),
            kq: Interest::new(),
            op: Interest::new(),
            oq: Interest::new(),
        }
    }

    /// Decide, once, which numbers each tier wants, and refuse a table that names a number twice in one tier.
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
        fill(self.hooks.kernel_post, Some(&self.kq), &machine, "kernel_post");
        fill(self.hooks.fallback, None, &machine, "fallback");
        fill(self.hooks.own_pre, Some(&self.op), &machine, "own_pre");
        fill(self.hooks.own_post, Some(&self.oq), &machine, "own_post");
    }

    /// Run the [`Hooks::init`] hook, if there is one, with `params`. Called once, by [`init`], after the command line is captured.
    pub fn init_params(&self, params: &Params<'static>) {
        if let Some(h) = self.hooks.init {
            h(params, &mut self.state.0.lock());
        }
    }

    pub(super) fn m(&self) -> Machine {
        self.machine.get().expect("middleware: used before it was armed")
    }

    pub(super) fn find<F: Copy>(&self, routes: &[Route<F>], nr: u64) -> Option<F> {
        let m = self.m();
        routes.iter().find(|r| r.on.iter().any(|o| native(&m, *o) == nr)).map(|r| r.hook)
    }

    /// The `kernel_pre`/`kernel_post` decisions for `nr`, read once.
    #[must_use]
    pub fn kernel_latch(&self, nr: u64) -> Latch {
        Latch { pre: self.kp.test(nr), post: self.kq.test(nr) }
    }

    /// The `own_pre`/`own_post` decisions for `nr`, read once.
    #[must_use]
    pub fn own_latch(&self, nr: u64) -> Latch {
        Latch { pre: self.op.test(nr), post: self.oq.test(nr) }
    }

    /// Run the `own_pre` hook for `nr`, if the latch says so.
    pub fn run_own_pre(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64) {
        if !latch.pre {
            return;
        }
        if let Some(h) = self.find(self.hooks.own_pre, nr) {
            let call = (self.m().decode)(nr);
            self.state.with(|state| h(&mut OwnCtx { nr, call, args, tid, state }));
        }
    }

    /// Run the `own_post` hook for `nr`, if the latch says so.
    pub fn run_own_post(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64, ret: u64) {
        if !latch.post {
            return;
        }
        if let Some(h) = self.find(self.hooks.own_post, nr) {
            let call = (self.m().decode)(nr);
            self.state.with(|state| h(&mut OwnCtx { nr, call, args, tid, state }, ret));
        }
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
