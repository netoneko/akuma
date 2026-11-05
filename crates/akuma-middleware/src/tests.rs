use crate::*;
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

calls! { enum Guard { Getpid, Clone, Execve } }
calls! { enum Serve { Hello = 1000, Pid = 1001, Getpid } }

const CLONE: u64 = 56;
const EXECVE: u64 = 59;

#[test]
fn a_call_nobody_hooked_runs_no_hook() {
    static MW: Middleware = Middleware::new(Hooks::new());
    MW.arm(machine());
    let a = [1, 2, 3, 4, 5, 6];
    assert!(!MW.kernel_wants(GETPID));
    assert_eq!(MW.run_kernel(GETPID, a, 1, || 99), 99);
}

#[test]
fn only_the_calls_in_the_enum_reach_the_hook() {
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    static MW: Middleware<Guard, NoCalls> = Middleware::new(Hooks {
        pre: Some(|_, which| {
            SEEN.fetch_add(1, Relaxed);
            match which {
                Guard::Getpid => Verdict::Errno(1),
                Guard::Clone | Guard::Execve => Verdict::Pass,
            }
        }),
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    // getpid is in the enum: answered, and the call does not run.
    let ran = AtomicUsize::new(0);
    let r = MW.run_kernel(GETPID, a, 1, || {
        ran.fetch_add(1, Relaxed);
        5
    });
    assert_eq!(r.cast_signed(), -1);
    assert_eq!(ran.load(Relaxed), 0);
    assert_eq!(SEEN.load(Relaxed), 1);
    // openat is not: no mask bit, the hook never runs.
    assert!(!MW.kernel_wants(257));
    assert_eq!(MW.run_kernel(257, a, 1, || 3), 3);
    assert_eq!(SEEN.load(Relaxed), 1);
}

#[test]
fn handler_serves_only_what_would_be_enosys() {
    static MW: Middleware<NoCalls, Serve> = Middleware::new(Hooks {
        handler: Some(|ctx, which| match which {
            Serve::Hello => Some(42),
            Serve::Pid => Some(u64::from(ctx.pid)),
            Serve::Getpid => Some(7),
        }),
        ..Hooks::new()
    });
    MW.arm(machine());
    let a = [0; 6];
    assert_eq!(MW.run_kernel(1000, a, 1, || ENOSYS_RET), 42);
    assert_eq!(MW.run_kernel(1001, a, 1, || ENOSYS_RET), 7);
    // getpid is served by the kernel, so its answer stands; the handler only fills a gap.
    assert_eq!(MW.run_kernel(GETPID, a, 1, || 1234), 1234);
    assert_eq!(MW.run_kernel(GETPID, a, 1, || ENOSYS_RET), 7);
    // A number in no enum stays -ENOSYS.
    assert_eq!(MW.run_kernel(1002, a, 1, || ENOSYS_RET), ENOSYS_RET);
}

#[test]
fn pre_refuses_execve_but_not_clone() {
    static MW: Middleware<Guard, NoCalls> = Middleware::new(Hooks {
        pre: Some(|c, which| match which {
            Guard::Execve if c.args[0] == 1 => Verdict::Errno(1),
            Guard::Clone => Verdict::Return(0),
            Guard::Execve | Guard::Getpid => Verdict::Pass,
        }),
        ..Hooks::new()
    });
    MW.arm(machine());
    let ran = AtomicUsize::new(0);
    let r = MW.run_kernel(EXECVE, [1, 0, 0, 0, 0, 0], 1, || {
        ran.fetch_add(1, Relaxed);
        0
    });
    assert_eq!(r.cast_signed(), -1);
    assert_eq!(ran.load(Relaxed), 0);
    // Not refused: it runs.
    assert_eq!(MW.run_kernel(EXECVE, [0; 6], 1, || 7), 7);
    // clone cannot be refused before it runs: the Return is ignored.
    assert_eq!(MW.run_kernel(CLONE, [0; 6], 1, || 77), 77);
}

#[test]
fn pass_and_forget_clears_the_bit() {
    static MW: Middleware<Guard, NoCalls> = Middleware::new(Hooks {
        pre: Some(|_, _| Verdict::PassAndForget),
        ..Hooks::new()
    });
    MW.arm(machine());
    assert!(MW.kernel_wants(GETPID));
    MW.run_kernel(GETPID, [0; 6], 1, || 1);
    assert!(!MW.kernel_wants(GETPID));
}

#[test]
fn custom_numbers_are_always_asked() {
    static MW: Middleware<NoCalls, Serve> = Middleware::new(Hooks::new());
    MW.arm(machine());
    assert!(MW.kernel_wants(CUSTOM));
    assert!(!MW.kernel_wants(GETPID));
}

#[test]
fn calls_know_their_numbers_and_variants() {
    assert_eq!(Guard::ALL.len(), 3);
    assert_eq!(Serve::ALL[0], On::Nr(1000));
    assert_eq!(Serve::from_call(None, 1001), Some(Serve::Pid));
    assert_eq!(Serve::from_call(Some(Syscall::Getpid), GETPID), Some(Serve::Getpid));
    assert_eq!(Serve::from_call(None, 1002), None);
    assert_eq!(Guard::from_call(Some(Syscall::Openat), 257), None);
}

#[test]
fn the_compile_time_rules() {
    use crate::calls::{check_calls, servable};
    // `check_calls` is a const fn; these are the rules it enforces, shown as values (the failing cases are `should_panic` below).
    check_calls(&[On::Call(Syscall::Getpid), On::Nr(CUSTOM_BASE), On::Nr(CUSTOM_BASE + 1)]);
    assert!(servable(Serve::ALL));
    assert!(!servable(Guard::ALL));
    assert!(servable(NoCalls::ALL));
}

#[test]
#[should_panic(expected = "named twice in one enum")]
fn a_call_named_twice_in_one_enum_is_refused() {
    crate::calls::check_calls(&[On::Call(Syscall::Getpid), On::Call(Syscall::Getpid)]);
}

#[test]
#[should_panic(expected = "below CUSTOM_BASE")]
fn a_custom_number_below_the_base_is_refused() {
    crate::calls::check_calls(&[On::Nr(39)]);
}

#[test]
#[should_panic(expected = "named twice in `pre`")]
fn a_linux_call_and_a_custom_number_that_meet_on_one_machine_are_refused_at_arm_time() {
    // The one case the types cannot see: `Getpid` and `Nr(1000)` only meet once a machine says what `Getpid` is.
    #[derive(Clone, Copy)]
    enum Mixed {
        A,
        B,
    }
    impl Calls for Mixed {
        const ALL: &'static [On] = &[On::Call(Syscall::Getpid), On::Nr(CUSTOM_BASE)];
        const VARIANTS: &'static [Self] = &[Self::A, Self::B];
    }
    static MW: Middleware<Mixed, NoCalls> = Middleware::new(Hooks::new());
    // A machine that numbers getpid 1000.
    MW.arm(Machine { encode: |_| CUSTOM_BASE, ..machine() });
}

#[test]
fn the_object_safe_face_runs_a_table() {
    static MW: Middleware<NoCalls, Serve> = Middleware::new(Hooks {
        handler: Some(|_, which| match which {
            Serve::Hello => Some(42),
            Serve::Pid | Serve::Getpid => None,
        }),
        ..Hooks::new()
    });
    MW.arm(machine());
    let face: &'static dyn Dispatch = &MW;
    assert_eq!(face.run_kernel(CUSTOM_BASE, [0; 6], 1, &mut || ENOSYS_RET), 42);
    assert_eq!(face.run_kernel(39, [0; 6], 1, &mut || 5), 5);
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
