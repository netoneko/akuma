use super::*;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

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
const CUSTOM: u64 = CUSTOM_BASE;

#[test]
fn a_call_nobody_hooked_runs_no_hook() {
    static MW: Middleware = Middleware::new(Hooks::new());
    MW.arm(machine());
    let a = [1, 2, 3, 4, 5, 6];
    assert!(!MW.kernel_wants(GETPID));
    assert_eq!(MW.run_kernel(GETPID, a, 1, || 99), 99);
}

#[test]
fn pre_can_answer_and_the_call_does_not_run() {
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::Errno(1))],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    let ran = AtomicUsize::new(0);
    let r = MW.run_kernel(GETPID, a, 1, || {
        ran.fetch_add(1, Relaxed);
        5
    });
    assert_eq!(r.cast_signed(), -1);
    assert_eq!(ran.load(Relaxed), 0);
}

#[test]
fn fallback_serves_only_what_would_be_enosys() {
    static MW: Middleware = Middleware::new(Hooks {
        fallback: &[Route::new(&[On::Nr(CUSTOM), On::Call(Syscall::Getpid)], |_| Some(42))],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert_eq!(MW.run_kernel(CUSTOM, a, 1, || ENOSYS_RET), 42);
    // getpid is served, so its answer stands.
    assert_eq!(MW.run_kernel(GETPID, a, 1, || 1234), 1234);
}

#[test]
fn exit_and_clone_are_observed_not_replaced() {
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[Route::new(&[On::Call(Syscall::Clone)], |_| Verdict::Return(0))],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert_eq!(MW.run_kernel(56, a, 1, || 77), 77);
}

#[test]
fn execve_can_be_refused_before_it_runs_but_not_served_or_rewritten() {
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[Route::new(&[On::Call(Syscall::Execve)], |c| {
            if c.args[0] == 1 { Verdict::Errno(1) } else { Verdict::Pass }
        })],
        // This may not take effect on an observe-only call.
        fallback: &[Route::new(&[On::Call(Syscall::Execve)], |_| Some(99))],
        ..Hooks::new()
    });
    MW.arm(machine());
    let ran = AtomicUsize::new(0);
    // A refusal is honoured: EPERM, and the call never ran.
    let a = [1, 0, 0, 0, 0, 0];
    let r = MW.run_kernel(59, a, 1, || {
        ran.fetch_add(1, Relaxed);
        0
    });
    assert_eq!(r.cast_signed(), -1);
    assert_eq!(ran.load(Relaxed), 0);
    // Not refused: it runs, its -ENOSYS is not turned into 99 by the fallback, 
    let a = [0; 6];
    assert_eq!(MW.run_kernel(59, a, 1, || ENOSYS_RET), ENOSYS_RET);
    assert_eq!(MW.run_kernel(59, a, 1, || 7), 7);
}

#[test]
fn exit_and_rt_sigreturn_cannot_be_refused() {
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[
            Route::new(&[On::Call(Syscall::ExitGroup), On::Call(Syscall::Exit)], |_| Verdict::Errno(1)),
            Route::new(&[On::Call(Syscall::RtSigreturn)], |_| Verdict::Return(0)),
        ],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert_eq!(MW.run_kernel(231, a, 1, || 5), 5); // exit_group
    assert_eq!(MW.run_kernel(60, a, 1, || 5), 5); // exit
    assert_eq!(MW.run_kernel(15, a, 1, || 5), 5); // rt_sigreturn
}

#[test]
fn pass_and_forget_clears_the_bit() {
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::PassAndForget)],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert!(MW.kernel_wants(GETPID));
    MW.run_kernel(GETPID, a, 1, || 1);
    assert!(!MW.kernel_wants(GETPID));
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
    static MW: Middleware = Middleware::new(Hooks {
        kernel_pre: &[
            Route::new(&[On::Call(Syscall::Getpid)], |_| Verdict::Pass),
            Route::new(&[On::Nr(GETPID)], |_| Verdict::Pass),
        ],
        ..Hooks::new()
    });
    MW.arm(machine());
}

/// `docs/middleware.md` § "Your first syscall": the table as written there.
#[test]
fn the_guides_first_syscall_compiles_and_answers() {
    static MW: Middleware = Middleware::new(Hooks {
        fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_ctx| Some(42))],
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert_eq!(MW.run_kernel(CUSTOM_BASE, a, 1, || ENOSYS_RET), 42);
    // And as `Kernel::middleware` takes it: behind the object-safe face.
    let face: &'static dyn Dispatch = &MW;
    assert_eq!(face.run_kernel(39, a, 1, &mut || 5), 5);
}

#[test]
fn the_exec_policy_sees_every_way_code_arrives_and_is_allow_by_default() {
    static NONE: Middleware = Middleware::new(Hooks::new());
    static MW: Middleware = Middleware::new(Hooks {
        exec: Some(|ev| match ev.kind {
            ExecKind::Image { interp: Some(_), .. } => ExecVerdict::Deny(13),
            ExecKind::Map { inode: Some(_) } | ExecKind::Protect => ExecVerdict::Deny(1),
            _ => ExecVerdict::Allow,
        }),
        ..Hooks::new()
    });
    NONE.arm(machine());
    MW.arm(machine());
    let img = |interp| ExecKind::Image { path: "/bin/x", interp, image: &[], sidecar: None };
    assert_eq!(NONE.exec_event(&img(Some("/lib/ld.so"))), ExecVerdict::Allow);
    assert_eq!(NONE.exec_event(&ExecKind::Protect), ExecVerdict::Allow);
    assert_eq!(MW.exec_event(&img(Some("/lib/ld.so"))), ExecVerdict::Deny(13));
    assert_eq!(MW.exec_event(&img(None)), ExecVerdict::Allow);
    assert_eq!(MW.exec_event(&ExecKind::Map { inode: Some(7) }), ExecVerdict::Deny(1));
    assert_eq!(MW.exec_event(&ExecKind::Map { inode: None }), ExecVerdict::Allow);
    assert_eq!(MW.exec_event(&ExecKind::Protect), ExecVerdict::Deny(1));
}

#[test]
fn a_policy_can_ask_for_the_image_to_be_the_only_source_and_for_a_sidecar() {
    static MW: Middleware = Middleware::new(Hooks {
        exec: Some(|ev| match ev.kind {
            // Allowed only if the sidecar says so, and then mapped from the checked bytes.
            ExecKind::Image { sidecar: Some(b"ok"), .. } => ExecVerdict::AllowFromImage,
            ExecKind::Image { .. } => ExecVerdict::Deny(13),
            _ => ExecVerdict::Allow,
        }),
        exec_sidecar: Some(".sig"),
        ..Hooks::new()
    });
    MW.arm(machine());
    assert_eq!(MW.hooks.exec_sidecar, Some(".sig"));
    let img = |sidecar| ExecKind::Image { path: "/bin/x", interp: None, image: &[1, 2, 3], sidecar };
    assert_eq!(MW.exec_event(&img(Some(b"ok"))), ExecVerdict::AllowFromImage);
    assert_eq!(MW.exec_event(&img(Some(b"no"))), ExecVerdict::Deny(13));
    assert_eq!(MW.exec_event(&img(None)), ExecVerdict::Deny(13));
    // For a mapping or a protection change `AllowFromImage` is not a thing a policy needs to say.
    assert_eq!(MW.exec_event(&ExecKind::Protect), ExecVerdict::Allow);
}

#[test]
fn numbers_from_512_are_always_asked() {
    static MW: Middleware = Middleware::new(Hooks::new());
    MW.arm(machine());
    assert!(MW.kernel_wants(CUSTOM));
    assert!(!MW.kernel_wants(GETPID));
}
