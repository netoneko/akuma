#![forbid(unsafe_code)]
//! Shanghai's sample syscall middleware (B22; guide: `crates/akuma-tokio-kernel-core/docs/middleware.md`). Built only with
//! `--features middleware`; the default Shanghai has none of it.
//!
//! One table, three tiers, each demonstrated by one entry. `mwprobe` (`shanghai/mwprobe/mwprobe.rs`) is the
//! guest-side check and `shanghai/mw_gate.sh` boots it.
//!
//! | what | tier | call |
//! |---|---|---|
//! | `hello`: answers 42 | `fallback` | 1000 |
//! | `whoami`: writes the caller's pid to `*arg0` (or `-EFAULT`) | `fallback` | 1001 |
//! | how many `getpid`s the `own` tier saw before / after the lock | `fallback` | 1002 / 1003 |
//! | refuse `open("/mw-deny")` with `EACCES`; log any other `/mw-*` path | `kernel_pre` | `openat` |
//! | with `--features no-dynamic-loading` only: no dynamic loading (file `PROT_EXEC` mmap, `mprotect(+X)`, `LD_*` in an `execve` env), see the `akuma-mw-no-dynamic-loading` crate | `kernel_pre` | `mmap` / `mprotect` / `execve` |
//! | count `getpid`, outside the kernel lock (a split read-modify-write on the state, so a lost update shows) | `own_pre` / `own_post` | `getpid` |
//! | publish the `own` state when a `getppid` passes; read it back | `own_post` / `fallback` | `getppid` / 1005 |

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use akuma_primitives::OnceCopy;
use akuma_tokio_kernel::middleware::{CUSTOM_BASE, Hooks, Middleware, On, ParamSource, Params, Route, Syscall, Verdict};

/// `getpid`s the `own_pre` hook saw.
static GETPID_PRE: AtomicU64 = AtomicU64::new(0);
/// `getpid`s the `own_post` hook saw (the `own` state counts the same thing, under its autolock).
static GETPID_POST: AtomicU64 = AtomicU64::new(0);

const EACCES: i32 = 13;
const EFAULT_RET: u64 = (-14i64) as u64;

/// The `own` state as of the last `getppid`, for the probe to compare with [`GETPID_PRE`].
static STATE_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

/// An extra path to refuse, from the command line (`mw.sample.also=/path`), set once by [`init`]. Unset: none.
static ALSO: OnceCopy<&'static str> = OnceCopy::new();
/// What [`init`] saw, for the probe (call 1006): bit 0 `mw.sample.also` was readable, bit 1 the kernel's own `init=` was
/// readable through the middleware's view (it must not be), bit 2 `mw.other.secret` was readable through the `mw.sample.` view (it must
/// not be; the gate's line carries it).
static PARAMS_SEEN: AtomicU64 = AtomicU64::new(0);

/// [`Hooks::init`]: read this middleware's parameters, once, at boot. The view is already narrowed to `mw.`; this table narrows it
/// again to `mw.sample.`, which is all it reads.
fn init(params: &Params<'static>, _state: &mut u64) {
    let mine = params.scoped("sample.");
    let mut seen = 0;
    if let Some(path) = mine.value("also") {
        ALSO.set(path);
        seen |= 1;
    }
    // Both of these must be invisible: the kernel's words are outside `mw.`, and `other.` is another crate's namespace.
    seen |= u64::from(params.get("init").is_some()) << 1;
    seen |= u64::from(mine.get("secret").is_some()) << 2;
    PARAMS_SEEN.store(seen, Relaxed);
    #[cfg(feature = "signed-exec")]
    {
        let cfg = akuma_mw_signed_exec::Config::from_params(&params.scoped("signed."));
        akuma_primitives::safe_print!(160, "[signed] allow={} keys={} malformed={}\n", cfg.allowed().count(), cfg.keys().count(), cfg.malformed);
        SIGNED.set(cfg);
    }
    akuma_primitives::safe_print!(128, "[mw] params: also={:?} seen={:#x}\n", ALSO.get(), seen);
}

/// `openat(dirfd, path, ..)`: `path` is argument 1.

fn open_guard(ctx: &akuma_tokio_kernel::middleware::KernelCtx) -> Verdict {
    let mut buf = [0u8; 64];
    let Ok(path) = ctx.user_str(ctx.args[1], &mut buf) else {
        return Verdict::Pass; // not ours to judge: the kernel will say what is wrong with it
    };
    if ALSO.get() == Some(path) {
        akuma_primitives::safe_print!(128, "[mw] pid {} openat {} (refused by mw.sample.also)\n", ctx.pid, path);
        return Verdict::Errno(EACCES);
    }
    if !path.starts_with("/mw-") {
        return Verdict::Pass;
    }
    akuma_primitives::safe_print!(128, "[mw] pid {} openat {}\n", ctx.pid, path);
    if path == "/mw-deny" { Verdict::Errno(EACCES) } else { Verdict::Pass }
}

/// What `mw.signed.*` said, read once by [`init`]. Unset (init did not run): [`akuma_mw_signed_exec::Config::EMPTY`], so nothing runs.
#[cfg(feature = "signed-exec")]
static SIGNED: OnceCopy<akuma_mw_signed_exec::Config<'static>> = OnceCopy::new();

/// The kernel's one exec policy: the features' policies in a row, composed here, where it can be read. A refusal from any of them
/// wins; otherwise the strongest allow does (`AllowFromImage` over `Allow`).
#[cfg(any(feature = "no-dynamic-loading", feature = "signed-exec"))]
fn exec_policy(ev: &akuma_tokio_kernel::middleware::ExecEvent) -> akuma_tokio_kernel::middleware::ExecVerdict {
    #[cfg(any(feature = "no-dynamic-loading", not(feature = "signed-exec")))]
    use akuma_tokio_kernel::middleware::ExecVerdict as V;
    #[cfg(feature = "no-dynamic-loading")]
    if let V::Deny(no) = akuma_mw_no_dynamic_loading::exec_policy(ev) {
        return V::Deny(no);
    }
    #[cfg(feature = "signed-exec")]
    return akuma_mw_signed_exec::exec_policy(&SIGNED.get().unwrap_or(akuma_mw_signed_exec::Config::EMPTY), ev);
    #[cfg(not(feature = "signed-exec"))]
    V::Allow
}

/// The exec policy: this table's own, composed from the features that are on; none without one (everything is allowed).
#[cfg(any(feature = "no-dynamic-loading", feature = "signed-exec"))]
const EXEC: Option<akuma_tokio_kernel::middleware::ExecPolicy> = Some(exec_policy);
#[cfg(not(any(feature = "no-dynamic-loading", feature = "signed-exec")))]
const EXEC: Option<akuma_tokio_kernel::middleware::ExecPolicy> = None;
/// The loader also reads `<path>.sig` when the signing policy is on.
#[cfg(feature = "signed-exec")]
const SIDECAR: Option<&str> = Some(".sig");
#[cfg(not(feature = "signed-exec"))]
const SIDECAR: Option<&str> = None;

/// The table the kernel registers.
pub static MW: Middleware<u64> = Middleware::new(0, Hooks {
    fallback: &[
        Route::new(&[On::Nr(CUSTOM_BASE)], |_| Some(42)),
        Route::new(&[On::Nr(CUSTOM_BASE + 1)], |ctx| {
            Some(match ctx.user_write_u64(ctx.args[0], u64::from(ctx.pid)) {
                Ok(()) => 0,
                Err(_) => EFAULT_RET,
            })
        }),
        Route::new(&[On::Nr(CUSTOM_BASE + 2)], |_| Some(GETPID_PRE.load(Relaxed))),
        Route::new(&[On::Nr(CUSTOM_BASE + 3)], |_| Some(GETPID_POST.load(Relaxed))),
        Route::new(&[On::Nr(CUSTOM_BASE + 5)], |_| Some(STATE_SNAPSHOT.load(Relaxed))),
        Route::new(&[On::Nr(CUSTOM_BASE + 6)], |_| Some(PARAMS_SEEN.load(Relaxed))),
    ],
    kernel_pre: &[
        Route::new(&[On::Call(Syscall::Openat)], open_guard),
        // The "no dynamic loading" demo (the `akuma-mw-no-dynamic-loading` crate), only with `--features no-dynamic-loading`: an env guard on execve here, and
        // the exec policy (`EXEC` below): one `match` over every way code arrives.
        #[cfg(feature = "no-dynamic-loading")]
        Route::new(&[On::Call(Syscall::Execve)], akuma_mw_no_dynamic_loading::exec_guard),
    ],
    own_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |ctx| {
        // Deliberately not `+= 1`: with the load and the store apart, two cores in here at once would lose an
        // update. The autolock is what makes `state` equal the atomic count below.
        let v = *ctx.state;
        core::hint::spin_loop();
        *ctx.state = v + 1;
        GETPID_PRE.fetch_add(1, Relaxed);
    })],
    own_post: &[
        Route::new(&[On::Call(Syscall::Getpid)], |_, _| {
            GETPID_POST.fetch_add(1, Relaxed);
        }),
        Route::new(&[On::Call(Syscall::Getppid)], |ctx, _| STATE_SNAPSHOT.store(*ctx.state, Relaxed)),
    ],
    exec: EXEC,
    init: Some(init),
    exec_sidecar: SIDECAR,
});
