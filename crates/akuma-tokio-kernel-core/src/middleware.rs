//! Syscall middleware: one optional table of hooks around the kernel's syscall dispatch.
//!
//! The guide is `docs/middleware.md` in this crate; the design record is `docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md`.
//! Off unless the `middleware` feature is on; a kernel that
//! does not ask for it compiles none of this and pays nothing.
//!
//! # The shape
//!
//! A kernel registers **one** [`Middleware`] (a `static`). It is a flat table, not a stack: every hook is a
//! plain `fn`, there is no `next`, no priority and no registration order, and two entries for the same call in
//! the same tier are refused when the table is armed. Someone who wants layers writes a function that calls
//! their own functions and registers that; the framework gives them nothing to make it feel supported.
//!
//! | tier | runs | sees | may |
//! |---|---|---|---|
//! | `kernel_pre`  | inside the big kernel lock, before the call | [`KernelCtx`]: args, caller's memory, tid, pid | `Pass`, `Return`, `Errno`, rewrite `args` |
//! | `fallback`    | inside the lock, only where the answer is `-ENOSYS` | [`KernelCtx`] | supply the answer (a *new* syscall) |
//! | `kernel_post` | inside the lock, after the call | [`KernelCtx`] + the result | replace the result |
//! | `own_pre` / `own_post` | **outside** the lock, around it | [`OwnCtx`]: plain integers + the middleware's private state | observe only |
//!
//! A hook never sees a lock. A `kernel` hook runs under the kernel's own lock, taken by the kernel. An `own` hook
//! is serialised by an *autolock* the framework owns (a spinlock with preemption off, held for exactly the hook
//! call, never while the big lock is taken). An `own` hook therefore must be short, must not block, allocate or
//! print, and gets **no user memory**: the page-table walk behind a user copy races a sibling thread's `munmap`
//! when the big lock is not held, which is why anything that reads the caller's memory is a `kernel` hook.
//!
//! # What it costs
//!
//! Which numbers a tier cares about is decided once, when the table is armed: a 512-bit mask per tier. A call
//! nobody hooked costs one relaxed load and a bit test per tier. A hook may return [`Verdict::PassAndForget`] to
//! clear its own bit for that number. The mask is read **once** per call and latched ([`Latch`]), so a pre-hook
//! that prunes itself cannot make its own post-hook vanish. Numbers from 512 up are always asked.
//!
//! # Not interceptable
//!
//! `exit`, `exit_group`, `clone`, `execve` and `rt_sigreturn` rewrite the frame or never return; a hook sees them
//! ([`observe_only`]) but a `Return`/`Errno` verdict or a changed result is ignored.
//!
//! # `#![forbid(unsafe_code)]`
//!
//! This module has none and the API needs none. Raw access means patching the attribute out of your own crate.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub use akuma_syscalls_abi::Syscall;
use akuma_primitives::{OnceCopy, PreemptGuard};
use spinning_top::Spinlock;

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

/// What a `kernel_pre` hook decided.
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

/// A user-memory access that did not go through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    /// The address (or a page of the range) does not translate.
    Fault,
    /// A string had no NUL inside the buffer.
    TooLong,
    /// A string was not UTF-8.
    NotUtf8,
}

impl UserError {
    /// The syscall result for it (`-EFAULT`, `-ENAMETOOLONG`, `-EINVAL`).
    #[must_use]
    pub const fn errno(self) -> i32 {
        match self {
            Self::Fault => 14,
            Self::TooLong => 36,
            Self::NotUtf8 => 22,
        }
    }
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

/// Calls a hook may watch but whose result it may not change.
#[must_use]
pub const fn observe_only(call: Option<Syscall>) -> bool {
    matches!(
        call,
        Some(Syscall::Exit | Syscall::ExitGroup | Syscall::Clone | Syscall::Execve | Syscall::RtSigreturn)
    )
}

/// The context of a `kernel` hook: runs with the kernel lock held.
pub struct KernelCtx {
    /// The native syscall number (after the machine's legacy folding, where it has one).
    pub nr: u64,
    /// The call it names, if it names one.
    pub call: Option<Syscall>,
    /// The six arguments. A `kernel_pre` hook may rewrite them; the call then sees the new values.
    pub args: [u64; 6],
    /// The calling thread.
    pub tid: u64,
    /// The calling process.
    pub pid: u32,
    machine: Machine,
}

impl KernelCtx {
    /// Fill `buf` from the caller's memory at `addr`.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_read(&self, addr: u64, buf: &mut [u8]) -> Result<(), UserError> {
        if (self.machine.copy_in)(buf, addr) { Ok(()) } else { Err(UserError::Fault) }
    }

    /// Write `data` to the caller's memory at `addr`.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_write(&self, addr: u64, data: &[u8]) -> Result<(), UserError> {
        if (self.machine.copy_out)(addr, data) { Ok(()) } else { Err(UserError::Fault) }
    }

    /// A little-endian `u64` from the caller's memory.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_read_u64(&self, addr: u64) -> Result<u64, UserError> {
        let mut b = [0u8; 8];
        self.user_read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Write a little-endian `u64` to the caller's memory.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_write_u64(&self, addr: u64, v: u64) -> Result<(), UserError> {
        self.user_write(addr, &v.to_le_bytes())
    }

    /// A NUL-terminated UTF-8 string from the caller's memory, into `buf` (bytewise, so a string that ends
    /// just short of an unmapped page does not fault).
    ///
    /// # Errors
    /// [`UserError::Fault`], [`UserError::TooLong`] (no NUL in `buf`), [`UserError::NotUtf8`].
    pub fn user_str<'b>(&self, addr: u64, buf: &'b mut [u8]) -> Result<&'b str, UserError> {
        let mut n = 0;
        while n < buf.len() {
            let mut b = [0u8; 1];
            self.user_read(addr + n as u64, &mut b)?;
            if b[0] == 0 {
                return core::str::from_utf8(&buf[..n]).map_err(|_| UserError::NotUtf8);
            }
            buf[n] = b[0];
            n += 1;
        }
        Err(UserError::TooLong)
    }
}

/// The context of an `own` hook: runs outside the kernel lock. Plain integers, and the middleware's state.
pub struct OwnCtx<'a, S> {
    /// The native syscall number as the machine received it (before any legacy folding).
    pub nr: u64,
    /// The call it names, if it names one.
    pub call: Option<Syscall>,
    /// The six arguments, as integers. They are not pointers you may follow: there is no user memory here.
    pub args: [u64; 6],
    /// The calling thread.
    pub tid: u64,
    /// The middleware's private state, locked for the duration of the hook.
    pub state: &'a mut S,
}

/// `kernel_pre`.
pub type KernelPre = fn(&mut KernelCtx) -> Verdict;
/// `kernel_post`: the result in, the result out.
pub type KernelPost = fn(&mut KernelCtx, u64) -> u64;
/// `fallback`: `Some(answer)` to serve the call, `None` to leave the `-ENOSYS`.
pub type Fallback = fn(&mut KernelCtx) -> Option<u64>;
/// `own_pre`.
pub type OwnPre<S> = fn(&mut OwnCtx<'_, S>);
/// `own_post`: the result.
pub type OwnPost<S> = fn(&mut OwnCtx<'_, S>, u64);

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
struct Interest([AtomicU64; MASK_BITS / 64]);

impl Interest {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; MASK_BITS / 64])
    }
    fn set(&self, nr: u64) {
        if (nr as usize) < MASK_BITS {
            self.0[(nr >> 6) as usize].fetch_or(1 << (nr & 63), Relaxed);
        }
    }
    fn clear(&self, nr: u64) {
        if (nr as usize) < MASK_BITS {
            self.0[(nr >> 6) as usize].fetch_and(!(1 << (nr & 63)), Relaxed);
        }
    }
    fn test(&self, nr: u64) -> bool {
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
struct OwnLock<S>(Spinlock<S>);

impl<S> OwnLock<S> {
    const fn new(s: S) -> Self {
        Self(Spinlock::new(s))
    }
    fn with<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
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
}

impl<S> Hooks<S> {
    /// No hooks.
    #[must_use]
    pub const fn new() -> Self {
        Self { kernel_pre: &[], kernel_post: &[], fallback: &[], own_pre: &[], own_post: &[] }
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
    hooks: Hooks<S>,
    state: OwnLock<S>,
    machine: OnceCopy<Machine>,
    kp: Interest,
    kq: Interest,
    op: Interest,
    oq: Interest,
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

    fn m(&self) -> Machine {
        self.machine.get().expect("middleware: used before it was armed")
    }

    fn find<F: Copy>(&self, routes: &[Route<F>], nr: u64) -> Option<F> {
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

    /// The whole `kernel` side of one call: pre, the call (`inner`, given the possibly rewritten arguments),
    /// the fallback where it answered `-ENOSYS`, then post. Call with the kernel lock held.
    pub fn run_kernel(&self, nr: u64, args: &mut [u64; 6], tid: u64, inner: impl FnOnce(&[u64; 6]) -> u64) -> u64 {
        let machine = self.m();
        let latch = self.kernel_latch(nr);
        let call = (machine.decode)(nr);
        let watch = observe_only(call);
        let mut ctx = KernelCtx { nr, call, args: *args, tid, pid: (machine.pid)(), machine };

        let mut ret = None;
        if latch.pre
            && let Some(h) = self.find(self.hooks.kernel_pre, nr)
        {
            match h(&mut ctx) {
                Verdict::Pass => {}
                Verdict::PassAndForget => self.kp.clear(nr),
                Verdict::Return(v) if !watch => ret = Some(v),
                Verdict::Errno(e) if !watch => ret = Some((-i64::from(e)) as u64),
                Verdict::Return(_) | Verdict::Errno(_) => {}
            }
        }
        *args = ctx.args;
        let mut r = if let Some(v) = ret {
            v
        } else {
            let r = inner(args);
            if r == ENOSYS_RET
                && !watch
                && let Some(h) = self.find(self.hooks.fallback, nr)
            {
                h(&mut ctx).unwrap_or(r)
            } else {
                r
            }
        };
        if latch.post
            && let Some(h) = self.find(self.hooks.kernel_post, nr)
        {
            let new = h(&mut ctx, r);
            if !watch {
                r = new;
            }
        }
        r
    }
}

fn native(m: &Machine, on: On) -> u64 {
    match on {
        On::Call(c) => (m.encode)(c),
        On::Nr(n) => n,
    }
}

/// What a kernel registers: the object-safe face of a [`Middleware`], whatever its state type.
pub trait Dispatch: Sync {
    /// See [`Middleware::arm`].
    fn arm_with(&self, machine: Machine);
    /// See [`Middleware::kernel_latch`] and friends, behind one call each.
    fn run_kernel(&self, nr: u64, args: &mut [u64; 6], tid: u64, inner: &mut dyn FnMut(&[u64; 6]) -> u64) -> u64;
    fn own_latch(&self, nr: u64) -> Latch;
    fn run_own_pre(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64);
    fn run_own_post(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64, ret: u64);
}

impl<S: Send + 'static> Dispatch for Middleware<S> {
    fn arm_with(&self, machine: Machine) {
        self.arm(machine);
    }
    fn run_kernel(&self, nr: u64, args: &mut [u64; 6], tid: u64, inner: &mut dyn FnMut(&[u64; 6]) -> u64) -> u64 {
        Self::run_kernel(self, nr, args, tid, inner)
    }
    fn own_latch(&self, nr: u64) -> Latch {
        Self::own_latch(self, nr)
    }
    fn run_own_pre(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64) {
        Self::run_own_pre(self, latch, nr, args, tid);
    }
    fn run_own_post(&self, latch: Latch, nr: u64, args: [u64; 6], tid: u64, ret: u64) {
        Self::run_own_post(self, latch, nr, args, tid, ret);
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

/// The registered middleware, if any.
#[must_use]
pub fn active() -> Option<&'static dyn Dispatch> {
    ACTIVE.get()
}

/// The `kernel` side of a call, for a machine's dispatcher: with no middleware registered, `inner` alone.
pub fn run_kernel(nr: u64, args: &mut [u64; 6], mut inner: impl FnMut(&[u64; 6]) -> u64) -> u64 {
    match active() {
        Some(mw) => mw.run_kernel(nr, args, akuma_primitives::preempt::current_tid() as u64, &mut inner),
        None => inner(args),
    }
}

/// The `own` side of a call before the kernel lock: `Some` when there is something to run after it.
#[must_use]
pub fn own_enter(nr: u64, args: [u64; 6]) -> Option<(&'static dyn Dispatch, Latch)> {
    let mw = active()?;
    let latch = mw.own_latch(nr);
    if !latch.pre && !latch.post {
        return None;
    }
    mw.run_own_pre(latch, nr, args, akuma_primitives::preempt::current_tid() as u64);
    Some((mw, latch))
}

/// The `own` side after the kernel lock is released.
pub fn own_leave(entered: Option<(&'static dyn Dispatch, Latch)>, nr: u64, args: [u64; 6], ret: u64) {
    if let Some((mw, latch)) = entered {
        mw.run_own_post(latch, nr, args, akuma_primitives::preempt::current_tid() as u64, ret);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    fn machine() -> Machine {
        Machine {
            decode: Syscall::from_x86_64,
            encode: Syscall::to_x86_64,
            copy_in: |dst, src| {
                // A fake address space: address N holds byte N (low 8 bits); above 0x10000 nothing translates.
                if src + dst.len() as u64 > 0x10000 {
                    return false;
                }
                for (i, b) in dst.iter_mut().enumerate() {
                    *b = (src + i as u64) as u8;
                }
                true
            },
            copy_out: |dst, src| dst + src.len() as u64 <= 0x10000,
            pid: || 7,
        }
    }

    const GETPID: u64 = 39;
    const OPENAT: u64 = 257;
    const CUSTOM: u64 = CUSTOM_BASE;

    #[test]
    fn a_call_nobody_hooked_runs_no_hook() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks::new());
        MW.arm(machine());
        let mut a = [1, 2, 3, 4, 5, 6];
        assert_eq!(MW.kernel_latch(GETPID), Latch::default());
        assert_eq!(MW.own_latch(GETPID), Latch::default());
        assert_eq!(MW.run_kernel(GETPID, &mut a, 1, |_| 99), 99);
    }

    #[test]
    fn pre_can_answer_and_the_call_does_not_run() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::Errno(1))],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        let ran = AtomicUsize::new(0);
        let r = MW.run_kernel(GETPID, &mut a, 1, |_| {
            ran.fetch_add(1, Relaxed);
            5
        });
        assert_eq!(r.cast_signed(), -1);
        assert_eq!(ran.load(Relaxed), 0);
    }

    #[test]
    fn pre_rewrites_arguments_and_post_replaces_the_result() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[Route::new(&[On::Call(Syscall::Openat)], |c| {
                c.args[1] = 0xAA;
                Verdict::Pass
            })],
            kernel_post: &[Route::new(&[On::Call(Syscall::Openat)], |_, r| r + 1)],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        let r = MW.run_kernel(OPENAT, &mut a, 1, |a| a[1]);
        assert_eq!(r, 0xAB);
        assert_eq!(a[1], 0xAA);
    }

    #[test]
    fn fallback_serves_only_what_would_be_enosys() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            fallback: &[Route::new(&[On::Nr(CUSTOM), On::Call(Syscall::Getpid)], |_| Some(42))],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        assert_eq!(MW.run_kernel(CUSTOM, &mut a, 1, |_| ENOSYS_RET), 42);
        // getpid is served, so its answer stands.
        assert_eq!(MW.run_kernel(GETPID, &mut a, 1, |_| 1234), 1234);
    }

    #[test]
    fn exit_and_clone_are_observed_not_replaced() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[Route::new(&[On::Call(Syscall::Clone)], |_| Verdict::Return(0))],
            kernel_post: &[Route::new(&[On::Call(Syscall::Clone)], |_, _| 0)],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        assert_eq!(MW.run_kernel(56, &mut a, 1, |_| 77), 77);
    }

    #[test]
    fn pass_and_forget_clears_the_bit_but_the_latch_keeps_the_post_hook() {
        static POSTS: AtomicUsize = AtomicUsize::new(0);
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::PassAndForget)],
            kernel_post: &[Route::new(&[On::Call(Syscall::Getpid)], |_, r| {
                POSTS.fetch_add(1, Relaxed);
                r
            })],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        assert!(MW.kernel_latch(GETPID).pre);
        MW.run_kernel(GETPID, &mut a, 1, |_| 1);
        // The next call is not asked, but the post bit is its own.
        assert!(!MW.kernel_latch(GETPID).pre);
        assert!(MW.kernel_latch(GETPID).post);
        MW.run_kernel(GETPID, &mut a, 1, |_| 1);
        assert_eq!(POSTS.load(Relaxed), 2);
    }

    #[test]
    fn own_hooks_keep_private_state_and_see_integers() {
        static MW: Middleware<u64> = Middleware::new(0, Hooks {
            own_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |c| *c.state += 1)],
            own_post: &[Route::new(&[On::Call(Syscall::Getpid)], |c, r| *c.state += r)],
            ..Hooks::new()
        });
        MW.arm(machine());
        let l = MW.own_latch(GETPID);
        assert_eq!(l, Latch { pre: true, post: true });
        MW.run_own_pre(l, GETPID, [0; 6], 1);
        MW.run_own_post(l, GETPID, [0; 6], 1, 10);
        MW.state.with(|s| assert_eq!(*s, 11));
        assert_eq!(MW.own_latch(OPENAT), Latch::default());
    }

    #[test]
    fn user_memory_is_checked_and_strings_are_bounded() {
        let m = machine();
        let ctx = KernelCtx { nr: 0, call: None, args: [0; 6], tid: 0, pid: 0, machine: m };
        assert_eq!(ctx.user_read_u64(0x100).unwrap(), u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]));
        assert_eq!(ctx.user_read_u64(0x20000), Err(UserError::Fault));
        assert_eq!(ctx.user_write(0x20000, &[1]), Err(UserError::Fault));
        let mut buf = [0u8; 4];
        // Address 0x41 holds 'A','B','C',... never a NUL inside four bytes.
        assert_eq!(ctx.user_str(0x41, &mut buf), Err(UserError::TooLong));
        // Address 0 is a NUL at once: the empty string.
        assert_eq!(ctx.user_str(0, &mut buf), Ok(""));
        assert_eq!(UserError::Fault.errno(), 14);
    }

    #[test]
    #[should_panic(expected = "named twice")]
    fn two_entries_for_one_call_in_one_tier_are_refused() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[
                Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::Pass),
                Route::new(&[On::Nr(GETPID)], |_| Verdict::Pass),
            ],
            ..Hooks::new()
        });
        MW.arm(machine());
    }

    #[test]
    fn the_same_call_in_two_tiers_is_fine() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks {
            kernel_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::Pass)],
            kernel_post: &[Route::new(&[On::Call(Syscall::Getpid)], |_, r| r)],
            ..Hooks::new()
        });
        MW.arm(machine());
    }

    /// `docs/middleware.md` § "Your first syscall": the table as written there, with the stateless `()`.
    #[test]
    fn the_guides_first_syscall_compiles_and_answers() {
        static MW: Middleware<()> = Middleware::new((), Hooks {
            fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_ctx| Some(42))],
            ..Hooks::new()
        });
        MW.arm(machine());
        let mut a = [0; 6];
        assert_eq!(MW.run_kernel(CUSTOM_BASE, &mut a, 1, |_| ENOSYS_RET), 42);
        // And as `Kernel::middleware` takes it: behind the object-safe face.
        let face: &'static dyn Dispatch = &MW;
        assert_eq!(face.own_latch(39), Latch::default());
    }

    #[test]
    fn numbers_from_512_are_always_asked() {
        static MW: Middleware<u32> = Middleware::new(0, Hooks::new());
        MW.arm(machine());
        assert!(MW.kernel_latch(CUSTOM).pre);
        assert!(!MW.kernel_latch(GETPID).pre);
    }
}
