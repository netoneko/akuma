//! `execve`, every part of it that is not the loader.
//!
//! - [`read_request`] (before the load): the path, resolved against the
//!   process's `cwd`, and argv/envp read out of user memory under an
//!   `akuma_syscalls_proc::exec::Budget`.
//! - [`run`]: the whole call for the Akuma kernels (AArch64 and amd64) —
//!   execute permission, the `ETXTBSY` pin, `#!` scripts, stopping the other
//!   threads, the shared-mapping write-back, [`commit`], the `vfork` wake —
//!   around an [`ExecArch`] that reads, loads, installs and enters the image.
//! - [`commit`] (after the install, before entering the new image): the
//!   process's `/proc` identity, the close-on-exec sweep, and caught signal
//!   handlers back to the default. Taoxichuan calls it directly; its loader
//!   and process model are its own.
//!
//! The two Akuma kernels each had the whole of `execve` until 2026-11-12, and
//! the copies had drifted (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`,
//! "One execve"). What still differs is what [`ExecArch`] carries: AArch64 reads
//! a small binary whole and demand-pages a large one, then `eret`s from inside
//! the syscall; amd64 reads a head, maps every segment as it loads, and returns
//! into the new image through its syscall exit path.

use alloc::string::String;
use alloc::vec::Vec;

use akuma_syscalls_proc::exec::Budget;

const EFAULT: u64 = (-14i64) as u64;
const ENOENT: u64 = (-2i64) as u64;
const E2BIG: u64 = (-7i64) as u64;
const ENAMETOOLONG: u64 = (-36i64) as u64;
/// Linux's `PATH_MAX`.
const PATH_MAX: usize = 4096;

/// One `execve`'s inputs, read out of ring 3. Strings are bytes: Linux puts no
/// encoding on argv or envp, and a kernel whose loader needs `&str` checks that
/// itself.
pub struct Request {
    /// Resolved against the caller's `cwd`.
    pub path: String,
    /// `argv`; a NULL or empty `argv` becomes `[path as given]` (Linux would
    /// start the program with `argc == 0`, which musl programs do not expect).
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
}

/// Read `execve(path, argv, envp)`'s arguments.
///
/// `Err` is the syscall return:
/// `EFAULT` for an unreadable pointer, `ENOENT` for an empty path, `E2BIG` when
/// the block is over `budget` or a vector over `max_argv`/`max_envp` entries.
pub fn read_request(
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    max_argv: usize,
    max_envp: usize,
    budget: &mut Budget,
) -> Result<Request, u64> {
    let raw = read_cstr(path_ptr, PATH_MAX).map_err(|e| if e == E2BIG { ENAMETOOLONG } else { e })?;
    if raw.is_empty() {
        return Err(ENOENT);
    }
    let raw = String::from_utf8(raw).map_err(|_| ENOENT)?;
    let path = crate::fs::resolve_path_at(AT_FDCWD, &raw).map_err(|_| ENOENT)?;
    let mut argv = read_vector(argv_ptr, max_argv, budget)?;
    if argv.is_empty() {
        argv.push(raw.into_bytes());
    }
    let envp = read_vector(envp_ptr, max_envp, budget)?;
    Ok(Request { path, argv, envp })
}

const AT_FDCWD: i32 = -100;

/// A NULL-terminated array of string pointers: every string, or the whole call
/// refused. Never a shorter list.
fn read_vector(ptr: u64, max: usize, budget: &mut Budget) -> Result<Vec<Vec<u8>>, u64> {
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    for i in 0..=max {
        let mut p = 0u64;
        crate::read_user_into(&mut p, ptr + (i as u64) * 8).map_err(|_| EFAULT)?;
        if p == 0 {
            return Ok(out);
        }
        if i == max {
            return Err(E2BIG);
        }
        let s = read_cstr(p, akuma_config::MAX_ARG_STRLEN)?;
        budget.charge(s.len()).map_err(|_| E2BIG)?;
        out.push(s);
    }
    Ok(out)
}

/// One C string of at most `max` bytes (without the NUL), page by page so a
/// string that ends just before an unmapped page reads cleanly. `E2BIG` when
/// there is no NUL within `max`.
fn read_cstr(ptr: u64, max: usize) -> Result<Vec<u8>, u64> {
    let mut out = Vec::new();
    let mut at = ptr;
    let mut buf = [0u8; 256];
    loop {
        let remaining = max + 1 - out.len();
        let want = akuma_exec::process::user_access::cstr_chunk_len(at, remaining, buf.len());
        crate::copy_from_user(&mut buf[..want], at).map_err(|_| EFAULT)?;
        if let Some(nul) = buf[..want].iter().position(|&b| b == 0) {
            out.extend_from_slice(&buf[..nul]);
            return Ok(out);
        }
        out.extend_from_slice(&buf[..want]);
        if out.len() > max {
            return Err(E2BIG);
        }
        at += want as u64;
    }
}

/// The process side of a successful exec.
///
/// After the kernel's loader has built the new image and before it enters it:
/// `/proc/<pid>/exe`, `comm` and
/// `cmdline` name the new program (musl expands a RUNPATH's `$ORIGIN` through
/// `/proc/self/exe`), close-on-exec descriptors are released, and caught
/// handlers go back to the default (their addresses named the old image;
/// ignored signals stay ignored).
pub fn commit(proc: &akuma_exec::process::Process, exe: &str, argv: &[Vec<u8>]) {
    {
        let mut img = proc.image.lock();
        img.exe = String::from(exe);
        img.name = String::from(exe.rsplit('/').next().unwrap_or(exe));
        img.args = argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    }
    proc.close_cloexec_fds_releasing();
    for a in proc.signal_actions.actions.lock().iter_mut() {
        if matches!(a.handler, akuma_exec::process::SignalHandler::UserFn(_)) {
            *a = akuma_exec::process::SignalAction::default();
        }
    }
}

/// What `execve` does differently per kernel.
///
/// [`run`] calls these in order: `read`, then (for an ELF image) `load`, `stop_siblings`, and inside
/// `with_target`, `install` and `enter`. Everything before `stop_siblings` may
/// fail and leaves the caller's image untouched.
pub trait ExecArch {
    /// What [`Self::read`] produced: enough of the file to see a `#!` line.
    type Image;
    /// A loaded program, not yet installed.
    type Loaded;

    /// Read the executable at `path` (symlinks already resolved). `Err` is the
    /// syscall's errno.
    fn read(&self, path: &str) -> Result<Self::Image, u64>;
    /// The file's first bytes, for the `#!` check; empty when the kernel does
    /// not have them (AArch64's path load of a file it could not read whole).
    fn head<'a>(&self, image: &'a Self::Image) -> &'a [u8];
    /// Build the new image and its initial stack. Consumes `image`, so a
    /// whole-file buffer is freed here, before an `eret` abandons this frame.
    fn load(&self, image: Self::Image, path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Self::Loaded, u64>;
    /// POSIX: every other thread of the group dies. Past the point of no return.
    fn stop_siblings(&self);
    /// Run `f` on the process the new image goes to; `ESRCH` without one.
    fn with_target(&self, f: impl FnOnce(&akuma_exec::process::Process) -> u64) -> u64;
    /// Put `loaded` on `proc`, dropping the old image.
    fn install(&self, proc: &akuma_exec::process::Process, loaded: Self::Loaded) -> Result<(), u64>;
    /// Start the installed image. AArch64's never returns; amd64's arms its
    /// syscall return and returns 0. Nothing in [`run`]'s frames owns heap by
    /// the time this is called, because a call that does not return leaks it.
    fn enter(&self, proc: &akuma_exec::process::Process) -> u64;
}

/// `execve` on an Akuma kernel, after [`read_request`]. Returns only on failure,
/// or with 0 where [`ExecArch::enter`] returns.
pub fn run<A: ExecArch>(arch: &A, req: Request) -> u64 {
    let Request { path, argv, envp } = req;
    // The file is read, permission-checked, pinned and reported as
    // `/proc/<pid>/exe` through its symlinks; `path` itself is what a script's
    // interpreter is handed.
    let resolved = akuma_vfs_glue::resolve_symlinks(&path);
    // Execute permission (`EACCES`). Root passes on any file with some x bit;
    // see `akuma_isolation::dac::may`. Set-user-ID bits are never honoured.
    if let Err(e) = crate::perm::check(&resolved, akuma_isolation::dac::MAY_EXEC) {
        return e;
    }
    // Mark the file as being executed BEFORE its bytes are read, so it cannot
    // be rewritten between the read and the mark (`ETXTBSY`,
    // `docs/archive/ETXTBSY_TEXT_BUSY.md`). It reaches `Process::set_text` or
    // is dropped before `enter`.
    let Ok(text) = akuma_vfs_glue::text_pin(&resolved) else {
        return ENOMEM;
    };
    let image = match arch.read(&resolved) {
        Ok(i) => i,
        Err(e) => return e,
    };

    let head = arch.head(&image);
    if head.len() >= 2 && head[0] == b'#' && head[1] == b'!' {
        // One parser and one argv rule with `spawn`, host-tested in
        // `akuma-exec`'s `shebang_tests`.
        let line = &head[..head.len().min(akuma_exec::process::SHEBANG_MAX)];
        let Some((interpreter, arg)) = akuma_exec::process::parse_shebang(line) else {
            return ENOENT;
        };
        // `interp_argv0` is the interpreter as written, and is argv[0]: busybox
        // dispatches on it, so the symlink-resolved name would lose a
        // `#!/bin/sh`'s identity (`docs/archive/DEVBOX_ISSUES.md` Issue 14).
        let mut hop: Vec<Vec<u8>> = akuma_exec::process::shebang_hop(interpreter, arg, &path, &[])
            .into_iter()
            .map(String::into_bytes)
            .collect();
        let interp = String::from(interpreter);
        // The recursion may not return (AArch64 `eret`s), so nothing large may
        // still be owned here: the script, its pin and the old argv go first.
        drop(image);
        drop(text);
        hop.extend(argv.into_iter().skip(1));
        drop(path);
        drop(resolved);
        return run(arch, Request { path: interp, argv: hop, envp });
    }

    let loaded = match arch.load(image, &resolved, &argv, &envp) {
        Ok(l) => l,
        Err(e) => return e,
    };
    drop(envp);
    arch.stop_siblings();
    // The old image's writable `MAP_SHARED` file mappings die with it: written
    // back and released from the shared table now, while its address space is
    // still the current one — the install's teardown would do neither.
    if let Some(p) = crate::mem::current_owner() {
        crate::mem::release_shared_write_mappings(p);
    }
    arch.with_target(move |proc| {
        if let Err(e) = arch.install(proc, loaded) {
            return e;
        }
        // The new image's executable is busy for as long as it runs (the old
        // image's mark is released).
        proc.set_text(text);
        // The point of no return: close-on-exec descriptors close only now, so
        // a failed exec (every return above) leaves the fd table as it was.
        commit(proc, &resolved, &argv);
        drop(argv);
        drop(resolved);
        drop(path);
        // A `CLONE_VFORK` parent waits for exactly this.
        crate::proc::vfork_complete(proc.pid);
        arch.enter(proc)
    })
}

const ENOMEM: u64 = (-12i64) as u64;
