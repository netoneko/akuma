//! `rumpuser-akuma` — the rump-kernel hypercall layer for Akuma, in Rust.
//!
//! Exports the `rumpuser_*` C ABI (`RUMPUSER_VERSION 17`, per
//! `src-netbsd/sys/rump/include/rump/rumpuser.h`) that a NetBSD rump kernel
//! links against. This replaces NetBSD's C `librumpuser` (we build the rump libs
//! with buildrump `-k`, kernel-only, so that C layer is skipped — see
//! docs/PHASE01_BUILDRUMP.md).
//!
//! **Stub-first**: the init-critical families (memory, clock, randomness,
//! console, errno, params, threads, locks/cv, curlwp) are implemented over
//! libc/pthread — which is exactly how NetBSD's own librumpuser works on Linux,
//! and on Akuma musl libc is itself backed by Akuma syscalls. The file/disk I/O
//! (`bio`/`iov`/`syncfd`/`open`), syscall-proxy (`sp_*`), and dynloader families
//! are safe stubs (not needed to `rump_init()` the networking stack); they are
//! filled in as later phases need them.
//!
//! Goal of this phase: `rump_init()` links and returns success (proved first in a
//! Linux container, then on Akuma). `rumpuser_dprintf` (C variadic) lives in the
//! companion `csupport.c`.
//!
//! `no_std`: this is pure syscall/libc glue — no allocator, no std runtime. It
//! links into a musl C program, which supplies libc/pthread and the
//! compiler-builtin `memcpy`/`memset`. Matches the rest of Akuma's Rust userspace.
//!
//! Build with `--features rumpuser_debug` to trace every hypercall (and the
//! memory sizes/pointers) to stderr — used to localise the bring-up crash.

// `no_std` for the shipped staticlib; under `cargo test` we pull in `std` so the
// default test harness (and its own panic handler) are available — the fiber
// cooperative-primitive tests run as a normal Rust test binary (see fiber.rs
// `mod tests`). The hand-rolled aarch64 asm is ELF-style, so tests run on
// linux/arm64 (cross-build `--no-run`, execute in a Docker arm64 container).
#![cfg_attr(not(test), no_std)]
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]
#![allow(static_mut_refs)]

use core::ffi::{c_char, c_int, c_long, c_void};
use core::ptr;

/// Panic = abort (no unwinding in a freestanding syscall-glue staticlib). Under
/// `cfg(test)` std supplies the panic handler, so this one must stand down.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { abort() }
}

/// Trace a hypercall name to stderr when the `rumpuser_debug` feature is on.
/// Expands to nothing otherwise (zero cost in release).
macro_rules! tr {
    ($name:literal) => {{
        #[cfg(feature = "rumpuser_debug")]
        trace($name);
    }};
}

// ── libc / pthread externs (resolved by the musl libc the final program links) ──

extern "C" {
    // malloc: only the pthread backend's lock/cv allocs use it (the fiber backend
    // declares its own); rumpuser_malloc itself uses posix_memalign.
    #[cfg(not(feature = "threads_fiber"))]
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn posix_memalign(memptr: *mut *mut c_void, align: usize, size: usize) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn clock_gettime(clk: c_int, ts: *mut Timespec) -> c_int;
    // nanosleep: only the pthread clock_sleep uses it (fiber yields cooperatively).
    #[cfg(not(feature = "threads_fiber"))]
    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn abort() -> !;
    fn _exit(code: c_int) -> !;
    fn strlen(s: *const c_char) -> usize;
    fn getenv(name: *const c_char) -> *const c_char;
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
    fn getrandom(buf: *mut c_void, buflen: usize, flags: u32) -> isize;

    // Only referenced by the rumpuser_debug tid trace; keep the decl unconditionally.
    #[allow(dead_code)]
    fn pthread_self() -> *mut c_void;
}

// pthread primitives used only by the default (pthread) threading backend; gated
// out under `threads_fiber` (the fiber backend supplies its own scheduler).
#[cfg(not(feature = "threads_fiber"))]
extern "C" {
    fn pthread_create(t: *mut PthreadT, attr: *const c_void, f: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> c_int;
    fn pthread_join(t: PthreadT, retval: *mut *mut c_void) -> c_int;
    fn pthread_exit(retval: *mut c_void) -> !;
    fn pthread_key_create(key: *mut PthreadKey, destructor: *const c_void) -> c_int;
    fn pthread_setspecific(key: PthreadKey, value: *const c_void) -> c_int;
    fn pthread_getspecific(key: PthreadKey) -> *mut c_void;

    fn pthread_mutex_init(m: *mut c_void, attr: *const c_void) -> c_int;
    fn pthread_mutex_lock(m: *mut c_void) -> c_int;
    fn pthread_mutex_trylock(m: *mut c_void) -> c_int;
    fn pthread_mutex_unlock(m: *mut c_void) -> c_int;
    fn pthread_mutex_destroy(m: *mut c_void) -> c_int;

    fn pthread_rwlock_init(rw: *mut c_void, attr: *const c_void) -> c_int;
    fn pthread_rwlock_rdlock(rw: *mut c_void) -> c_int;
    fn pthread_rwlock_wrlock(rw: *mut c_void) -> c_int;
    fn pthread_rwlock_tryrdlock(rw: *mut c_void) -> c_int;
    fn pthread_rwlock_trywrlock(rw: *mut c_void) -> c_int;
    fn pthread_rwlock_unlock(rw: *mut c_void) -> c_int;
    fn pthread_rwlock_destroy(rw: *mut c_void) -> c_int;

    fn pthread_cond_init(c: *mut c_void, attr: *const c_void) -> c_int;
    fn pthread_cond_wait(c: *mut c_void, m: *mut c_void) -> c_int;
    fn pthread_cond_timedwait(c: *mut c_void, m: *mut c_void, abstime: *const Timespec) -> c_int;
    fn pthread_cond_signal(c: *mut c_void) -> c_int;
    fn pthread_cond_broadcast(c: *mut c_void) -> c_int;
    fn pthread_cond_destroy(c: *mut c_void) -> c_int;
}

#[cfg_attr(feature = "threads_fiber", allow(dead_code))]
type PthreadT = *mut c_void; // musl pthread_t is pointer-sized
#[cfg_attr(feature = "threads_fiber", allow(dead_code))]
type PthreadKey = u32; // musl pthread_key_t is unsigned int

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: c_long,
}

// errno values used here
const ENOMEM: c_int = 12;
#[cfg(not(feature = "threads_fiber"))] // only the pthread lock/cv paths use EBUSY
const EBUSY: c_int = 16;
const ENXIO: c_int = 6;

// CLOCK_* (Linux/musl)
const CLOCK_REALTIME: c_int = 0;
const CLOCK_MONOTONIC: c_int = 1;

// mmap prot/flags (Linux/musl)
const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const PROT_EXEC: c_int = 0x4;
const MAP_PRIVATE: c_int = 0x2;
const MAP_ANON: c_int = 0x20;
const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

#[inline]
unsafe fn set_errno(e: c_int) {
    *__errno_location() = e;
}

// ── init ────────────────────────────────────────────────────────────────────

/// The hypervisor upcall table the rump kernel hands us at init. Stored for the
/// scheduler-wrap lock paths (filled in later); kept verbatim.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RumpHyperUp {
    hyp_schedule: Option<extern "C" fn()>,
    hyp_unschedule: Option<extern "C" fn()>,
    hyp_backend_unschedule: Option<extern "C" fn(c_int, *mut c_int, *mut c_void)>,
    hyp_backend_schedule: Option<extern "C" fn(c_int, *mut c_void)>,
    hyp_lwproc_switch: Option<extern "C" fn(*mut c_void)>,
    hyp_lwproc_release: Option<extern "C" fn()>,
    hyp_lwproc_rfork: Option<extern "C" fn(*mut c_void, c_int, *const c_char) -> c_int>,
    hyp_lwproc_newlwp: Option<extern "C" fn(c_int) -> c_int>,
    hyp_lwproc_curlwp: Option<extern "C" fn() -> *mut c_void>,
    hyp_syscall: Option<extern "C" fn(c_int, *mut c_void, *mut c_long) -> c_int>,
    hyp_lwpexit: Option<extern "C" fn()>,
    hyp_execnotify: Option<extern "C" fn(*const c_char)>,
    hyp_getpid: Option<extern "C" fn() -> c_int>,
    hyp_extra: [*mut c_void; 8],
}

static mut HYPERUP: *const RumpHyperUp = ptr::null();

/// The C global the rump kernel's own C glue (and, post-Step-1, the in-tree
/// sysproxy server `rumpuser_sp.c`/`sp_common.c`) read by value as
/// `rumpuser__hyp.hyp_*`. In stock NetBSD this lives in `rumpuser.c`/`rumpfiber.c`;
/// since our rumpuser is Rust, we export it here and populate it in `rumpuser_init`.
///
/// SECURITY TODO (see docs/RUMP_SYSPROXY.md "Security / hardening TODOs"): this is a
/// function-pointer table in writable `.data`. It is write-once (set in
/// `rumpuser_init`, read-only after), so before any non-showcase use it should be
/// `mprotect()`-sealed read-only right after init to remove the post-init overwrite
/// gadget. Acceptable as-is for the non-prod showcase; matches stock NetBSD posture.
#[no_mangle]
pub static mut rumpuser__hyp: RumpHyperUp = RumpHyperUp {
    hyp_schedule: None,
    hyp_unschedule: None,
    hyp_backend_unschedule: None,
    hyp_backend_schedule: None,
    hyp_lwproc_switch: None,
    hyp_lwproc_release: None,
    hyp_lwproc_rfork: None,
    hyp_lwproc_newlwp: None,
    hyp_lwproc_curlwp: None,
    hyp_syscall: None,
    hyp_lwpexit: None,
    hyp_execnotify: None,
    hyp_getpid: None,
    hyp_extra: [ptr::null_mut(); 8],
};

/// Cooperative (fiber) threading backend — a Rust port of NetBSD's rumpfiber.c
/// on top of an aarch64 context switch. Replaces the pthread threading/sync/
/// curlwp/clock_sleep hypercalls below when `--features threads_fiber` is set.
#[cfg(feature = "threads_fiber")]
mod fiber;

/// Rust port of the `rump_server` wrapper `main` (FIBER_HANDOFF.md port).
/// Feature-gated so this `#[no_mangle] main` is absent from the default
/// `librumpuser_akuma.a` (other consumers — rumphttp/sic/tests — define their own
/// `main`); the shipped rump_server binary builds with `--features rump_server_main`.
#[cfg(feature = "rump_server_main")]
mod rump_server;

/// pthread TLS key holding the current lwp pointer (per host thread).
#[cfg(not(feature = "threads_fiber"))]
static mut CURLWP_KEY: PthreadKey = 0;

#[no_mangle]
pub unsafe extern "C" fn rumpuser_init(version: c_int, hyp: *const RumpHyperUp) -> c_int {
    tr!(b"init");
    const RUMPUSER_VERSION: c_int = 17;
    if version != RUMPUSER_VERSION {
        dprint(b"rumpuser_init: version mismatch\n");
        return 1;
    }
    // Copy the upcall table by value into the exported C global (stock NetBSD
    // does `rumpuser__hyp = *hyp;`), then point HYPERUP at our stable copy so the
    // caller's hyp need not outlive init.
    rumpuser__hyp = *hyp;
    HYPERUP = core::ptr::addr_of!(rumpuser__hyp);
    // Allocate the curlwp TLS key once (init is single-threaded).
    #[cfg(not(feature = "threads_fiber"))]
    if pthread_key_create(&mut CURLWP_KEY, ptr::null()) != 0 {
        return ENOMEM;
    }
    // Fiber backend: bring up the cooperative scheduler instead (the main thread
    // becomes the first fiber; no pthread TLS key needed for curlwp).
    #[cfg(feature = "threads_fiber")]
    fiber::init_sched();
    0
}

// ── memory ────────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_malloc(len: usize, alignment: c_int, memp: *mut *mut c_void) -> c_int {
    tr!(b"malloc");
    let align = if alignment <= 0 { core::mem::size_of::<usize>() } else { alignment as usize };
    // posix_memalign requires alignment to be a power of two and >= sizeof(void*).
    let align = align.max(core::mem::size_of::<usize>()).next_power_of_two();
    let mut ptr: *mut c_void = ptr::null_mut();
    let rv = posix_memalign(&mut ptr, align, len);
    if rv != 0 {
        return rv;
    }
    *memp = ptr;
    #[cfg(feature = "rumpuser_debug")]
    dbg3(b"  malloc len/align/ptr ", len, align, ptr as usize);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_free(ptr: *mut c_void, _size: usize) {
    tr!(b"free");
    free(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_anonmmap(
    _prefaddr: *mut c_void,
    size: usize,
    _alignbit: c_int,
    exec: c_int,
    memp: *mut *mut c_void,
) -> c_int {
    tr!(b"anonmmap");
    let mut prot = PROT_READ | PROT_WRITE;
    if exec != 0 {
        prot |= PROT_EXEC;
    }
    let p = mmap(ptr::null_mut(), size, prot, MAP_PRIVATE | MAP_ANON, -1, 0);
    if p == MAP_FAILED {
        return *__errno_location();
    }
    *memp = p;
    #[cfg(feature = "rumpuser_debug")]
    dbg3(b"  anonmmap size/exec/ptr ", size, exec as usize, p as usize);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_unmap(addr: *mut c_void, size: usize) {
    tr!(b"unmap");
    munmap(addr, size);
}

// ── clock ───────────────────────────────────────────────────────────────────

const RUMPUSER_CLOCK_ABSMONO: c_int = 1;

#[no_mangle]
pub unsafe extern "C" fn rumpuser_clock_gettime(enum_: c_int, sec: *mut i64, nsec: *mut c_long) -> c_int {
    tr!(b"clock_gettime");
    let clk = if enum_ == RUMPUSER_CLOCK_ABSMONO { CLOCK_MONOTONIC } else { CLOCK_REALTIME };
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    if clock_gettime(clk, &mut ts) != 0 {
        return *__errno_location();
    }
    *sec = ts.tv_sec;
    *nsec = ts.tv_nsec;
    0
}

// pthread clock_sleep: unschedule the rump CPU around a real host nanosleep.
// (Fiber backend replaces this with a cooperative msleep that yields to the
// scheduler — a real nanosleep there would block ALL fibers.)
#[cfg(not(feature = "threads_fiber"))]
#[no_mangle]
pub unsafe extern "C" fn rumpuser_clock_sleep(enum_: c_int, sec: i64, nsec: c_long) -> c_int {
    tr!(b"clock_sleep");
    // CRITICAL: release the rump CPU for the duration of the sleep. Otherwise the
    // calling lwp (e.g. the hardclock thread, which sleeps to the next tick every
    // ~10ms) holds the single rump CPU across the sleep and STARVES every other
    // lwp — a thread blocked in the scheduler slowpath waiting for the CPU then
    // never runs (this is what wedged ifcreate). Matches NetBSD rumpuser_clock_sleep.
    let nlocks = rumpkern_unsched(ptr::null_mut());
    // RELWALL: relative sleep. ABSMONO: sleep until the absolute monotonic time.
    let req = if enum_ == RUMPUSER_CLOCK_ABSMONO {
        let mut now = Timespec { tv_sec: 0, tv_nsec: 0 };
        clock_gettime(CLOCK_MONOTONIC, &mut now);
        let mut s = sec - now.tv_sec;
        let mut n = nsec - now.tv_nsec;
        if n < 0 {
            n += 1_000_000_000;
            s -= 1;
        }
        if s < 0 {
            // Deadline already passed — re-acquire the CPU before returning.
            rumpkern_sched(nlocks, ptr::null_mut());
            return 0;
        }
        Timespec { tv_sec: s, tv_nsec: n }
    } else {
        Timespec { tv_sec: sec, tv_nsec: nsec }
    };
    nanosleep(&req, ptr::null_mut());
    rumpkern_sched(nlocks, ptr::null_mut());
    0
}

// ── host params ───────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_getparam(name: *const c_char, buf: *mut c_void, blen: usize) -> c_int {
    tr!(b"getparam");
    let n = cstr(name);

    // Honor the host environment first for every param (as NetBSD's own
    // librumpuser does): a set RUMP_VERBOSE / RUMP_NCPU / RUMP_MEMLIMIT / … wins.
    let env = getenv(name);
    if !env.is_null() {
        let len = strlen(env) + 1; // include NUL
        if len > blen {
            return ENOMEM;
        }
        memcpy(buf, env as *const c_void, len);
        return 0;
    }

    // Defaults when the env doesn't set it.
    let default: &[u8] = if n == b"_RUMPUSER_NCPU" {
        b"1\0"
    } else if n == b"_RUMPUSER_HOSTNAME" {
        b"rump-akuma\0"
    } else if n == b"RUMP_VERBOSE" {
        // ON by default so the NetBSD copyright banner + boot steps print — we
        // keep the NetBSD attribution visible out of respect. The `rump_quiet`
        // cargo feature flips the default off; an explicit RUMP_VERBOSE env
        // (handled above) still overrides either way.
        if cfg!(feature = "rump_quiet") { b"0\0" } else { b"1\0" }
    } else {
        return ENXIO;
    };
    if default.len() > blen {
        return ENOMEM;
    }
    memcpy(buf, default.as_ptr() as *const c_void, default.len());
    0
}

// ── errno / termination ─────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_seterrno(e: c_int) {
    tr!(b"seterrno");
    set_errno(e);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_kill(_pid: i64, _sig: c_int) -> c_int {
    tr!(b"kill");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_exit(value: c_int) -> ! {
    tr!(b"exit");
    const RUMPUSER_PANIC: c_int = -1;
    if value == RUMPUSER_PANIC {
        dprint(b"rumpuser_exit: PANIC\n");
        abort();
    }
    _exit(value);
}

// ── console ─────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_putchar(ch: c_int) {
    // (no tr! — fires per character; would drown the trace)
    let b = ch as u8;
    write(1, &b as *const u8 as *const c_void, 1);
}

// rumpuser_dprintf (C variadic) lives in csupport.c.

// ── randomness ──────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_getrandom(buf: *mut c_void, buflen: usize, _flags: c_int, retp: *mut usize) -> c_int {
    tr!(b"getrandom");
    let mut got = 0usize;
    while got < buflen {
        let r = getrandom((buf as *mut u8).add(got) as *mut c_void, buflen - got, 0);
        if r < 0 {
            // getrandom unavailable/failed: fall back to zero-fill so init proceeds.
            break;
        }
        got += r as usize;
    }
    *retp = if got == 0 { buflen } else { got };
    0
}

// ── Akuma backend-capability hooks (not part of the rumpuser contract) ────────
// Let a packet backend (rumpcomp_tap.c) adapt its blocking I/O to the backend:
// the fiber backend runs all rump kthreads on ONE OS thread, so a blocking tap
// read would freeze every fiber — there it must poll non-blocking + cooperatively
// yield. The pthread backend keeps a real blocking read (its RX is its own thread).

/// 1 if rump kthreads are cooperative fibers on one OS thread; 0 for pthread.
#[no_mangle]
pub extern "C" fn rumpuser_akuma_cooperative() -> c_int {
    if cfg!(feature = "threads_fiber") {
        1
    } else {
        0
    }
}

/// Short yield for a backend's cooperative poll loop. pthread build: a real 1ms
/// sleep (the fiber build overrides this with a cooperative scheduler yield).
#[cfg(not(feature = "threads_fiber"))]
#[no_mangle]
pub unsafe extern "C" fn rumpuser_akuma_yield() {
    let req = Timespec { tv_sec: 0, tv_nsec: 1_000_000 };
    nanosleep(&req, ptr::null_mut());
}

// ── pthread threading/sync/curlwp backend ────────────────────────────────────
// The default backend: rump kthreads are 1:1 host pthreads; locks/cv/rw map to
// pthread primitives. Wrapped in a module so the entire block is swapped out for
// the cooperative `fiber` backend under `--features threads_fiber` (the
// #[no_mangle] exports below are unaffected by the module path). See
// docs/HIJACK_VS_KERNEL_PROXY.md.
#[cfg(not(feature = "threads_fiber"))]
mod pthread_backend {
    use super::*;

// ── threads ─────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_create(
    f: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    _thrname: *const c_char,
    _mustjoin: c_int,
    _priority: c_int,
    _cpuidx: c_int,
    cookie: *mut *mut c_void,
) -> c_int {
    tr!(b"thread_create");
    let mut tid: PthreadT = ptr::null_mut();
    let rv = pthread_create(&mut tid, ptr::null(), f, arg);
    if rv != 0 {
        return rv;
    }
    if !cookie.is_null() {
        *cookie = tid;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_exit() -> ! {
    tr!(b"thread_exit");
    pthread_exit(ptr::null_mut());
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_thread_join(cookie: *mut c_void) -> c_int {
    tr!(b"thread_join");
    pthread_join(cookie as PthreadT, ptr::null_mut())
}

// ── curlwp (thread-local current lwp pointer) ───────────────────────────────

const RUMPUSER_LWP_CREATE: c_int = 0;
const RUMPUSER_LWP_DESTROY: c_int = 1;
const RUMPUSER_LWP_SET: c_int = 2;
const RUMPUSER_LWP_CLEAR: c_int = 3;

#[no_mangle]
pub unsafe extern "C" fn rumpuser_curlwpop(op: c_int, lwp: *mut c_void) -> c_int {
    tr!(b"curlwpop");
    match op {
        RUMPUSER_LWP_SET => { pthread_setspecific(CURLWP_KEY, lwp); }
        RUMPUSER_LWP_CLEAR => { pthread_setspecific(CURLWP_KEY, ptr::null()); }
        RUMPUSER_LWP_CREATE | RUMPUSER_LWP_DESTROY => { /* bookkeeping only */ }
        _ => {}
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_curlwp() -> *mut c_void {
    // (no tr! — extremely hot; would drown the trace)
    pthread_getspecific(CURLWP_KEY)
}

// ── mutex ───────────────────────────────────────────────────────────────────
//
// musl pthread_mutex_t = 40 bytes / cond = 48 / rwlock = 56 on aarch64; the
// generously-sized, 8-aligned buffers below hold them with room to spare.

const RUMPUSER_MTX_SPIN: c_int = 0x01;
const RUMPUSER_MTX_KMUTEX: c_int = 0x02;

#[repr(C, align(8))]
pub struct Mtx {
    pm: [u8; 64],
    owner: *mut c_void,
    flags: c_int,
}

/// Record/clear ownership — only meaningful for KMUTEX (kernel adaptive) mutexes;
/// spin mutexes don't track an owner (mirrors NetBSD's mtxenter/mtxexit).
#[inline]
unsafe fn mtxenter(m: *mut Mtx) {
    if (*m).flags & RUMPUSER_MTX_KMUTEX != 0 {
        (*m).owner = rumpuser_curlwp();
    }
}
#[inline]
unsafe fn mtxexit(m: *mut Mtx) {
    if (*m).flags & RUMPUSER_MTX_KMUTEX != 0 {
        (*m).owner = ptr::null_mut();
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_init(mtxp: *mut *mut Mtx, flags: c_int) {
    tr!(b"mutex_init");
    let m = malloc(core::mem::size_of::<Mtx>()) as *mut Mtx;
    pthread_mutex_init((*m).pm.as_mut_ptr() as *mut c_void, ptr::null());
    (*m).owner = ptr::null_mut();
    (*m).flags = flags;
    *mtxp = m;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_enter(m: *mut Mtx) {
    tr!(b"mutex_enter");
    // Spin mutexes must NOT release the rump CPU (held briefly, taken in contexts
    // where unscheduling is illegal) — go straight to the no-wrap path.
    if (*m).flags & RUMPUSER_MTX_SPIN != 0 {
        rumpuser_mutex_enter_nowrap(m);
        return;
    }
    let p = (*m).pm.as_mut_ptr() as *mut c_void;
    // Only release the rump CPU if the lock is actually contended; an uncontended
    // acquire is a fast path and must not unschedule. On contention, unschedule so
    // the lwp that holds the lock can be scheduled to release it (single rump CPU).
    if pthread_mutex_trylock(p) != 0 {
        let nlocks = rumpkern_unsched(ptr::null_mut());
        pthread_mutex_lock(p);
        rumpkern_sched(nlocks, ptr::null_mut());
    }
    mtxenter(m);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_enter_nowrap(m: *mut Mtx) {
    tr!(b"mutex_enter_nowrap");
    pthread_mutex_lock((*m).pm.as_mut_ptr() as *mut c_void);
    mtxenter(m);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_tryenter(m: *mut Mtx) -> c_int {
    tr!(b"mutex_tryenter");
    if pthread_mutex_trylock((*m).pm.as_mut_ptr() as *mut c_void) == 0 {
        mtxenter(m);
        0
    } else {
        EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_exit(m: *mut Mtx) {
    tr!(b"mutex_exit");
    mtxexit(m);
    pthread_mutex_unlock((*m).pm.as_mut_ptr() as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_destroy(m: *mut Mtx) {
    tr!(b"mutex_destroy");
    pthread_mutex_destroy((*m).pm.as_mut_ptr() as *mut c_void);
    free(m as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_owner(m: *mut Mtx, lp: *mut *mut c_void) {
    tr!(b"mutex_owner");
    *lp = (*m).owner;
}

// ── rwlock ──────────────────────────────────────────────────────────────────

// We track ownership ourselves (pthread offers no portable "held" query, and the
// rump kernel KASSERTs `rw_lock_held()`): `writer` is the lwp holding it
// exclusively (or null), `readers` the shared-hold count. Mirrors NetBSD's own
// librumpuser bookkeeping.
#[repr(C, align(8))]
pub struct Rw {
    prw: [u8; 64],
    writer: *mut c_void,
    readers: c_int,
}

const RUMPUSER_RW_WRITER: c_int = 1;

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_init(rwp: *mut *mut Rw) {
    tr!(b"rw_init");
    let rw = malloc(core::mem::size_of::<Rw>()) as *mut Rw;
    pthread_rwlock_init((*rw).prw.as_mut_ptr() as *mut c_void, ptr::null());
    (*rw).writer = ptr::null_mut();
    (*rw).readers = 0;
    *rwp = rw;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_enter(enum_: c_int, rw: *mut Rw) {
    tr!(b"rw_enter");
    let p = (*rw).prw.as_mut_ptr() as *mut c_void;
    // Release the rump CPU only when the lock is contended (see mutex_enter): the
    // holder may be another lwp that needs the single rump CPU to release it.
    if enum_ == RUMPUSER_RW_WRITER {
        if pthread_rwlock_trywrlock(p) != 0 {
            let nlocks = rumpkern_unsched(ptr::null_mut());
            pthread_rwlock_wrlock(p);
            rumpkern_sched(nlocks, ptr::null_mut());
        }
        (*rw).writer = rumpuser_curlwp();
    } else {
        if pthread_rwlock_tryrdlock(p) != 0 {
            let nlocks = rumpkern_unsched(ptr::null_mut());
            pthread_rwlock_rdlock(p);
            rumpkern_sched(nlocks, ptr::null_mut());
        }
        (*rw).readers += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_tryenter(enum_: c_int, rw: *mut Rw) -> c_int {
    tr!(b"rw_tryenter");
    let p = (*rw).prw.as_mut_ptr() as *mut c_void;
    if enum_ == RUMPUSER_RW_WRITER {
        if pthread_rwlock_trywrlock(p) == 0 {
            (*rw).writer = rumpuser_curlwp();
            0
        } else {
            EBUSY
        }
    } else if pthread_rwlock_tryrdlock(p) == 0 {
        (*rw).readers += 1;
        0
    } else {
        EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_tryupgrade(_rw: *mut Rw) -> c_int {
    tr!(b"rw_tryupgrade");
    // pthread rwlock has no atomic upgrade; report failure so the caller retries
    // via drop+reacquire (rump handles EBUSY here).
    EBUSY
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_downgrade(rw: *mut Rw) {
    tr!(b"rw_downgrade");
    // pthread rwlocks can't downgrade in place — the lock stays exclusive (safe,
    // just no reader concurrency), but update bookkeeping so held()/asserts agree.
    (*rw).writer = ptr::null_mut();
    (*rw).readers += 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_exit(rw: *mut Rw) {
    tr!(b"rw_exit");
    if (*rw).writer == rumpuser_curlwp() && !(*rw).writer.is_null() {
        (*rw).writer = ptr::null_mut();
    } else if (*rw).readers > 0 {
        (*rw).readers -= 1;
    }
    pthread_rwlock_unlock((*rw).prw.as_mut_ptr() as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_destroy(rw: *mut Rw) {
    tr!(b"rw_destroy");
    pthread_rwlock_destroy((*rw).prw.as_mut_ptr() as *mut c_void);
    free(rw as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_rw_held(enum_: c_int, rw: *mut Rw, held: *mut c_int) {
    tr!(b"rw_held");
    *held = if enum_ == RUMPUSER_RW_WRITER {
        c_int::from((*rw).writer == rumpuser_curlwp() && !(*rw).writer.is_null())
    } else {
        c_int::from((*rw).readers > 0)
    };
}

// ── condvar ─────────────────────────────────────────────────────────────────

#[repr(C, align(8))]
pub struct Cv {
    pcv: [u8; 64],
    waiters: c_int,
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_init(cvp: *mut *mut Cv) {
    tr!(b"cv_init");
    let cv = malloc(core::mem::size_of::<Cv>()) as *mut Cv;
    pthread_cond_init((*cv).pcv.as_mut_ptr() as *mut c_void, ptr::null());
    (*cv).waiters = 0;
    *cvp = cv;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_destroy(cv: *mut Cv) {
    tr!(b"cv_destroy");
    pthread_cond_destroy((*cv).pcv.as_mut_ptr() as *mut c_void);
    free(cv as *mut c_void);
}

// A cv wait must release the rump CPU before sleeping (the lwp that will signal
// needs it — there is only one rump CPU), and reacquire on wake. The interlock
// mutex is handed to the scheduler so the CPU handoff and the mutex release are
// coordinated (avoids a lost-wakeup race). Mirrors NetBSD cv_unschedule/reschedule.
#[inline]
unsafe fn cv_unschedule(m: *mut Mtx) -> c_int {
    let nlocks = rumpkern_unsched(m as *mut c_void);
    mtxexit(m);
    nlocks
}

#[inline]
unsafe fn cv_reschedule(m: *mut Mtx, nlocks: c_int) {
    // If the interlock is a spin kmutex, pthread_cond_wait reacquired pthmtx on
    // return; to preserve lock-ordering vs. the rump CPU we must drop it, take the
    // CPU, then relock — otherwise we'd hold-and-wait and could deadlock. Plain
    // (non-spin) mutexes don't have this problem.
    if (*m).flags & (RUMPUSER_MTX_SPIN | RUMPUSER_MTX_KMUTEX)
        == (RUMPUSER_MTX_SPIN | RUMPUSER_MTX_KMUTEX)
    {
        pthread_mutex_unlock((*m).pm.as_mut_ptr() as *mut c_void);
        rumpkern_sched(nlocks, m as *mut c_void);
        rumpuser_mutex_enter_nowrap(m);
    } else {
        mtxenter(m);
        rumpkern_sched(nlocks, m as *mut c_void);
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait(cv: *mut Cv, m: *mut Mtx) {
    tr!(b"cv_wait");
    (*cv).waiters += 1;
    let nlocks = cv_unschedule(m);
    pthread_cond_wait((*cv).pcv.as_mut_ptr() as *mut c_void, (*m).pm.as_mut_ptr() as *mut c_void);
    cv_reschedule(m, nlocks);
    (*cv).waiters -= 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait_nowrap(cv: *mut Cv, m: *mut Mtx) {
    tr!(b"cv_wait_nowrap");
    // No CPU release: the caller is in a context where unscheduling is illegal.
    (*cv).waiters += 1;
    mtxexit(m);
    pthread_cond_wait((*cv).pcv.as_mut_ptr() as *mut c_void, (*m).pm.as_mut_ptr() as *mut c_void);
    mtxenter(m);
    (*cv).waiters -= 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_timedwait(cv: *mut Cv, m: *mut Mtx, sec: i64, nsec: i64) -> c_int {
    tr!(b"cv_timedwait");
    // rump passes a RELATIVE timeout (sec/nsec); pthread_cond_timedwait wants an
    // absolute CLOCK_REALTIME deadline. Sample the clock before unscheduling (we
    // may be parked a while after releasing the rump CPU). Matches NetBSD.
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_REALTIME, &mut ts);

    (*cv).waiters += 1;
    let nlocks = cv_unschedule(m);

    ts.tv_sec += sec;
    ts.tv_nsec += nsec as c_long;
    if ts.tv_nsec >= 1_000_000_000 {
        ts.tv_sec += 1;
        ts.tv_nsec -= 1_000_000_000;
    }
    let rv = pthread_cond_timedwait(
        (*cv).pcv.as_mut_ptr() as *mut c_void,
        (*m).pm.as_mut_ptr() as *mut c_void,
        &ts,
    );

    cv_reschedule(m, nlocks);
    (*cv).waiters -= 1;
    // ETIMEDOUT (110 on Linux/musl) → rump expects nonzero on timeout; else 0.
    if rv == 110 { rv } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_signal(cv: *mut Cv) {
    tr!(b"cv_signal");
    pthread_cond_signal((*cv).pcv.as_mut_ptr() as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_broadcast(cv: *mut Cv) {
    tr!(b"cv_broadcast");
    pthread_cond_broadcast((*cv).pcv.as_mut_ptr() as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_has_waiters(cv: *mut Cv, nwaiters: *mut c_int) {
    tr!(b"cv_has_waiters");
    *nwaiters = (*cv).waiters;
}

} // mod pthread_backend

// ── files / block I/O — STUBS (not needed to init the network stack) ─────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_open(_name: *const c_char, _mode: c_int, fdp: *mut c_int) -> c_int {
    tr!(b"open(STUB)");
    let _ = fdp;
    ENXIO
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_close(_fd: c_int) -> c_int {
    tr!(b"close(STUB)");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_getfileinfo(_name: *const c_char, _size: *mut u64, _ft: *mut c_int) -> c_int {
    tr!(b"getfileinfo(STUB)");
    ENXIO
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_bio(
    _fd: c_int,
    _op: c_int,
    _data: *mut c_void,
    _dlen: usize,
    _off: i64,
    _done: Option<extern "C" fn(*mut c_void, usize, c_int)>,
    _donearg: *mut c_void,
) {
    tr!(b"bio(STUB)");
    // No block device backing in the networking-only phase.
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_iovread(_fd: c_int, _iov: *mut c_void, _iovcnt: usize, _off: i64, _retv: *mut usize) -> c_int {
    tr!(b"iovread(STUB)");
    ENXIO
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_iovwrite(_fd: c_int, _iov: *const c_void, _iovcnt: usize, _off: i64, _retv: *mut usize) -> c_int {
    tr!(b"iovwrite(STUB)");
    ENXIO
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_syncfd(_fd: c_int, _flags: c_int, _start: u64, _len: u64) -> c_int {
    tr!(b"syncfd(STUB)");
    0
}

// ── dynloader / daemonize — STUBS ────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_dl_bootstrap(
    _modinit: *mut c_void,
    _symload: *mut c_void,
    _compload: *mut c_void,
) {
    tr!(b"dl_bootstrap(STUB)");
    // Components are linked statically (RUMP_COMPONENT ctors run via the linker),
    // so there is nothing to discover dynamically.
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_daemonize_begin() -> c_int {
    tr!(b"daemonize_begin(STUB)");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_daemonize_done(_error: c_int) -> c_int {
    tr!(b"daemonize_done(STUB)");
    0
}

// ── syscall proxy (sp_*) ──────────────────────────────────────────────────────
// The rumpuser_sp_* family is provided by the in-tree NetBSD sysproxy SERVER
// (`src-netbsd/lib/librumpuser/rumpuser_sp.c`, which #includes `sp_common.c`),
// compiled and linked alongside this staticlib (RUMP_SYSPROXY.md Step 1). The
// former ENOTSUP stubs were removed once that source links against our base
// hypercalls + the exported `rumpuser__hyp` global above. `rumpuser__errtrans`
// (also needed by the server) is supplied by NetBSD's `rumpuser_errtrans.c`.

// ── component (hypercall-backend ↔ rump scheduler bridge) ─────────────────────
//
// These are NOT part of the RUMPUSER_VERSION hypercall contract — they are the
// helper ABI a `rumpcomp_user` packet backend (e.g. libvirtif's `virtif_user.c`,
// or our own `/dev/net/tap0` glue) calls to step in and out of the rump CPU
// scheduler around blocking host I/O and its receive kthread. NetBSD provides
// them in `lib/librumpuser/rumpuser_component.c`; since our Rust rumpuser
// replaces that C layer, we must provide them too — verbatim ports.
//
// `rumpkern_{un,}sched` (rumpuser_int.h) are thin inlines over the backend
// schedule upcalls in the hyp table, so we inline them here directly.

// Release / re-acquire the single rump-kernel CPU. `interlock` is the rump mutex
// the scheduler coordinates the handoff with (NULL for the plain backend wrap);
// it is compared by pointer against the scheduler's own CPU mutex inside the rump
// kernel — pass the cv's interlock mutex in the cv paths, NULL otherwise.
#[inline]
unsafe fn rumpkern_unsched(interlock: *mut c_void) -> c_int {
    let mut nlocks: c_int = 0;
    ((*HYPERUP).hyp_backend_unschedule.unwrap())(0, &mut nlocks, interlock);
    nlocks
}

#[inline]
unsafe fn rumpkern_sched(nlocks: c_int, interlock: *mut c_void) {
    ((*HYPERUP).hyp_backend_schedule.unwrap())(nlocks, interlock);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_unschedule() -> *mut c_void {
    tr!(b"component_unschedule");
    rumpkern_unsched(ptr::null_mut()) as isize as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_schedule(cookie: *mut c_void) {
    tr!(b"component_schedule");
    rumpkern_sched(cookie as isize as c_int, ptr::null_mut());
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_kthread() {
    tr!(b"component_kthread");
    ((*HYPERUP).hyp_schedule.unwrap())();
    ((*HYPERUP).hyp_lwproc_newlwp.unwrap())(0);
    ((*HYPERUP).hyp_unschedule.unwrap())();
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_curlwp() -> *mut c_void {
    tr!(b"component_curlwp");
    ((*HYPERUP).hyp_schedule.unwrap())();
    let l = ((*HYPERUP).hyp_lwproc_curlwp.unwrap())();
    ((*HYPERUP).hyp_unschedule.unwrap())();
    l
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_switchlwp(l: *mut c_void) {
    tr!(b"component_switchlwp");
    ((*HYPERUP).hyp_schedule.unwrap())();
    ((*HYPERUP).hyp_lwproc_switch.unwrap())(l);
    ((*HYPERUP).hyp_unschedule.unwrap())();
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_kthread_release() {
    tr!(b"component_kthread_release");
    ((*HYPERUP).hyp_schedule.unwrap())();
    ((*HYPERUP).hyp_lwproc_release.unwrap())();
    ((*HYPERUP).hyp_unschedule.unwrap())();
}

/// Translate a host errno to a rump-kernel errno. NetBSD's librumpuser maps
/// Linux→NetBSD here; on aarch64-linux-musl the low errno values we care about
/// on backend error paths line up, so identity is correct enough for bring-up.
/// (Revisit with a real table if a backend starts surfacing high errnos.)
#[no_mangle]
pub unsafe extern "C" fn rumpuser_component_errtrans(hosterr: c_int) -> c_int {
    tr!(b"component_errtrans");
    hosterr
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Borrow a NUL-terminated C string as bytes (without the NUL).
unsafe fn cstr<'a>(s: *const c_char) -> &'a [u8] {
    if s.is_null() {
        return &[];
    }
    core::slice::from_raw_parts(s, strlen(s))
}

/// Write a diagnostic to stderr (fd 2) without going through C variadics.
unsafe fn dprint(msg: &[u8]) {
    write(2, msg.as_ptr() as *const c_void, msg.len());
}

/// TEMP debug: trace one hypercall as "[<tid> #<seq>] ru:<name>\n" in a SINGLE
/// write() so concurrent threads can't tear the line (one write to a pipe is
/// atomic up to PIPE_BUF). The tid (low 32 bits of pthread_self) lets us follow a
/// single lwp's flow, and the global seq orders events across threads — exactly
/// what's needed to see which thread parks on a cv and which one should signal it.
#[cfg(feature = "rumpuser_debug")]
unsafe fn trace(name: &[u8]) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tid = pthread_self() as usize as u32;

    let mut buf = [0u8; 128];
    let mut n = 0usize;
    let mut put = |b: u8| {
        if n < buf.len() {
            buf[n] = b;
            n += 1;
        }
    };
    // "[" <tid:08x> " #" <seq> "] ru:" <name> "\n"
    put(b'[');
    for i in 0..8 {
        let nib = ((tid >> ((7 - i) * 4)) & 0xf) as u8;
        put(if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) });
    }
    put(b' ');
    put(b'#');
    // seq in decimal (small, human-orderable)
    if seq == 0 {
        put(b'0');
    } else {
        let mut tmp = [0u8; 20];
        let mut t = 0;
        let mut v = seq;
        while v > 0 {
            tmp[t] = b'0' + (v % 10) as u8;
            t += 1;
            v /= 10;
        }
        while t > 0 {
            t -= 1;
            put(tmp[t]);
        }
    }
    put(b']');
    put(b' ');
    put(b'r');
    put(b'u');
    put(b':');
    for &c in name {
        put(c);
    }
    put(b'\n');
    write(2, buf.as_ptr() as *const c_void, n);
}

/// TEMP debug: print "tag 0xA 0xB 0xC\n" without variadics/alloc.
#[cfg(feature = "rumpuser_debug")]
unsafe fn dbg3(tag: &[u8], a: usize, b: usize, c: usize) {
    dprint(tag);
    dprint_hex(a);
    dprint(b" ");
    dprint_hex(b);
    dprint(b" ");
    dprint_hex(c);
    dprint(b"\n");
}

#[cfg(feature = "rumpuser_debug")]
unsafe fn dprint_hex(v: usize) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nib = ((v >> ((15 - i) * 4)) & 0xf) as u8;
        buf[2 + i] = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
    }
    dprint(&buf);
}
