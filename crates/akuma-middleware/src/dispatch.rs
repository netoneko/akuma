//! Running one call through the table, and the process-wide registration the kernels' dispatchers use.

use crate::calls::{may_refuse_before, observe_only, Calls, Verdict, ENOSYS_RET};
use crate::ctx::KernelCtx;
use crate::exec::{ExecKind, ExecVerdict};
use crate::table::Middleware;
use crate::calls::Machine;
use akuma_params::Params;
use akuma_primitives::OnceCopy;

impl<P: Calls, H: Calls> Middleware<P, H> {
    /// The whole `kernel` side of one call: pre, the call (`inner`),
    /// and the handler where it answered `-ENOSYS`. Call with the kernel lock held.
    pub fn run_kernel(&self, nr: u64, args: [u64; 6], tid: u64, inner: impl FnOnce() -> u64) -> u64 {
        let machine = self.m();
        let wants = self.kernel_wants(nr);
        let call = (machine.decode)(nr);
        let watch = observe_only(call);
        let refusable = may_refuse_before(call);
        let ctx = KernelCtx { nr, call, args, tid, pid: (machine.pid)(), machine };

        let mut ret = None;
        if wants
            && let Some(h) = self.hooks.pre
            && let Some(which) = P::from_call(call, nr)
        {
            match h(&ctx, which) {
                Verdict::Pass => {}
                Verdict::PassAndForget => self.kp.clear(nr),
                Verdict::Return(v) if refusable => ret = Some(v),
                Verdict::Errno(e) if refusable => ret = Some((-i64::from(e)) as u64),
                Verdict::Return(_) | Verdict::Errno(_) => {}
            }
        }
        if let Some(v) = ret {
            v
        } else {
            let r = inner();
            if r == ENOSYS_RET
                && !watch
                && let Some(h) = self.hooks.handler
                && let Some(which) = H::from_call(call, nr)
            {
                h(&ctx, which).unwrap_or(r)
            } else {
                r
            }
        }
    }
}

/// What a kernel registers: the object-safe face of a [`Middleware`], whatever its state type.
pub trait Dispatch: Sync {
    /// See [`Middleware::arm`].
    fn arm_with(&self, machine: Machine);
    /// See [`Middleware::run_kernel`].
    fn run_kernel(&self, nr: u64, args: [u64; 6], tid: u64, inner: &mut dyn FnMut() -> u64) -> u64;
    /// See [`Middleware::exec_event`].
    fn exec_event(&self, kind: &ExecKind<'_>) -> ExecVerdict;
    /// See [`Hooks::exec_sidecar`](crate::Hooks::exec_sidecar).
    fn exec_sidecar(&self) -> Option<&'static str>;
    /// See [`Middleware::init_params`].
    fn init_with(&self, params: &Params<'static>);
}

impl<P: Calls, H: Calls> Dispatch for Middleware<P, H> {
    fn arm_with(&self, machine: Machine) {
        self.arm(machine);
    }
    fn run_kernel(&self, nr: u64, args: [u64; 6], tid: u64, inner: &mut dyn FnMut() -> u64) -> u64 {
        Self::run_kernel(self, nr, args, tid, inner)
    }
    fn exec_event(&self, kind: &ExecKind<'_>) -> ExecVerdict {
        Self::exec_event(self, kind)
    }
    fn exec_sidecar(&self) -> Option<&'static str> {
        self.hooks.exec_sidecar
    }
    fn init_with(&self, params: &Params<'static>) {
        self.init_params(params);
    }
}

static ACTIVE: OnceCopy<&'static dyn Dispatch> = OnceCopy::new();

/// Register the kernel's one middleware and arm it. Called once, from the machine's boot.
///
/// # Panics
/// If one is already registered: compose in your own function instead.
pub fn install(mw: &'static dyn Dispatch, machine: Machine) {
    assert!(ACTIVE.get().is_none(), "middleware: one table per kernel - compose in your own function");
    mw.arm_with(machine);
    ACTIVE.set(mw);
}

/// Hand the registered middleware its parameters: its [`Hooks::init`](crate::Hooks::init) runs, once, with the view under `mw.`. The machine calls
/// this after [`params::capture`](akuma_params::capture) and before it starts init. A no-op with no middleware.
pub fn init() {
    if let Some(mw) = active() {
        mw.init_with(&Params::middleware());
    }
}

/// The registered middleware, if any.
#[must_use]
pub fn active() -> Option<&'static dyn Dispatch> {
    ACTIVE.get()
}

/// The `kernel` side of a call, for a machine's dispatcher: with no middleware registered, `inner` alone.
pub fn run_kernel(nr: u64, args: [u64; 6], mut inner: impl FnMut() -> u64) -> u64 {
    match active() {
        Some(mw) => mw.run_kernel(nr, args, akuma_primitives::preempt::current_tid() as u64, &mut inner),
        None => inner(),
    }
}
