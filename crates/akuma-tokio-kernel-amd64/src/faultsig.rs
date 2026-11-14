//! Synchronous fault -> signal delivery (PHASE-3-JAVAC, G13), behind the
//! off-by-default `fault-signal` feature (R6).
//!
//! The datum this serves: HotSpot compiles **implicit null checks** — a `mov`
//! through a NULL object — and safepoint polls, both relying on the kernel
//! delivering SIGSEGV to the JVM's own `SA_SIGINFO` handler, which reads
//! `siginfo` (`si_code`, `si_addr`) and `ucontext` (`uc_mcontext.gregs[]`),
//! patches the saved `%rip` to its exception stub, and returns. On the
//! unfeatured kernel a ring-3 fault that lazy-fill / stack-grow / COW cannot
//! resolve goes straight to `task killed` (`fault_trap_locked`), so javac dies
//! (`mov 0x8(%rdx),%r10d`, rdx=0, in the JIT code cache) instead of raising
//! NullPointerException.
//!
//! **The design is a 1:1 port of the metal kernel's proven path**
//! (`crates/akuma-kernel-amd64/src/signal.rs`: `deliver_fault_signal` /
//! `build_frame` / `install_handler_frame` — the frame Chromium, Go's
//! `sigpanic`, V8's wasm traps and Rust's stack-overflow handler were validated
//! against on metal; R9 precedent: `docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`).
//! Two deliberate divergences, both recorded in
//! `denver/docs/research/PHASE-3-JAVAC.md`:
//!
//! 1. **The `SigContext` here is the Linux/glibc ABI field order** (`rsp` at
//!    greg 11 = byte 88, `bx` 96, `dx` 104, `ax` 112, `cx` 120, `rip` 128) —
//!    the layout musl HotSpot indexes through the `REG_*` constants. The metal
//!    struct carries `rbx, rdx, rax, rcx, rsp` in those slots (its `rip`/
//!    `eflags`/`fpstate` offsets happen to match, which is why rip-only
//!    handlers worked there); this port does not inherit that — a handler
//!    reading `REG_RSP` must get the interrupted `%rsp`.
//! 2. **The guest already has an async frame format** (`child.rs`'s private
//!    block + zeroed ucontext, restored by its `rt_sigreturn`), so a fault
//!    frame must be *identifiable* at `rt_sigreturn`. Metal has one frame
//!    format and one restore path; here the fault path records its frame base
//!    in a per-slot LIFO and [`sync_sigreturn`] matches the current
//!    `%rsp - 8` (where the restorer's `ret` leaves it) against it — no frame
//!    magic on the user stack, the async path is untouched.
//!
//! What a delivery leaves behind, from the handler's `%rsp` up:
//! `[pretcode = sa_restorer][ucontext (real mcontext)][siginfo]`, the FPU image
//! 64-aligned above (`fpstate` points at it; restored on `rt_sigreturn` after
//! the same sanitize the async arm applies). Only `%rdi`/`%rsi`/`%rdx`/
//! `%rip`/`%rsp`/`rflags` change on install — Linux's `setup_rt_frame` shape,
//! so the other saved registers read as the interrupted code had them.
//!
//! Declined deliveries fall through to the kill exactly as before: no handler,
//! `SA_RESTORER` missing, SIGSEGV blocked (a blocked fault cannot be deferred —
//! the instruction would re-execute and fault again), an unwritable stack, or a
//! full per-slot frame LIFO. The kill path is unchanged code.
//!
//! Recorded limit: the restore cannot put `%rcx`/`%r11` back (the syscall exit
//! is `sysret` — both are architecturally consumed, and the async path clobbers
//! them across every handler return today, which guest programs already
//! tolerate). The FPU image restore means the interrupted XMM state survives
//! the handler.

use akuma_exec::process::{SignalHandler, MAX_SIGNALS};
use akuma_ring3_amd64 as ring3;
use akuma_tokio_kernel_core::proc::NSLOTS;
use akuma_tokio_kernel_core::signal as sigcore;
use akuma_trap_amd64::Kind;

/// The signal this module delivers: `SIGSEGV` (page faults, the G13 datum).
const SIGSEGV: u32 = 11;

/// `SA_ONSTACK` (Linux uapi): run the handler on the registered alternate
/// stack. The guest's `sigaltstack(2)` is ENOSYS today (the dispatch table has
/// no 131 arm), so this is honored best-effort: a registered stack is used, no
/// stack falls back to the interrupted one — metal's rule.
const SA_ONSTACK: u64 = 0x1;

/// The x86-64 red zone: 128 bytes below the interrupted `%rsp` are the
/// interrupted code's and must not be a frame landing pad (metal's rule; an
/// alternate stack has nothing below its top to preserve).
const RED_ZONE: u64 = 128;

/// `si_code` for a fault, metal's (`akuma-kernel-amd64::signal::segv`).
const SEGV_MAPERR: i32 = 1;
const SEGV_ACCERR: i32 = 2;

/// The handler's entry `rflags`: `IF` set (a ring-3 thread with interrupts
/// masked is not preemptible) and reserved bit 1 set; `DF` clear, which the C
/// ABI promises. The same value the async arm's restore sanitizes to.
const RFLAGS_FORCED: u64 = 0x202;

/// The interrupted FPU/SSE image's bytes, as `fxsave` writes them.
const FX_BYTES: u64 = akuma_sched_amd64::FxArea::SIZE as u64;

/// What a delivery puts on the stack, end to end (metal's `FRAME_FOOTPRINT`):
/// the FPU image, its 64-byte alignment slack, the frame and its 16-byte
/// alignment slack plus the 8 for `pretcode`. An alternate stack smaller than
/// this cannot take a signal.
const FRAME_FOOTPRINT: u64 = FX_BYTES + 64 + RT_SIGFRAME_SIZE as u64 + 16 + 8;

/// `struct sigcontext_64` — the interrupted register file as Linux lays it out
/// (`arch/x86/include/uapi/asm/sigcontext.h`). Field order is the ABI; the
/// values are serialized by hand below, so these structs exist to be measured:
/// the offsets a handler indexes through the `REG_*` constants.
#[repr(C)]
#[allow(dead_code)] // offsets are the ABI; the values are written as raw bytes
#[derive(Clone, Copy, Default)]
struct SigContext {
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    /// glibc `REG_RSP` = 11 — byte 88. (The metal struct carries `rbx` here;
    /// see the module header, divergence 1.)
    rsp: u64,
    rbx: u64,
    rdx: u64,
    rax: u64,
    rcx: u64,
    rip: u64,
    eflags: u64,
    cs: u16,
    gs: u16,
    fs: u16,
    ss: u16,
    err: u64,
    trapno: u64,
    oldmask: u64,
    cr2: u64,
    /// `struct _fpstate __user *`: the interrupted FPU image, placed above the
    /// frame and loaded back by `rt_sigreturn` — Linux's contract.
    fpstate: u64,
    reserved: [u64; 8],
}

/// `stack_t`, as the kernel writes it into `uc_stack`.
#[repr(C)]
#[allow(dead_code)] // measured, not constructed
#[derive(Clone, Copy, Default)]
struct StackT {
    ss_sp: u64,
    ss_flags: i32,
    _pad: u32,
    ss_size: u64,
}

/// `struct ucontext`, the kernel's: `uc_sigmask` is the 8-byte kernel
/// `sigset_t`, not musl's 128-byte one — Linux's own mismatch (the metal
/// kernel documents it; reproducing it is the point).
#[repr(C)]
#[allow(dead_code)] // measured, not constructed
#[derive(Clone, Copy, Default)]
struct UContext {
    uc_flags: u64,
    uc_link: u64,
    uc_stack: StackT,
    uc_mcontext: SigContext,
    uc_sigmask: u64,
}

/// `siginfo_t`'s kernel form: three ints, then the 112-byte union. Only the
/// fault fields are ever set: `si_code` and `si_addr` (the union's first word),
/// which is what a `SIGSEGV` handler works from.
#[repr(C)]
#[allow(dead_code)] // measured, not constructed
#[derive(Clone, Copy, Default)]
struct SigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    _pad: i32,
    fields: [u64; 14],
}

/// `struct rt_sigframe`, metal's: `pretcode` first (the handler is entered by
/// `iret`, not `call`, so the return address must already be on the stack for
/// its closing `ret`), then the ucontext and the siginfo. `rsi`/`rdx` carry the
/// pointers, so the structs' order in the frame is private.
#[repr(C)]
#[allow(dead_code)] // measured, not constructed
#[derive(Clone, Copy, Default)]
struct RtSigFrame {
    pretcode: u64,
    uc: UContext,
    info: SigInfo,
}

/// Offsets a handler and a debugger index by hand (metal's assertion block,
/// restated for this port's Linux-order `SigContext` — the divergence that
/// matters is `rsp` at 88, not 120).
const _: () = {
    assert!(core::mem::offset_of!(SigContext, rsp) == 88);
    assert!(core::mem::offset_of!(SigContext, rip) == 128);
    assert!(core::mem::offset_of!(SigContext, eflags) == 136);
    assert!(core::mem::offset_of!(SigContext, err) == 152);
    assert!(core::mem::offset_of!(SigContext, cr2) == 176);
    assert!(core::mem::offset_of!(SigContext, fpstate) == 184);
    assert!(core::mem::size_of::<SigContext>() == 256);
    assert!(core::mem::size_of::<StackT>() == 24);
    assert!(core::mem::offset_of!(UContext, uc_stack) == 16);
    assert!(core::mem::offset_of!(UContext, uc_mcontext) == 40);
    assert!(core::mem::size_of::<UContext>() == 304);
    assert!(core::mem::offset_of!(SigInfo, si_code) == 8);
    assert!(core::mem::size_of::<SigInfo>() == 128);
    assert!(core::mem::offset_of!(RtSigFrame, uc) == 8);
    assert!(core::mem::offset_of!(RtSigFrame, info) == 312);
    assert!(core::mem::size_of::<RtSigFrame>() == 440);
};

const RT_SIGFRAME_SIZE: usize = core::mem::size_of::<RtSigFrame>();

/// Byte offsets inside the serialized frame — the structs above, fixed.
const OFF_UC: usize = core::mem::offset_of!(RtSigFrame, uc);
const OFF_INFO: usize = core::mem::offset_of!(RtSigFrame, info);
const OFF_SC: usize = OFF_UC + core::mem::offset_of!(UContext, uc_mcontext);
const OFF_SIGMASK: usize = OFF_UC + core::mem::offset_of!(UContext, uc_sigmask);

/// The `gregs[]` word for `sigcontext` field `word` (glibc's `REG_*` order).
const fn greg(word: usize) -> usize {
    OFF_SC + word * 8
}

fn put(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn get(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().expect("fixed frame offsets"))
}

/// Per-slot LIFO of live fault frames. Both ends run under the BKL (the fault
/// path holds it for a user fault; every syscall return holds it), so relaxed
/// atomics and a dense-prefix scan are enough — the same discipline as
/// `child.rs`'s `RETURN_RSP`.
const SYNC_DEPTH_MAX: usize = 4;
static SYNC_BASES: [[core::sync::atomic::AtomicU64; SYNC_DEPTH_MAX]; NSLOTS] =
    [const { [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_DEPTH_MAX] }; NSLOTS];
static SYNC_FPS: [[core::sync::atomic::AtomicU64; SYNC_DEPTH_MAX]; NSLOTS] =
    [const { [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_DEPTH_MAX] }; NSLOTS];
static SYNC_DEPTHS: [core::sync::atomic::AtomicUsize; NSLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; NSLOTS];

fn lifo_push(slot: usize, base: u64, fp: u64) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let depth = SYNC_DEPTHS[slot].load(Relaxed);
    if depth >= SYNC_DEPTH_MAX {
        return false;
    }
    SYNC_BASES[slot][depth].store(base, Relaxed);
    SYNC_FPS[slot][depth].store(fp, Relaxed);
    SYNC_DEPTHS[slot].store(depth + 1, Relaxed);
    true
}

fn lifo_take(slot: usize, base: u64) -> Option<u64> {
    use core::sync::atomic::Ordering::Relaxed;
    let depth = SYNC_DEPTHS[slot].load(Relaxed);
    let mut at = None;
    for (i, slot_base) in SYNC_BASES[slot][..depth].iter().enumerate() {
        if slot_base.load(Relaxed) == base {
            at = Some(i);
            break;
        }
    }
    let at = at?;
    let fp = SYNC_FPS[slot][at].load(Relaxed);
    // Keep the used prefix dense so the push-side first-free-slot scan stays
    // the only bookkeeping.
    for i in at..depth - 1 {
        SYNC_BASES[slot][i].store(SYNC_BASES[slot][i + 1].load(Relaxed), Relaxed);
        SYNC_FPS[slot][i].store(SYNC_FPS[slot][i + 1].load(Relaxed), Relaxed);
    }
    SYNC_BASES[slot][depth - 1].store(0, Relaxed);
    SYNC_FPS[slot][depth - 1].store(0, Relaxed);
    SYNC_DEPTHS[slot].store(depth - 1, Relaxed);
    Some(fp)
}

/// Deliver the fault as `SIGSEGV` to the task's own handler.
///
/// Called from `fault_trap_locked` for a ring-3 page fault that the fill /
/// stack-grow / COW arms could not resolve — before the kill. `false` leaves
/// everything untouched: the caller's kill proceeds exactly as on the
/// unfeatured kernel.
pub fn deliver(t: &mut akuma_trap_amd64::Trap<'_>, cr2: u64, code: u64) -> bool {
    if t.kind != Kind::PageFault {
        return false;
    }
    let slot = crate::exec_twin::current_slot();
    if slot >= NSLOTS || !crate::child::is_real_task() {
        return false;
    }
    let Some(process) = akuma_exec::process::current_process_shared() else {
        return false;
    };
    let idx = SIGSEGV as usize - 1;
    if idx >= MAX_SIGNALS {
        return false;
    }
    let action = process.signal_actions.actions.lock()[idx];
    let SignalHandler::UserFn(entry) = action.handler else {
        return false;
    };
    // A blocked synchronous fault cannot be deferred — the faulting instruction
    // would re-execute and fault again (metal's rule). Kill is Linux's answer too.
    if akuma_exec::threading::thread_signal_mask() & (1u64 << idx) != 0 {
        return false;
    }
    // x86_64 has no kernel signal trampoline: the restorer is the program's.
    if action.flags & sigcore::SA_RESTORER == 0 || action.restorer == 0 {
        return false;
    }

    let (alt_sp, alt_size, _alt_flags) = akuma_threading::get_sigaltstack(slot);
    let on_alt = action.flags & SA_ONSTACK != 0 && alt_sp != 0 && alt_size >= FRAME_FOOTPRINT;
    let interrupted_rsp = t.frame.rsp;
    // The red zone is only live on the interrupted thread's own stack; an
    // alternate stack has nothing below its top to preserve (metal's rule).
    let top = if on_alt {
        let Some(v) = alt_sp.checked_add(alt_size) else { return false };
        v
    } else {
        let Some(v) = interrupted_rsp.checked_sub(RED_ZONE) else { return false };
        v
    };
    // Highest, the FPU image: 64-byte aligned (`xsave`'s rule; Linux puts the
    // image there too). The frame below it, `%rsp + 8` 16-byte aligned at the
    // handler's first instruction — entered as if `call`ed, `pretcode` the
    // pushed return address.
    let Some(fp_base) = top.checked_sub(FX_BYTES).map(|v| v & !0x3F) else {
        return false;
    };
    let Some(base) = fp_base
        .checked_sub(RT_SIGFRAME_SIZE as u64)
        .map(|v| v & !0xF)
        .and_then(|v| v.checked_sub(8))
    else {
        return false;
    };
    if base < 0x1000
        || !crate::user_ok(base, RT_SIGFRAME_SIZE as u64)
        || !crate::user_ok(fp_base, FX_BYTES)
        || !lifo_push(slot, base, fp_base)
    {
        return false;
    }

    // The interrupted FPU/SSE file, read before anything else happens — the
    // trap has not switched tasks, so the live registers are the faulting
    // thread's. The handler starts from the initial state, as on Linux.
    let mut fx = akuma_sched_amd64::FxArea::zeroed();
    akuma_sched_amd64::fpu_save_live(&mut fx);

    let mask = akuma_exec::threading::thread_signal_mask();
    let si_code = if code & 1 == 0 { SEGV_MAPERR } else { SEGV_ACCERR };

    let mut buf = [0u8; RT_SIGFRAME_SIZE];
    put(&mut buf, 0, action.restorer as u64); // pretcode
    // uc_stack: what `sigaltstack` would report inside the handler.
    put(&mut buf, OFF_UC + 16, alt_sp);
    put_i32(&mut buf, OFF_UC + 24, i32::from(on_alt)); // SS_ONSTACK
    put(&mut buf, OFF_UC + 32, alt_size);
    put(&mut buf, OFF_SIGMASK, mask);
    // uc_mcontext, Linux field order (the `greg()` constants above).
    put(&mut buf, greg(0), t.regs.r8);
    put(&mut buf, greg(1), t.regs.r9);
    put(&mut buf, greg(2), t.regs.r10);
    put(&mut buf, greg(3), t.regs.r11);
    put(&mut buf, greg(4), t.regs.r12);
    put(&mut buf, greg(5), t.regs.r13);
    put(&mut buf, greg(6), t.regs.r14);
    put(&mut buf, greg(7), t.regs.r15);
    put(&mut buf, greg(8), t.regs.rdi);
    put(&mut buf, greg(9), t.regs.rsi);
    put(&mut buf, greg(10), t.regs.rbp);
    put(&mut buf, greg(11), interrupted_rsp);
    put(&mut buf, greg(12), t.regs.rbx);
    put(&mut buf, greg(13), t.regs.rdx);
    put(&mut buf, greg(14), t.regs.rax);
    put(&mut buf, greg(15), t.regs.rcx);
    put(&mut buf, greg(16), t.frame.rip);
    put(&mut buf, greg(17), t.frame.rflags);
    put_u16(&mut buf, greg(18), t.frame.cs as u16);
    put_u16(&mut buf, greg(18) + 2, 0); // gs
    put_u16(&mut buf, greg(18) + 4, 0); // fs
    put_u16(&mut buf, greg(18) + 6, t.frame.ss as u16);
    put(&mut buf, greg(19), code); // err: the #PF error code
    put(&mut buf, greg(20), 14); // trapno: #PF
    put(&mut buf, greg(21), mask); // oldmask
    put(&mut buf, greg(22), cr2);
    put(&mut buf, greg(23), fp_base); // fpstate
    // info: si_signo, si_errno 0, si_code, si_addr (the union's first word).
    put_i32(&mut buf, OFF_INFO, SIGSEGV as i32);
    put_i32(&mut buf, OFF_INFO + 8, si_code);
    put(&mut buf, OFF_INFO + 16, cr2);

    if !crate::copy_to_user(fp_base, fx.bytes()) || !crate::copy_to_user(base, &buf) {
        lifo_take(slot, base);
        return false;
    }
    // After the writes, so a declined delivery leaves the file untouched.
    akuma_sched_amd64::fpu_reset_live();

    akuma_exec::threading::set_thread_signal_mask(sigcore::mask_during(SIGSEGV, &action, mask));
    // Linux's `setup_rt_frame` shape: only the argument registers, `rip`,
    // `rsp` and the flags change — the rest of the file the stub saved reads
    // as the interrupted code had it. The restorer slot carries the return:
    // the handler's `ret` lands on `sa_restorer`, whose `rt_sigreturn` finds
    // the frame at `%rsp - 8`.
    t.frame.rip = entry as u64;
    t.frame.rsp = base;
    t.frame.rflags = RFLAGS_FORCED;
    t.regs.rdi = u64::from(SIGSEGV);
    if action.flags & sigcore::SA_SIGINFO != 0 {
        t.regs.rsi = base + OFF_INFO as u64;
        t.regs.rdx = base + OFF_UC as u64;
    }
    true
}

/// The `rt_sigreturn` half: restore the fault frame the returning `%rsp - 8`
/// names, if it is one of this module's. `None` = not ours — the caller's async
/// path proceeds untouched. `Some(rax)` = restored; `rax` is the interrupted
/// call's own answer, the same convention the async arm keeps.
///
/// The restore source is the frame's **ucontext** — the point of the port:
/// HotSpot's handler patches `uc_mcontext.gregs[REG_RIP]` (the exception-stub
/// redirect that an implicit null check IS), and any edit it makes must be what
/// `sysret` resumes. The FPU image comes from `fpstate`, sanitized exactly like
/// the async arm sanitizes its user-chosen image.
pub fn sync_sigreturn(fr: &mut ring3::SyscallFrame) -> Option<u64> {
    let slot = crate::exec_twin::current_slot();
    if slot >= NSLOTS {
        return None;
    }
    let rsp = ring3::user_rsp_stash();
    let base = rsp.checked_sub(8)?;
    // The frame's own `fpstate` field names the image; the LIFO's copy is the
    // bookkeeping value, consumed here so the slot is freed either way.
    lifo_take(slot, base)?;

    let mut buf = [0u8; RT_SIGFRAME_SIZE];
    if !crate::user_ok(base, RT_SIGFRAME_SIZE as u64) || !crate::copy_from_user(&mut buf, base) {
        return Some(crate::e(ring3::err::EFAULT));
    }
    let rip = get(&buf, greg(16));
    let user_rsp = get(&buf, greg(11));
    // Never trust a user-built frame with a kernel address or odd flags.
    if rip >= crate::USER_TOP || user_rsp >= crate::USER_TOP {
        return Some(crate::e(ring3::err::EFAULT));
    }
    // The FP/SSE file the handler interrupted: a program chooses these bytes,
    // and four kinds of malformed image make `xrstor` a ring-0 #GP, so it is
    // sanitized first. 0 = initial state.
    let fx_at = get(&buf, greg(23));
    if fx_at == 0 {
        akuma_sched_amd64::fpu_reset_live();
    } else {
        let mut img = [0u8; akuma_sched_amd64::FxArea::SIZE];
        if fx_at % 64 != 0
            || !crate::user_ok(fx_at, img.len() as u64)
            || !crate::copy_from_user(&mut img, fx_at)
        {
            return Some(crate::e(ring3::err::EFAULT));
        }
        let Some(mut fx) = akuma_sched_amd64::FxArea::from_bytes(&img) else {
            return Some(crate::e(ring3::err::EFAULT));
        };
        fx.sanitize();
        akuma_sched_amd64::fpu_restore_live(&fx);
    }
    fr.r15 = get(&buf, greg(7));
    fr.r14 = get(&buf, greg(6));
    fr.r13 = get(&buf, greg(5));
    fr.r12 = get(&buf, greg(4));
    fr.rbp = get(&buf, greg(10));
    fr.rbx = get(&buf, greg(12));
    fr.r9 = get(&buf, greg(1));
    fr.r8 = get(&buf, greg(0));
    fr.r10 = get(&buf, greg(2));
    fr.rdx = get(&buf, greg(13));
    fr.rsi = get(&buf, greg(9));
    fr.rdi = get(&buf, greg(8));
    fr.rax = get(&buf, greg(14));
    fr.user_rip = rip;
    fr.user_rflags = (get(&buf, greg(17)) & 0x0000_0CD5) | 0x202;
    crate::child::set_return_rsp(user_rsp);
    akuma_exec::threading::set_thread_signal_mask(get(&buf, OFF_SIGMASK));
    // The interrupted call's own result, as the async arm returns.
    Some(fr.rax)
}
