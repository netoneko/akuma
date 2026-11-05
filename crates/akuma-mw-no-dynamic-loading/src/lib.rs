#![no_std]
#![forbid(unsafe_code)]
//! "No dynamic loading" as a middleware crate (B22; guide: `akuma-tokio-kernel-core/docs/middleware.md` § Example 2). It bans
//! **dynamically linked programs** and any way of turning a file into code, and it is written once, as one `match`, because the
//! kernel asks one question - "may this process get new code?" - at every place code can arrive:
//!
//! | how code arrives | the event | what [`exec_policy`] does |
//! |---|---|---|
//! | `execve`, init, a `#!` interpreter | `ExecKind::Image` (asked before anything is torn down) | refuse an image with a `PT_INTERP`; a static image is allowed |
//! | `mmap` with `PROT_EXEC` | `ExecKind::Map` | refuse a file-backed mapping; anonymous memory is allowed (W^X already forbids the writable kind) |
//! | `mprotect` adding `PROT_EXEC` | `ExecKind::Protect` | refuse (the kernel cannot say which file a present page came from) |
//!
//! Besides the policy there is [`exec_guard`], an ordinary `kernel_pre` hook on `execve` that refuses an `LD_*` variable in the new
//! environment: the guide's Example 2, kept because it is the clearest example of reading the caller's memory.
//!
//! **How a kernel uses it.** There is one middleware table per kernel and nothing chains, so this crate is not a table: it is two
//! functions the kernel's table names (`Hooks { exec: Some(akuma_mw_no_dynamic_loading::exec_policy), kernel_pre: &[Route::new(
//! &[On::Call(Syscall::Execve)], akuma_mw_no_dynamic_loading::exec_guard)], .. }`). A kernel that composes several such crates
//! writes the composition itself, in its own table, where it can be read. It takes no boot parameters and keeps no state.
//!
//! What gets around it is listed in the guide, and Shanghai's probe (`shanghai/mwprobe`) tries each avenue it knows.

use akuma_tokio_kernel_core::middleware::{ExecEvent, ExecKind, ExecVerdict, KernelCtx, UserError, Verdict};

const EPERM: i32 = 1;
const E2BIG: i32 = 7;
const EACCES: i32 = 13;
/// More entries than any real environment; past it the call is refused (fail closed).
const MAX_ENV: usize = 512;

/// The exec policy: one `match` over every way the kernel introduces code.
#[must_use]
pub fn exec_policy(ev: &ExecEvent) -> ExecVerdict {
    match ev.kind {
        // Kept for reference: a kernel built with the elf crate's `refuse-dynamic` (Shanghai's `no-dynamic-loading` turns it on) never
        // asks this arm, because the elf crate refuses a dynamic image first, with the same EACCES. It stays so the policy is complete on
        // its own (a kernel without that feature relies on it) and so the `Image` event has a worked example of a verdict that reads
        // the interpreter.
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
#[must_use]
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

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::vec::Vec;

    use akuma_tokio_kernel_core::middleware::{Machine, Syscall};

    use super::*;

    const MEM_LEN: usize = 0x3000;

    /// A fake address space: little-endian pointers and strings at fixed addresses, nothing beyond `MEM_LEN`.
    fn mem() -> &'static [u8] {
        static MEM: OnceLock<Vec<u8>> = OnceLock::new();
        MEM.get_or_init(|| {
            let mut m = std::vec![0u8; MEM_LEN];
            let put_str = |m: &mut Vec<u8>, at: usize, s: &str| m[at..at + s.len()].copy_from_slice(s.as_bytes());
            let put_ptrs = |m: &mut Vec<u8>, at: usize, ps: &[u64]| {
                for (i, p) in ps.iter().enumerate() {
                    m[at + i * 8..at + i * 8 + 8].copy_from_slice(&p.to_le_bytes());
                }
            };
            put_str(&mut m, 0x400, "PATH=/bin");
            put_str(&mut m, 0x420, "LD_PRELOAD=/x.so");
            put_str(&mut m, 0x440, "LD"); // two bytes then NUL: not `LD_`
            put_str(&mut m, 0x460, "HOME=/root");
            put_ptrs(&mut m, 0x100, &[0x400, 0]); // clean
            put_ptrs(&mut m, 0x120, &[0x400, 0x420, 0]); // LD_ on the second entry
            put_ptrs(&mut m, 0x140, &[0x9000_0000, 0]); // a pointer that does not translate
            put_ptrs(&mut m, 0x160, &[0x440, 0x460, 0]); // `LD` and `HOME`: neither starts with LD_
            // 513 entries, none NULL-terminated within the budget: refused as too long.
            put_ptrs(&mut m, 0x1000, &[0x400; 513]);
            m
        })
    }

    fn machine() -> Machine {
        Machine {
            decode: Syscall::from_aarch64,
            encode: Syscall::to_aarch64,
            copy_in: |dst, src| {
                let m = mem();
                let Some(end) = (src as usize).checked_add(dst.len()).filter(|&e| e <= m.len()) else { return false };
                dst.copy_from_slice(&m[src as usize..end]);
                true
            },
            copy_out: |_, _| false,
            pid: || 7,
        }
    }

    fn guard(envp: u64) -> Verdict {
        let nr = (machine().encode)(Syscall::Execve);
        exec_guard(&KernelCtx::for_test(nr, [0, 0, envp, 0, 0, 0], machine()))
    }

    fn ev(kind: ExecKind<'_>) -> ExecVerdict {
        exec_policy(&ExecEvent { pid: 7, kind })
    }

    #[test]
    fn a_dynamic_image_is_refused_and_a_static_one_is_not() {
        let dynamic = ExecKind::Image { path: "/bin/busybox", interp: Some("/lib/ld-musl-x86_64.so.1"), image: &[], sidecar: None };
        assert_eq!(ev(dynamic), ExecVerdict::Deny(EACCES));
        assert_eq!(ev(ExecKind::Image { path: "/bin/mwprobe", interp: None, image: &[], sidecar: None }), ExecVerdict::Allow);
    }

    #[test]
    fn only_a_file_backed_executable_mapping_is_refused() {
        assert_eq!(ev(ExecKind::Map { inode: Some(12) }), ExecVerdict::Deny(EACCES));
        assert_eq!(ev(ExecKind::Map { inode: None }), ExecVerdict::Allow, "anonymous memory: W^X already forbids the writable kind");
    }

    #[test]
    fn mprotect_adding_exec_is_refused() {
        assert_eq!(ev(ExecKind::Protect), ExecVerdict::Deny(EACCES));
    }

    #[test]
    fn the_guard_passes_a_clean_environment_and_a_null_one() {
        assert_eq!(guard(0x100), Verdict::Pass);
        assert_eq!(guard(0), Verdict::Pass, "envp may be NULL: an empty environment");
        assert_eq!(guard(0x160), Verdict::Pass, "`LD` and `HOME` do not start with `LD_`");
    }

    #[test]
    fn the_guard_refuses_ld_anywhere_in_the_list() {
        assert_eq!(guard(0x120), Verdict::Errno(EPERM));
    }

    #[test]
    fn the_guard_fails_closed_on_what_it_cannot_read_or_bound() {
        // Unknown is not allowed: a pointer that does not translate is an error, not a pass.
        assert!(matches!(guard(0x140), Verdict::Errno(e) if e != 0));
        // So is an envp pointer itself outside memory, and a list longer than any real environment.
        assert!(matches!(guard(0x9000_0000), Verdict::Errno(e) if e != 0));
        assert_eq!(guard(0x1000), Verdict::Errno(E2BIG));
    }
}
