//! Signal frames on AArch64: the machine half of
//! `akuma_tokio_kernel_core::signal`.
//!
//! A handler is entered with `x0` = the signal, `x1`/`x2` = the `siginfo` and
//! `ucontext` (`SA_SIGINFO`, else 0), `x30` = the action's restorer (musl's
//! `__restore_rt`: `mov x8, #139; svc #0`) and `sp` at the saved block. The
//! block is the whole trapped register file (the vector stubs' 832-byte
//! `Frame`, FP/SIMD included) plus the mask to restore; its layout is private
//! to [`A64::push_handler`] and [`sigreturn`]. As on the amd64 half, the
//! `ucontext` is zeros: a handler that edits `uc_mcontext` changes nothing.

use akuma_exec::process::SignalAction;
use akuma_syscalls_linux::nr;
use akuma_tokio_kernel_core::signal::{self, Deliver};
use akuma_tokio_kernel_core::task::USER_TOP;
use akuma_vectors_aarch64::Frame;

use crate::mm::Aarch64;

/// Calls Linux never restarts after a handler, `SA_RESTART` or not: the sleeps
/// and the waits with a timeout (asm-generic numbers).
pub const NO_RESTART: [u64; 8] = [
    nr::NANOSLEEP,
    nr::CLOCK_NANOSLEEP,
    nr::RT_SIGSUSPEND,
    nr::RT_SIGTIMEDWAIT,
    nr::PSELECT6,
    nr::PPOLL,
    nr::EPOLL_PWAIT,
    441, // epoll_pwait2
];

const FRAME_BYTES: usize = core::mem::size_of::<Frame>();
/// The saved block: the frame, then the mask `rt_sigreturn` restores, padded to 16.
const BLOCK_BYTES: usize = FRAME_BYTES + 16;
const SIGINFO_BYTES: u64 = 128;
/// `sizeof(ucontext_t)` on AArch64 (its `mcontext` carries a 4 KiB reserved area).
const UCONTEXT_BYTES: u64 = 4560;

/// `PSTATE` bits a returning program may choose: the condition flags. The rest
/// (exception level, `DAIF`, single step) are the kernel's: EL0t, interrupts on.
const SPSR_USER: u64 = 0xF000_0000;
/// The architected `FPCR` and `FPSR` bits.
const FPCR_BITS: u64 = 0x07FF_9F00;
const FPSR_BITS: u64 = 0xF800_009F;

/// The trapped frame and the `x0` the call came in with (a restart puts it back).
pub struct A64<'a> {
    pub f: &'a mut Frame,
    pub x0: u64,
    /// A synchronous fault being delivered: `(si_addr, si_code)`.
    pub fault: Option<(u64, i32)>,
}

/// `uc_sigmask` and `uc_mcontext` offsets in the AArch64 `ucontext_t`; the mcontext is
/// `fault_address`, `regs[31]`, `sp`, `pc`, `pstate`. HotSpot's SIGSEGV handler edits `pc`.
const UC_SIGMASK: usize = 40;
const UC_MCTX: usize = 176;
/// Bytes of the ucontext that carry state (through `pstate`); the rest is zeros.
const UC_HEAD: usize = UC_MCTX + 8 * 35;

fn frame_bytes(f: &Frame) -> [u8; FRAME_BYTES] {
    let mut out = [0u8; FRAME_BYTES];
    let mut w = 0;
    let mut put = |v: &[u8]| {
        out[w..w + v.len()].copy_from_slice(v);
        w += v.len();
    };
    for x in &f.x {
        put(&x.to_le_bytes());
    }
    for v in [f.sp, f.elr, f.spsr, f.tpidr, f.esr, f.far, 0] {
        put(&v.to_le_bytes());
    }
    for q in &f.q {
        put(&q.to_le_bytes());
    }
    put(&f.fpcr.to_le_bytes());
    put(&f.fpsr.to_le_bytes());
    out
}

fn word(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]))
}

fn zero_user(mut at: u64, len: u64) -> bool {
    let zeros = [0u8; 512];
    let end = at + len;
    while at < end {
        let n = (end - at).min(zeros.len() as u64);
        if akuma_user_access::copy_to_user(at, &zeros[..n as usize]).is_err() {
            return false;
        }
        at += n;
    }
    true
}

impl Deliver for A64<'_> {
    fn push_handler(
        &mut self,
        sig: u32,
        entry: u64,
        act: &SignalAction,
        result: u64,
        restart: Option<u64>,
        cur_mask: u64,
        restore_mask: u64,
    ) -> bool {
        if act.flags & signal::SA_RESTORER == 0 {
            return false;
        }
        let user_sp = self.f.sp;
        // Below a 128-byte gap: the siginfo, the ucontext, then the block, 16-aligned.
        let Some(info_at) = user_sp.checked_sub(128 + SIGINFO_BYTES).map(|v| v & !0xF) else {
            return false;
        };
        let Some(at) = info_at.checked_sub(UCONTEXT_BYTES + BLOCK_BYTES as u64).map(|v| v & !0xF) else {
            return false;
        };
        let uc_at = at + BLOCK_BYTES as u64;
        if user_sp > USER_TOP || at < 4096 {
            return false;
        }
        // What the interrupted code sees again after `rt_sigreturn`: its result, or
        // the call re-issued (`elr` back over the 4-byte `svc`, `x0` its argument).
        let mut saved = frame_bytes(self.f);
        let (x0, elr) = match restart {
            Some(_) => (self.x0, self.f.elr - 4),
            None => (result, self.f.elr),
        };
        saved[..8].copy_from_slice(&x0.to_le_bytes());
        saved[256..264].copy_from_slice(&elr.to_le_bytes());
        let mut tail = [0u8; BLOCK_BYTES - FRAME_BYTES];
        tail[..8].copy_from_slice(&restore_mask.to_le_bytes());
        let mut info = [0u8; SIGINFO_BYTES as usize];
        info[..4].copy_from_slice(&sig.cast_signed().to_le_bytes()); // si_signo; si_errno, si_code (SI_USER) 0
        if let Some((addr, code)) = self.fault {
            info[8..12].copy_from_slice(&code.to_le_bytes());
            info[16..24].copy_from_slice(&addr.to_le_bytes());
        }
        // The interrupted state, as Linux's `ucontext` carries it: a handler that edits it
        // (`pc` for a null-check stub) changes where `rt_sigreturn` resumes.
        let mut uc = [0u8; UC_HEAD];
        uc[UC_SIGMASK..UC_SIGMASK + 8].copy_from_slice(&cur_mask.to_le_bytes());
        let fa = self.fault.map_or(0, |(a, _)| a);
        uc[UC_MCTX..UC_MCTX + 8].copy_from_slice(&fa.to_le_bytes());
        for i in 0..34 {
            // regs[0..31], sp, pc, pstate are words 0..34 of the saved frame.
            uc[UC_MCTX + 8 + i * 8..UC_MCTX + 16 + i * 8].copy_from_slice(&saved[i * 8..i * 8 + 8]);
        }
        if akuma_user_access::copy_to_user(at, &saved).is_err()
            || akuma_user_access::copy_to_user(at + FRAME_BYTES as u64, &tail).is_err()
            || akuma_user_access::copy_to_user(uc_at, &uc).is_err()
            || !zero_user(uc_at + UC_HEAD as u64, UCONTEXT_BYTES - UC_HEAD as u64)
            || akuma_user_access::copy_to_user(info_at, &info).is_err()
        {
            return false;
        }
        akuma_exec::threading::set_thread_signal_mask(signal::mask_during(sig, act, cur_mask));
        let siginfo = act.flags & signal::SA_SIGINFO != 0;
        self.f.x[0] = u64::from(sig);
        self.f.x[1] = if siginfo { info_at } else { 0 };
        self.f.x[2] = if siginfo { uc_at } else { 0 };
        self.f.x[30] = act.restorer as u64;
        self.f.sp = at;
        self.f.elr = entry;
        true
    }
}

/// `rt_sigreturn` (139): the restorer's call after a handler returned, with
/// `sp` where [`A64::push_handler`] left the block. Puts the interrupted
/// register file back and answers its `x0`.
pub fn sigreturn(f: &mut Frame) -> u64 {
    const EFAULT: u64 = 0u64.wrapping_sub(14);
    let at = f.sp;
    let mut b = [0u8; BLOCK_BYTES];
    if at >= USER_TOP || akuma_user_access::copy_from_user(&mut b, at).is_err() {
        return EFAULT;
    }
    let (sp, elr) = (word(&b, 31), word(&b, 32));
    // Never trust a user-built frame with a kernel address or a privileged PSTATE.
    if sp >= USER_TOP || elr >= USER_TOP {
        return EFAULT;
    }
    for (i, x) in f.x.iter_mut().enumerate() {
        *x = word(&b, i);
    }
    f.sp = sp;
    f.elr = elr;
    f.spsr = word(&b, 33) & SPSR_USER;
    let q0 = 38 * 8;
    for (i, q) in f.q.iter_mut().enumerate() {
        *q = u128::from_le_bytes(b[q0 + i * 16..q0 + i * 16 + 16].try_into().unwrap_or([0; 16]));
    }
    f.fpcr = word(&b, 38 + 64) & FPCR_BITS;
    f.fpsr = word(&b, 39 + 64) & FPSR_BITS;
    akuma_exec::threading::set_thread_signal_mask(word(&b, FRAME_BYTES / 8));
    // The handler's edits to the ucontext win (Linux restores from it).
    let mut uc = [0u8; UC_HEAD];
    if akuma_user_access::copy_from_user(&mut uc, at + BLOCK_BYTES as u64).is_ok() {
        let g = |i: usize| word(&uc, (UC_MCTX / 8) + 1 + i);
        let (usp, upc) = (g(31), g(32));
        if usp < USER_TOP && upc < USER_TOP {
            for i in 0..31 {
                f.x[i] = g(i);
            }
            f.sp = usp;
            f.elr = upc;
            f.spsr = g(33) & SPSR_USER;
        }
    }
    f.x[0]
}

/// A synchronous fault at `far`: run the program's `SIGSEGV` handler, as HotSpot's implicit
/// null checks and safepoint polls need. `false` (nothing changed) = no handler, blocked,
/// or no room: the caller kills the task as before.
pub fn deliver_fault(f: &mut Frame, far: u64, code: i32) -> bool {
    use akuma_exec::process::SignalHandler;
    const SIGSEGV: u32 = 11;
    let idx = SIGSEGV as usize - 1;
    let Some(p) = akuma_exec::process::current_process_shared() else {
        return false;
    };
    let act = p.signal_actions.actions.lock()[idx];
    let SignalHandler::UserFn(entry) = act.handler else {
        return false;
    };
    let mask = akuma_exec::threading::thread_signal_mask();
    if mask & (1u64 << idx) != 0 {
        return false;
    }
    let x0 = f.x[0];
    A64 { f, x0, fault: Some((far, code)) }.push_handler(SIGSEGV, entry as u64, &act, x0, None, mask, mask)
}

/// The syscall return: pending signals, after the result is in `x0`.
pub fn at_return(f: &mut Frame, n: u64, x0: u64) {
    let r = f.x[0];
    let _ = signal::deliver_at_return::<Aarch64, _>(&mut A64 { f, x0, fault: None }, n, r, &NO_RESTART);
}
