#![forbid(unsafe_code)]
//! "No dynamic loading": a policy demo built from three `kernel_pre` hooks, compiled in only with `--features
//! no-dynamic-loading` (off by default; it implies `middleware`) (B22, guide:
//! `crates/akuma-tokio-kernel-core/docs/middleware.md` § Worked example). It is a demo, not a sandbox: it is **not** a
//! wholesale ban, and the guide lists what gets around it.
//!
//! What dynamic loading is, to a kernel: `ld-musl` (the interpreter the kernel's loader maps for a `PT_INTERP`
//! binary) maps each shared library from a file with `PROT_EXEC`, and `dlopen` does the same later. So:
//!
//! - [`mmap_guard`]: no file-backed `PROT_EXEC` mapping. Anonymous executable memory is untouched (W^X already
//!   forbids the writable kind), so a static binary, which maps no library, is unaffected.
//! - [`mprotect_guard`]: no adding `PROT_EXEC` after the fact, which is how a file mapped read-only would otherwise
//!   be turned executable. A stricter rule than the mmap one: from `mprotect`'s arguments alone, file-backed and
//!   anonymous cannot be told apart.
//! - [`exec_guard`]: refuse an `execve` whose environment names an `LD_*` variable (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
//!   `LD_AUDIT`, ...), the knobs the dynamic loader reads from the environment. Refusing the whole call is the only
//!   thing the hook API can do here: it cannot strip one variable and let the call proceed.

use akuma_tokio_kernel::middleware::{KernelCtx, UserError, Verdict};

const PROT_EXEC: u64 = 4;
const MAP_ANONYMOUS: u64 = 0x20;
const EPERM: i32 = 1;
const E2BIG: i32 = 7;
const EACCES: i32 = 13;
/// More entries than any real environment; past it the call is refused (fail closed).
const MAX_ENV: usize = 512;

/// `mmap(addr, len, prot, flags, fd, off)`: refuse a file-backed mapping that is executable.
pub fn mmap_guard(ctx: &KernelCtx) -> Verdict {
    let (prot, flags) = (ctx.args[2], ctx.args[3]);
    if prot & PROT_EXEC != 0 && flags & MAP_ANONYMOUS == 0 {
        akuma_primitives::safe_print!(96, "[nodl] pid {} refused a file-backed PROT_EXEC mmap\n", ctx.pid);
        return Verdict::Errno(EACCES);
    }
    Verdict::Pass
}

/// `mprotect(addr, len, prot)`: refuse making anything executable.
pub fn mprotect_guard(ctx: &KernelCtx) -> Verdict {
    if ctx.args[2] & PROT_EXEC != 0 {
        akuma_primitives::safe_print!(96, "[nodl] pid {} refused mprotect(PROT_EXEC)\n", ctx.pid);
        return Verdict::Errno(EACCES);
    }
    Verdict::Pass
}

/// Whether the NUL-terminated string at `s` starts with `LD_`. Bytewise and bounded to three bytes, so a long
/// string costs nothing and a short one at the end of a page does not fault.
fn starts_with_ld(ctx: &KernelCtx, s: u64) -> Result<bool, UserError> {
    let mut b = [0u8; 1];
    for (i, want) in (0u64..).zip(*b"LD_") {
        ctx.user_read(s + i, &mut b)?;
        if b[0] != want {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `execve(path, argv, envp)`: refuse a call whose `envp` has an `LD_*` entry. `envp` may be NULL (an empty
/// environment). A bad pointer or an over-long list is refused too: unknown is not allowed.
pub fn exec_guard(ctx: &KernelCtx) -> Verdict {
    let mut slot = ctx.args[2];
    if slot == 0 {
        return Verdict::Pass;
    }
    for _ in 0..MAX_ENV {
        let s = match ctx.user_read_u64(slot) {
            Ok(0) => return Verdict::Pass, // the NULL that ends the array
            Ok(s) => s,
            Err(e) => return Verdict::Errno(e.errno()),
        };
        match starts_with_ld(ctx, s) {
            Ok(false) => {}
            Ok(true) => {
                akuma_primitives::safe_print!(96, "[nodl] pid {} refused an execve with an LD_* variable\n", ctx.pid);
                return Verdict::Errno(EPERM);
            }
            Err(e) => return Verdict::Errno(e.errno()),
        }
        slot += 8;
    }
    Verdict::Errno(E2BIG)
}
