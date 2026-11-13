//! `execve`'s two halves that are not the loader, shared by the x86 kernels.
//!
//! amd64 and Taoxichuan each load ELF images into their own address spaces, so
//! the loader stays in each kernel. What surrounds it was written twice and had
//! drifted (amd64 let a bad `argv` pointer end the list where Taoxichuan answered
//! `EFAULT`; they charged a string differently against the stack):
//!
//! - [`read_request`] (before the load): the path, resolved against the
//!   process's `cwd`, and argv/envp read out of ring 3 under an
//!   `akuma_syscalls_proc::exec::Budget`.
//! - [`commit`] (after the load, before entering the new image): the process's
//!   `/proc` identity, the close-on-exec sweep, and caught signal handlers back
//!   to the default.

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
