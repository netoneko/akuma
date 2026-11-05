#![forbid(unsafe_code)]
//! "No dynamic loading": a policy demo, compiled in only with `--features no-dynamic-loading` (off by default; it implies `middleware`)
//! (B22, guide: `crates/akuma-tokio-kernel-core/docs/middleware.md` § Example 2). It bans **dynamically linked programs** and any
//! way of turning a file into code, and it is written once, as one `match`, because the kernel asks one question - "may this process
//! get new code?" - at every place code can arrive:
//!
//! | how code arrives | the event | what this policy does |
//! |---|---|---|
//! | `execve`, init, a `#!` interpreter | `ExecKind::Image` (asked before anything is torn down) | refuse an image with a `PT_INTERP`; a static image is allowed |
//! | `mmap` with `PROT_EXEC` | `ExecKind::Map` | refuse a file-backed mapping; anonymous memory is allowed (W^X already forbids the writable kind) |
//! | `mprotect` adding `PROT_EXEC` | `ExecKind::Protect` | refuse (the kernel cannot say which file a present page came from) |
//!
//! Besides the policy there is [`exec_guard`], an ordinary `kernel_pre` hook on `execve` that refuses an `LD_*` variable in the new
//! environment: the guide's Example 2, kept because it is the clearest example of reading the caller's memory.
//!
//! What gets around it is listed in the guide, and the probe (`shanghai/mwprobe`) tries each avenue it knows.

use akuma_tokio_kernel::middleware::{ExecEvent, ExecKind, ExecVerdict, KernelCtx, UserError, Verdict};

const EPERM: i32 = 1;
const E2BIG: i32 = 7;
const EACCES: i32 = 13;
/// More entries than any real environment; past it the call is refused (fail closed).
const MAX_ENV: usize = 512;

/// The exec policy: one `match` over every way the kernel introduces code.
pub fn exec_policy(ev: &ExecEvent) -> ExecVerdict {
    match ev.kind {
        ExecKind::Image { path, interp: Some(interp), .. } => {
            akuma_primitives::safe_print!(160, "[nodl] pid {} refused a dynamic executable {} (interpreter {})\n", ev.pid, path, interp);
            ExecVerdict::Deny(EACCES)
        }
        ExecKind::Image { interp: None, .. } | ExecKind::Map { inode: None } => ExecVerdict::Allow,
        ExecKind::Map { inode: Some(_) } => {
            akuma_primitives::safe_print!(96, "[nodl] pid {} refused a file-backed PROT_EXEC mmap\n", ev.pid);
            ExecVerdict::Deny(EACCES)
        }
        ExecKind::Protect => {
            akuma_primitives::safe_print!(96, "[nodl] pid {} refused mprotect(PROT_EXEC)\n", ev.pid);
            ExecVerdict::Deny(EACCES)
        }
    }
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
