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

#![no_std]
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]
#![allow(static_mut_refs)]

use core::ffi::{c_char, c_int, c_long, c_void};
use core::ptr;

/// Panic = abort (no unwinding in a freestanding syscall-glue staticlib).
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
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn posix_memalign(memptr: *mut *mut c_void, align: usize, size: usize) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn clock_gettime(clk: c_int, ts: *mut Timespec) -> c_int;
    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn abort() -> !;
    fn _exit(code: c_int) -> !;
    fn strlen(s: *const c_char) -> usize;
    fn getenv(name: *const c_char) -> *const c_char;
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn __errno_location() -> *mut c_int;
    fn getrandom(buf: *mut c_void, buflen: usize, flags: u32) -> isize;

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

type PthreadT = *mut c_void; // musl pthread_t is pointer-sized
type PthreadKey = u32; // musl pthread_key_t is unsigned int

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: c_long,
}

// errno values used here
const ENOMEM: c_int = 12;
const EBUSY: c_int = 16;
const ENXIO: c_int = 6;
const ENOTSUP: c_int = 95; // Linux/musl EOPNOTSUPP/ENOTSUP

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
struct RumpHyperUp {
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
/// pthread TLS key holding the current lwp pointer (per host thread).
static mut CURLWP_KEY: PthreadKey = 0;

#[no_mangle]
pub unsafe extern "C" fn rumpuser_init(version: c_int, hyp: *const RumpHyperUp) -> c_int {
    tr!(b"init");
    const RUMPUSER_VERSION: c_int = 17;
    if version != RUMPUSER_VERSION {
        dprint(b"rumpuser_init: version mismatch\n");
        return 1;
    }
    HYPERUP = hyp;
    // Allocate the curlwp TLS key once (init is single-threaded).
    if pthread_key_create(&mut CURLWP_KEY, ptr::null()) != 0 {
        return ENOMEM;
    }
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

#[no_mangle]
pub unsafe extern "C" fn rumpuser_clock_sleep(enum_: c_int, sec: i64, nsec: c_long) -> c_int {
    tr!(b"clock_sleep");
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
            return 0;
        }
        Timespec { tv_sec: s, tv_nsec: n }
    } else {
        Timespec { tv_sec: sec, tv_nsec: nsec }
    };
    // hyp_unschedule/schedule around the block is the proper behaviour; for the
    // stub phase we just sleep (single rump CPU during init).
    nanosleep(&req, ptr::null_mut());
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

#[repr(C, align(8))]
struct Mtx {
    pm: [u8; 64],
    owner: *mut c_void,
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_init(mtxp: *mut *mut Mtx, _flags: c_int) {
    tr!(b"mutex_init");
    let m = malloc(core::mem::size_of::<Mtx>()) as *mut Mtx;
    pthread_mutex_init((*m).pm.as_mut_ptr() as *mut c_void, ptr::null());
    (*m).owner = ptr::null_mut();
    *mtxp = m;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_enter(m: *mut Mtx) {
    tr!(b"mutex_enter");
    pthread_mutex_lock((*m).pm.as_mut_ptr() as *mut c_void);
    (*m).owner = rumpuser_curlwp();
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_enter_nowrap(m: *mut Mtx) {
    tr!(b"mutex_enter_nowrap");
    pthread_mutex_lock((*m).pm.as_mut_ptr() as *mut c_void);
    (*m).owner = rumpuser_curlwp();
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_tryenter(m: *mut Mtx) -> c_int {
    tr!(b"mutex_tryenter");
    if pthread_mutex_trylock((*m).pm.as_mut_ptr() as *mut c_void) == 0 {
        (*m).owner = rumpuser_curlwp();
        0
    } else {
        EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_mutex_exit(m: *mut Mtx) {
    tr!(b"mutex_exit");
    (*m).owner = ptr::null_mut();
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
struct Rw {
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
    if enum_ == RUMPUSER_RW_WRITER {
        pthread_rwlock_wrlock(p);
        (*rw).writer = rumpuser_curlwp();
    } else {
        pthread_rwlock_rdlock(p);
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
struct Cv {
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

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait(cv: *mut Cv, m: *mut Mtx) {
    tr!(b"cv_wait");
    (*cv).waiters += 1;
    (*m).owner = ptr::null_mut();
    pthread_cond_wait((*cv).pcv.as_mut_ptr() as *mut c_void, (*m).pm.as_mut_ptr() as *mut c_void);
    (*m).owner = rumpuser_curlwp();
    (*cv).waiters -= 1;
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_wait_nowrap(cv: *mut Cv, m: *mut Mtx) {
    tr!(b"cv_wait_nowrap");
    rumpuser_cv_wait(cv, m);
}

#[no_mangle]
pub unsafe extern "C" fn rumpuser_cv_timedwait(cv: *mut Cv, m: *mut Mtx, sec: i64, nsec: i64) -> c_int {
    tr!(b"cv_timedwait");
    // rump passes an ABSOLUTE CLOCK_MONOTONIC deadline (sec/nsec).
    let abstime = Timespec { tv_sec: sec, tv_nsec: nsec as c_long };
    (*cv).waiters += 1;
    (*m).owner = ptr::null_mut();
    let rv = pthread_cond_timedwait(
        (*cv).pcv.as_mut_ptr() as *mut c_void,
        (*m).pm.as_mut_ptr() as *mut c_void,
        &abstime,
    );
    (*m).owner = rumpuser_curlwp();
    (*cv).waiters -= 1;
    // ETIMEDOUT (110 on Linux/musl) → rump expects a nonzero return on timeout.
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

// ── syscall proxy (sp_*) — STUBS (only used with the sysproxy faction) ────────

#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_init(_url: *const c_char, _a: *const c_char, _b: *const c_char, _c: *const c_char) -> c_int {
    tr!(b"sp_init(STUB)");
    ENOTSUP
}
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_copyin(_arg: *mut c_void, _raddr: *const c_void, _laddr: *mut c_void, _len: usize) -> c_int { tr!(b"sp_copyin(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_copyinstr(_arg: *mut c_void, _raddr: *const c_void, _laddr: *mut c_void, _len: *mut usize) -> c_int { tr!(b"sp_copyinstr(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_copyout(_arg: *mut c_void, _laddr: *const c_void, _raddr: *mut c_void, _len: usize) -> c_int { tr!(b"sp_copyout(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_copyoutstr(_arg: *mut c_void, _laddr: *const c_void, _raddr: *mut c_void, _len: *mut usize) -> c_int { tr!(b"sp_copyoutstr(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_anonmmap(_arg: *mut c_void, _howmuch: usize, _addr: *mut *mut c_void) -> c_int { tr!(b"sp_anonmmap(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_raise(_arg: *mut c_void, _signo: c_int) -> c_int { tr!(b"sp_raise(STUB)"); ENOTSUP }
#[no_mangle]
pub unsafe extern "C" fn rumpuser_sp_fini(_arg: *mut c_void) { tr!(b"sp_fini(STUB)"); }

// ── helpers ─────────────────────────────────────────────────────────────────

/// Borrow a NUL-terminated C string as bytes (without the NUL).
unsafe fn cstr<'a>(s: *const c_char) -> &'a [u8] {
    if s.is_null() {
        return &[];
    }
    core::slice::from_raw_parts(s as *const u8, strlen(s))
}

/// Write a diagnostic to stderr (fd 2) without going through C variadics.
unsafe fn dprint(msg: &[u8]) {
    write(2, msg.as_ptr() as *const c_void, msg.len());
}

/// TEMP debug: trace a hypercall name ("ru:<name>\n") to stderr.
#[cfg(feature = "rumpuser_debug")]
unsafe fn trace(name: &[u8]) {
    dprint(b"ru:");
    dprint(name);
    dprint(b"\n");
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
