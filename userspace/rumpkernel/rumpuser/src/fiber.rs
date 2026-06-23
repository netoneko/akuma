//! Cooperative (fiber) threading backend for the Akuma rumpuser layer.
//!
//! This is a Rust port of NetBSD's `src-netbsd/lib/librumpuser/rumpfiber.c`
//! (Copyright (c) 2007-2013 Antti Kantee; (c) 2014 Justin Cormack; itself based
//! on Xen MiniOS `sched.c`, (c) 2005 Grzegorz Milos / Intel Research Cambridge).
//! The structure, control flow, and scheduling discipline follow rumpfiber.c
//! closely on purpose; names are kept aligned for reviewability.
//!
//! Difference from upstream: upstream switches fibers with libc `ucontext`
//! (`getcontext`/`makecontext`/`swapcontext`), which musl does NOT implement
//! (header-only — see docs/HIJACK_VS_KERNEL_PROXY.md). We substitute an inline
//! aarch64 context switch (`akfiber_switch`/`akfiber_tramp`) adapted from Akuma's
//! own EL1 `switch_context` (crates/akuma-exec/src/threading/mod.rs), reduced to
//! the EL0 cooperative callee-saved set: x19-x30, sp, d8-d15, tpidr_el0 — a pure
//! register/stack swap, no syscall, no signal-mask touch.
//!
//! Single OS thread, cooperative, non-preemptive: this is the whole point — it
//! collapses rump's ~19 pthread kthreads onto one thread and removes the
//! single-vCPU thundering-herd contention.

use core::ffi::{c_char, c_int, c_long, c_void};
use core::ptr;

// ── libc externs we need (resolved by the musl the final program links) ──
extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn clock_gettime(clk: c_int, ts: *mut Timespec) -> c_int;
    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn abort() -> !;
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: c_long,
}

const CLOCK_REALTIME: c_int = 0;
const CLOCK_MONOTONIC: c_int = 1;
const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const MAP_PRIVATE: c_int = 0x2;
const MAP_ANON: c_int = 0x20;
const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

const EBUSY: c_int = 16;
const EINVAL: c_int = 22;
const ETIMEDOUT: c_int = 110; // Linux/musl; rump only checks nonzero

const STACKSIZE: usize = 65536;

// rump mutex / rwlock / lwp-op flags (must match rump's rumpuser.h)
const RUMPUSER_MTX_SPIN: c_int = 0x01;
const RUMPUSER_MTX_KMUTEX: c_int = 0x02;
const RUMPUSER_RW_WRITER: c_int = 1;
const RUMPUSER_LWP_CREATE: c_int = 0;
const RUMPUSER_LWP_DESTROY: c_int = 1;
const RUMPUSER_LWP_SET: c_int = 2;
const RUMPUSER_LWP_CLEAR: c_int = 3;
const RUMPUSER_CLOCK_ABSMONO: c_int = 1;

// thread flags
const RUNNABLE_FLAG: c_int = 0x01;
const THREAD_MUSTJOIN: c_int = 0x02;
const THREAD_JOINED: c_int = 0x04;
const THREAD_TIMEDOUT: c_int = 0x10;

// ── aarch64 cooperative context switch ───────────────────────────────────────
//
// Context slots: x19..x30 @0..95, sp @96, d8..d15 @104..167, tpidr_el0 @168.
// (GAS/ELF syntax; this backend is only ever built for aarch64-linux-musl.)
#[repr(C)]
struct AkCtx {
    reg: [u64; 22],
}
impl AkCtx {
    const fn zero() -> Self {
        AkCtx { reg: [0; 22] }
    }
}

core::arch::global_asm!(
    r#"
    .text
    .globl akfiber_switch
akfiber_switch:
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    mov x2, sp
    str x2, [x0, #96]
    stp d8, d9,   [x0, #104]
    stp d10, d11, [x0, #120]
    stp d12, d13, [x0, #136]
    stp d14, d15, [x0, #152]
    mrs x2, tpidr_el0
    str x2, [x0, #168]
    ldp x19, x20, [x1, #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    ldp d8, d9,   [x1, #104]
    ldp d10, d11, [x1, #120]
    ldp d12, d13, [x1, #136]
    ldp d14, d15, [x1, #152]
    ldr x2, [x1, #168]
    msr tpidr_el0, x2
    ldr x2, [x1, #96]
    mov sp, x2
    ret
    .globl akfiber_tramp
akfiber_tramp:
    mov x0, x19
    mov x1, x20
    bl akfiber_start
    bl abort
"#
);

extern "C" {
    fn akfiber_switch(prev: *mut AkCtx, next: *mut AkCtx);
    fn akfiber_tramp();
}

#[inline]
fn rd_tpidr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) v) };
    v
}

/// Trampoline target: run the kthread entry, then exit cleanly. Never returns
/// (a rump kthread either loops forever or calls rumpuser_thread_exit; if its
/// entry returns we still tear the fiber down rather than fall off the stack).
#[no_mangle]
unsafe extern "C" fn akfiber_start(
    entry: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> ! {
    entry(arg);
    exit_thread();
}

/// Seed a fresh context so the first switch into it lands on the trampoline,
/// which calls `entry(arg)` on `stack_base`'s fiber stack.
unsafe fn akctx_make(c: *mut AkCtx, stack_base: *mut c_void, entry: usize, arg: *mut c_void) {
    (*c).reg = [0; 22];
    let top = ((stack_base as usize) + STACKSIZE) & !15usize;
    (*c).reg[0] = entry as u64; // x19
    (*c).reg[1] = arg as u64; // x20
    (*c).reg[11] = akfiber_tramp as *const () as usize as u64; // x30 -> trampoline
    (*c).reg[12] = top as u64; // sp
    (*c).reg[21] = rd_tpidr(); // inherit creator's TLS base so libc works
}

// ── intrusive doubly-linked list (TAILQ) ─────────────────────────────────────
struct Link<T> {
    next: *mut T,
    prev: *mut T,
}
impl<T> Link<T> {
    const fn null() -> Self {
        Link { next: ptr::null_mut(), prev: ptr::null_mut() }
    }
}

trait Linked: Sized {
    unsafe fn link(this: *mut Self) -> *mut Link<Self>;
}

struct List<T> {
    head: *mut T,
    tail: *mut T,
}
impl<T: Linked> List<T> {
    const fn new() -> Self {
        List { head: ptr::null_mut(), tail: ptr::null_mut() }
    }
    unsafe fn is_empty(&self) -> bool {
        self.head.is_null()
    }
    unsafe fn first(&self) -> *mut T {
        self.head
    }
    unsafe fn insert_tail(&mut self, n: *mut T) {
        let ln = T::link(n);
        (*ln).next = ptr::null_mut();
        (*ln).prev = self.tail;
        if self.tail.is_null() {
            self.head = n;
        } else {
            (*T::link(self.tail)).next = n;
        }
        self.tail = n;
    }
    unsafe fn insert_head(&mut self, n: *mut T) {
        let ln = T::link(n);
        (*ln).prev = ptr::null_mut();
        (*ln).next = self.head;
        if self.head.is_null() {
            self.tail = n;
        } else {
            (*T::link(self.head)).prev = n;
        }
        self.head = n;
    }
    unsafe fn remove(&mut self, n: *mut T) {
        let ln = T::link(n);
        let prev = (*ln).prev;
        let next = (*ln).next;
        if prev.is_null() {
            self.head = next;
        } else {
            (*T::link(prev)).next = next;
        }
        if next.is_null() {
            self.tail = prev;
        } else {
            (*T::link(next)).prev = prev;
        }
        (*ln).next = ptr::null_mut();
        (*ln).prev = ptr::null_mut();
    }
}

// ── thread + wait structures ─────────────────────────────────────────────────
#[repr(C)]
struct Thread {
    link: Link<Thread>,
    lwp: *mut c_void,
    wakeup_time: i64, // -1 = not sleeping
    ctx: AkCtx,
    flags: c_int,
    stack: *mut c_void, // mmap base to munmap on reap (null for the init thread)
}
impl Linked for Thread {
    unsafe fn link(this: *mut Self) -> *mut Link<Self> {
        &mut (*this).link
    }
}

struct Waiter {
    link: Link<Waiter>,
    who: *mut Thread,
    onlist: c_int,
}
impl Linked for Waiter {
    unsafe fn link(this: *mut Self) -> *mut Link<Self> {
        &mut (*this).link
    }
}

struct JoinWaiter {
    link: Link<JoinWaiter>,
    thread: *mut Thread, // the joiner, to wake
    wanted: *mut Thread, // the thread being joined
}
impl Linked for JoinWaiter {
    unsafe fn link(this: *mut Self) -> *mut Link<Self> {
        &mut (*this).link
    }
}

// ── scheduler globals (single OS thread → no locking) ─────────────────────────
static mut THREAD_LIST: List<Thread> = List::new();
static mut EXITED: List<Thread> = List::new();
static mut JOINWQ: List<JoinWaiter> = List::new();
static mut CURRENT: *mut Thread = ptr::null_mut();
static mut SCHED_HOOK: Option<extern "C" fn(*mut c_void, *mut c_void)> = None;

#[inline]
unsafe fn get_current() -> *mut Thread {
    CURRENT
}

#[inline]
unsafe fn now() -> i64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

#[inline]
unsafe fn is_runnable(t: *mut Thread) -> bool {
    (*t).flags & RUNNABLE_FLAG != 0
}
#[inline]
unsafe fn set_runnable(t: *mut Thread) {
    (*t).flags |= RUNNABLE_FLAG;
}
#[inline]
unsafe fn clear_runnable(t: *mut Thread) {
    (*t).flags &= !RUNNABLE_FLAG;
}

unsafe fn wake(t: *mut Thread) {
    (*t).wakeup_time = -1;
    set_runnable(t);
}
unsafe fn block(t: *mut Thread) {
    (*t).wakeup_time = -1;
    clear_runnable(t);
}

unsafe fn switch_threads(prev: *mut Thread, next: *mut Thread) {
    CURRENT = next;
    if let Some(hook) = SCHED_HOOK {
        // cookie field isn't tracked separately; pass lwp pointers (parity stub).
        hook((*prev).lwp, (*next).lwp);
    }
    akfiber_switch(&mut (*prev).ctx, &mut (*next).ctx);
}

/// Cooperative round-robin scheduler. Picks the next runnable thread, waking any
/// whose sleep timer expired; if none is runnable, sleeps the OS thread until the
/// next wakeup. Then reaps exited threads. Port of rumpfiber.c:schedule().
unsafe fn schedule() {
    let prev = get_current();
    let mut next: *mut Thread;

    loop {
        let tm = now();
        let mut wakeup = tm + 1000; // wake up in 1s max
        next = ptr::null_mut();

        // Walk all threads (capturing next link first — wake() may relink).
        let mut t = THREAD_LIST.first();
        while !t.is_null() {
            let tnext = (*Thread::link(t)).next;
            if !is_runnable(t) && (*t).wakeup_time >= 0 {
                if (*t).wakeup_time <= tm {
                    (*t).flags |= THREAD_TIMEDOUT;
                    wake(t);
                } else if (*t).wakeup_time < wakeup {
                    wakeup = (*t).wakeup_time;
                }
            }
            if is_runnable(t) {
                next = t;
                // move to tail (round-robin)
                THREAD_LIST.remove(t);
                THREAD_LIST.insert_tail(t);
                break;
            }
            t = tnext;
        }

        if !next.is_null() {
            break;
        }

        // Nothing runnable: sleep the OS thread until the soonest wakeup.
        let delta = wakeup - tm;
        let sl = Timespec {
            tv_sec: delta / 1000,
            tv_nsec: (delta % 1000) * 1_000_000,
        };
        nanosleep(&sl, ptr::null_mut());
    }

    if prev != next {
        switch_threads(prev, next);
    }

    // Reap exited threads (never the one we just switched away from).
    let mut t = EXITED.first();
    while !t.is_null() {
        let tnext = (*Thread::link(t)).next;
        if t != prev {
            EXITED.remove(t);
            if !(*t).stack.is_null() {
                munmap((*t).stack, STACKSIZE);
            }
            free(t as *mut c_void);
        }
        t = tnext;
    }
}

// ── thread lifecycle ──────────────────────────────────────────────────────────
unsafe fn create_thread(entry: usize, arg: *mut c_void) -> *mut Thread {
    let thr = malloc(core::mem::size_of::<Thread>()) as *mut Thread;
    if thr.is_null() {
        return ptr::null_mut();
    }
    let stack = mmap(ptr::null_mut(), STACKSIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if stack == MAP_FAILED {
        free(thr as *mut c_void);
        return ptr::null_mut();
    }
    ptr::write(
        thr,
        Thread {
            link: Link::null(),
            lwp: ptr::null_mut(),
            wakeup_time: -1,
            ctx: AkCtx::zero(),
            flags: 0,
            stack,
        },
    );
    akctx_make(&mut (*thr).ctx, stack, entry, arg);
    set_runnable(thr);
    THREAD_LIST.insert_tail(thr);
    thr
}

unsafe fn exit_thread() -> ! {
    let thread = get_current();

    // If joinable, gate until a joiner has seen us.
    while (*thread).flags & THREAD_MUSTJOIN != 0 {
        (*thread).flags |= THREAD_JOINED;
        // wake the joiner if it's already waiting
        let mut jw = JOINWQ.first();
        while !jw.is_null() {
            if (*jw).wanted == thread {
                wake((*jw).thread);
                break;
            }
            jw = (*JoinWaiter::link(jw)).next;
        }
        block(thread);
        schedule();
    }

    THREAD_LIST.remove(thread);
    clear_runnable(thread);
    EXITED.insert_head(thread);

    loop {
        schedule();
        dprint(b"fiber: schedule() returned to exited thread!\n");
    }
}

// THREAD_JOINED is set by the exiting thread while this joiner is parked in
// schedule() — mutated across the cooperative yield, invisible to clippy.
#[allow(clippy::while_immutable_condition)]
unsafe fn join_thread(joinable: *mut Thread) {
    let thread = get_current();
    while (*joinable).flags & THREAD_JOINED == 0 {
        let mut jw = JoinWaiter {
            link: Link::null(),
            thread,
            wanted: joinable,
        };
        let jwp: *mut JoinWaiter = &mut jw;
        JOINWQ.insert_tail(jwp);
        block(thread);
        schedule();
        JOINWQ.remove(jwp);
    }
    (*joinable).flags &= !THREAD_MUSTJOIN;
    wake(joinable);
}

unsafe fn msleep(millis: i64) {
    let thread = get_current();
    (*thread).wakeup_time = now() + millis;
    clear_runnable(thread);
    schedule();
}

unsafe fn abssleep(millis: i64) {
    let thread = get_current();
    (*thread).wakeup_time = millis;
    clear_runnable(thread);
    schedule();
}

/// Bring up the scheduler: the current (main) OS thread becomes the first fiber.
pub unsafe fn init_sched() {
    let thr = malloc(core::mem::size_of::<Thread>()) as *mut Thread;
    if thr.is_null() {
        abort();
    }
    ptr::write(
        thr,
        Thread {
            link: Link::null(),
            lwp: ptr::null_mut(),
            wakeup_time: -1,
            ctx: AkCtx::zero(),
            flags: 0,
            stack: ptr::null_mut(), // main thread's stack: not ours to munmap
        },
    );
    set_runnable(thr);
    THREAD_LIST.insert_tail(thr);
    CURRENT = thr;
}

// ── wait queues ───────────────────────────────────────────────────────────────
/// Block the current fiber on a wait queue, optionally with a timeout (millis).
/// Returns ETIMEDOUT if woken by timeout, else 0. Port of rumpfiber.c:wait().
unsafe fn wait(wh: *mut List<Waiter>, msec: i64) -> c_int {
    let cur = get_current();
    let mut w = Waiter {
        link: Link::null(),
        who: cur,
        onlist: 1,
    };
    let wp: *mut Waiter = &mut w;
    (*wh).insert_tail(wp);
    block(cur);
    if msec != 0 {
        (*cur).wakeup_time = now() + msec;
    }
    schedule();

    // Woken by timeout (still on the list)?
    if w.onlist != 0 {
        (*wh).remove(wp);
        ETIMEDOUT
    } else {
        0
    }
}

unsafe fn wakeup_one(wh: *mut List<Waiter>) {
    let w = (*wh).first();
    if !w.is_null() {
        (*wh).remove(w);
        (*w).onlist = 0;
        wake((*w).who);
    }
}

unsafe fn wakeup_all(wh: *mut List<Waiter>) {
    loop {
        let w = (*wh).first();
        if w.is_null() {
            break;
        }
        (*wh).remove(w);
        (*w).onlist = 0;
        wake((*w).who);
    }
}

// ── rump CPU schedule bridge (shared with the pthread backend in lib.rs) ──────
#[inline]
unsafe fn rumpkern_unsched(interlock: *mut c_void) -> c_int {
    crate::rumpkern_unsched(interlock)
}
#[inline]
unsafe fn rumpkern_sched(nlocks: c_int, interlock: *mut c_void) {
    crate::rumpkern_sched(nlocks, interlock)
}

unsafe fn dprint(msg: &[u8]) {
    write(2, msg.as_ptr() as *const c_void, msg.len());
}

// ══════════════════════════════════════════════════════════════════════════════
// rumpuser_* hypercall exports (fiber backend)
// ══════════════════════════════════════════════════════════════════════════════

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_create(
    f: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    _thrname: *const c_char,
    mustjoin: c_int,
    _priority: c_int,
    _cpuidx: c_int,
    cookie: *mut *mut c_void,
) -> c_int {
    let thr = create_thread(f as usize, arg);
    if thr.is_null() {
        return EINVAL;
    }
    if mustjoin != 0 {
        (*thr).flags |= THREAD_MUSTJOIN;
    }
    if !cookie.is_null() {
        *cookie = thr as *mut c_void;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_exit() -> ! {
    exit_thread();
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_join(cookie: *mut c_void) -> c_int {
    join_thread(cookie as *mut Thread);
    0
}

// ── curlwp ────────────────────────────────────────────────────────────────────
#[no_mangle]
pub unsafe extern "C" fn rumpuser_curlwpop(op: c_int, lwp: *mut c_void) -> c_int {
    match op {
        RUMPUSER_LWP_SET => (*get_current()).lwp = lwp,
        RUMPUSER_LWP_CLEAR => (*get_current()).lwp = ptr::null_mut(),
        RUMPUSER_LWP_CREATE | RUMPUSER_LWP_DESTROY => { /* bookkeeping only */ }
        _ => {}
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_curlwp() -> *mut c_void {
    (*get_current()).lwp
}

// ── clock_sleep (cooperative: yields to other fibers instead of blocking all) ──
#[no_mangle]
pub unsafe extern "C" fn rumpuser_clock_sleep(enum_: c_int, sec: i64, nsec: c_long) -> c_int {
    let nlocks = rumpkern_unsched(ptr::null_mut());
    let msec = sec * 1000 + nsec / 1_000_000;
    if enum_ == RUMPUSER_CLOCK_ABSMONO {
        abssleep(msec);
    } else {
        msleep(msec);
    }
    rumpkern_sched(nlocks, ptr::null_mut());
    0
}

// ── mutex ───────────────────────────────────────────────────────────────────
#[repr(C)]
pub struct Mtx {
    waiters: List<Waiter>,
    v: c_int,
    flags: c_int,
    o: *mut c_void,
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_init(mtxp: *mut *mut Mtx, flags: c_int) {
    let m = malloc(core::mem::size_of::<Mtx>()) as *mut Mtx;
    ptr::write(
        m,
        Mtx {
            waiters: List::new(),
            v: 0,
            flags,
            o: ptr::null_mut(),
        },
    );
    *mtxp = m;
}

#[no_mangle]
// The retry condition is mutated by OTHER fibers (which release the mutex while
// this one is parked in wait()) through the shared pointer — clippy can't see
// that across the cooperative yield. Mirrors rumpfiber.c's enter loop.
#[allow(clippy::while_immutable_condition)]
pub unsafe extern "C" fn rumpuser_mutex_enter(m: *mut Mtx) {
    if rumpuser_mutex_tryenter(m) != 0 {
        let nlocks = rumpkern_unsched(ptr::null_mut());
        while rumpuser_mutex_tryenter(m) != 0 {
            wait(&mut (*m).waiters, 0);
        }
        rumpkern_sched(nlocks, ptr::null_mut());
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_enter_nowrap(m: *mut Mtx) {
    // One vCPU, no preemption => the lock must be free (matches rumpfiber.c).
    if rumpuser_mutex_tryenter(m) != 0 {
        dprint(b"fiber: mutex_enter_nowrap on held lock\n");
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_tryenter(m: *mut Mtx) -> c_int {
    let l = (*get_current()).lwp;
    if (*m).v != 0 && (*m).o != l {
        return EBUSY;
    }
    (*m).v += 1;
    (*m).o = l;
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_exit(m: *mut Mtx) {
    (*m).v -= 1;
    if (*m).v == 0 {
        (*m).o = ptr::null_mut();
        wakeup_one(&mut (*m).waiters);
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_destroy(m: *mut Mtx) {
    free(m as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_owner(m: *mut Mtx, lp: *mut *mut c_void) {
    *lp = (*m).o;
}

// ── rwlock ──────────────────────────────────────────────────────────────────
#[repr(C)]
pub struct Rw {
    rwait: List<Waiter>,
    wwait: List<Waiter>,
    v: c_int,
    o: *mut c_void,
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_init(rwp: *mut *mut Rw) {
    let rw = malloc(core::mem::size_of::<Rw>()) as *mut Rw;
    ptr::write(
        rw,
        Rw {
            rwait: List::new(),
            wwait: List::new(),
            v: 0,
            o: ptr::null_mut(),
        },
    );
    *rwp = rw;
}

#[no_mangle]
#[allow(clippy::while_immutable_condition)] // released by other fibers across wait()
pub unsafe extern "C" fn rumpuser_rw_enter(enum_: c_int, rw: *mut Rw) {
    let wq: *mut List<Waiter> = if enum_ == RUMPUSER_RW_WRITER {
        &mut (*rw).wwait
    } else {
        &mut (*rw).rwait
    };
    if rumpuser_rw_tryenter(enum_, rw) != 0 {
        let nlocks = rumpkern_unsched(ptr::null_mut());
        while rumpuser_rw_tryenter(enum_, rw) != 0 {
            wait(wq, 0);
        }
        rumpkern_sched(nlocks, ptr::null_mut());
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_tryenter(enum_: c_int, rw: *mut Rw) -> c_int {
    if enum_ == RUMPUSER_RW_WRITER {
        if (*rw).o.is_null() {
            (*rw).o = rumpuser_curlwp();
            0
        } else {
            EBUSY
        }
    } else if (*rw).o.is_null() && (*rw).wwait.is_empty() {
        (*rw).v += 1;
        0
    } else {
        EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_exit(rw: *mut Rw) {
    if !(*rw).o.is_null() {
        (*rw).o = ptr::null_mut();
    } else {
        (*rw).v -= 1;
    }
    // Don't let readers starve writers.
    if !(*rw).wwait.is_empty() {
        if (*rw).o.is_null() {
            wakeup_one(&mut (*rw).wwait);
        }
    } else if !(*rw).rwait.is_empty() && (*rw).o.is_null() {
        wakeup_all(&mut (*rw).rwait);
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_destroy(rw: *mut Rw) {
    free(rw as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_held(enum_: c_int, rw: *mut Rw, rvp: *mut c_int) {
    *rvp = if enum_ == RUMPUSER_RW_WRITER {
        c_int::from((*rw).o == rumpuser_curlwp() && !(*rw).o.is_null())
    } else {
        c_int::from((*rw).v > 0)
    };
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_downgrade(rw: *mut Rw) {
    // Mirrors rumpfiber.c verbatim.
    (*rw).v = -1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_tryupgrade(rw: *mut Rw) -> c_int {
    if (*rw).v == -1 {
        (*rw).v = 1;
        (*rw).o = rumpuser_curlwp();
        0
    } else {
        EBUSY
    }
}

// ── condvar ─────────────────────────────────────────────────────────────────
#[repr(C)]
pub struct Cv {
    waiters: List<Waiter>,
    nwaiters: c_int,
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_init(cvp: *mut *mut Cv) {
    let cv = malloc(core::mem::size_of::<Cv>()) as *mut Cv;
    ptr::write(
        cv,
        Cv {
            waiters: List::new(),
            nwaiters: 0,
        },
    );
    *cvp = cv;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_destroy(cv: *mut Cv) {
    free(cv as *mut c_void);
}

// Release the rump CPU + drop the interlock before parking; reacquire on wake.
unsafe fn cv_unsched(m: *mut Mtx) -> c_int {
    let nlocks = rumpkern_unsched(m as *mut c_void);
    rumpuser_mutex_exit(m);
    nlocks
}

unsafe fn cv_resched(m: *mut Mtx, nlocks: c_int) {
    if (*m).flags & (RUMPUSER_MTX_SPIN | RUMPUSER_MTX_KMUTEX)
        == (RUMPUSER_MTX_SPIN | RUMPUSER_MTX_KMUTEX)
    {
        rumpkern_sched(nlocks, m as *mut c_void);
        rumpuser_mutex_enter_nowrap(m);
    } else {
        rumpuser_mutex_enter_nowrap(m);
        rumpkern_sched(nlocks, m as *mut c_void);
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait(cv: *mut Cv, m: *mut Mtx) {
    (*cv).nwaiters += 1;
    let nlocks = cv_unsched(m);
    wait(&mut (*cv).waiters, 0);
    cv_resched(m, nlocks);
    (*cv).nwaiters -= 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait_nowrap(cv: *mut Cv, m: *mut Mtx) {
    (*cv).nwaiters += 1;
    rumpuser_mutex_exit(m);
    wait(&mut (*cv).waiters, 0);
    rumpuser_mutex_enter_nowrap(m);
    (*cv).nwaiters -= 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_timedwait(cv: *mut Cv, m: *mut Mtx, sec: i64, nsec: i64) -> c_int {
    (*cv).nwaiters += 1;
    let nlocks = cv_unsched(m);
    let rv = wait(&mut (*cv).waiters, sec * 1000 + nsec / 1_000_000);
    cv_resched(m, nlocks);
    (*cv).nwaiters -= 1;
    rv
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_signal(cv: *mut Cv) {
    wakeup_one(&mut (*cv).waiters);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_broadcast(cv: *mut Cv) {
    wakeup_all(&mut (*cv).waiters);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_has_waiters(cv: *mut Cv, rvp: *mut c_int) {
    *rvp = c_int::from((*cv).nwaiters != 0);
}

// Keep CLOCK_REALTIME referenced (parity with upstream's realtime abssleep path;
// fiber timedwait uses monotonic deltas, so this avoids a dead-const warning).
const _: c_int = CLOCK_REALTIME;
