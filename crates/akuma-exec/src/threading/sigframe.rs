//! The AArch64 `rt_sigframe`, as `#[repr(C)]` structs instead of hand-written
//! byte offsets.
//!
//! `exceptions.rs` used to build this frame with ~130 `core::ptr::write(base.add(N))`
//! calls and tear it down with ~40 matching `read`s, each offset spelled as a literal
//! next to a comment naming the field it was supposed to be. Every one of those
//! literals was an unchecked claim: nothing connected `mc.add(256)` to "sp" except the
//! comment beside it, and the frame is ABI — a wrong offset is a corrupted userspace
//! context, not a compile error.
//!
//! Here the layout is the type, the offsets are derived from it with `offset_of!`, and
//! the `const _: () = assert!(...)` block below turns any drift into a build failure on
//! every profile. `UNSAFE_AUDIT.md` §4 P1 is the plan; the phase record is
//! `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` Phase 7.
//!
//! # The layout is Linux's, with one deliberate divergence
//!
//! `siginfo_t` (128) + `ucontext_t` header (176) + `sigcontext` (280) + an FPSIMD
//! extension record (528) + an `_aarch64_ctx` null terminator (8) = **1120 bytes**.
//! Linux's `struct sigcontext` ends with `__u8 __reserved[4096]
//! __attribute__((__aligned__(16)))`, which means two things this frame does
//! differently, both pre-existing and both preserved here rather than "fixed":
//!
//! 1. **The FPSIMD record sits at frame+584, not +592.** Linux's `aligned(16)` pads
//!    `sigcontext` from 280 to 288 before `__reserved` begins; this frame packs it at
//!    280. A handler that walks the `_aarch64_ctx` chain from
//!    `&uc.uc_mcontext.__reserved` finds the record 8 bytes early.
//! 2. **`__reserved` is 536 bytes, not 4096.** The frame is sized for exactly the
//!    FPSIMD record plus its terminator, so it costs 1120 bytes of user stack instead
//!    of ~4.7 KB.
//!
//! Neither is changed by this module — changing either is an ABI change with its own
//! A/B, and every offset here reproduces byte-for-byte what the hand-written writes
//! produced. They are recorded so the next reader does not have to re-derive them from
//! the Linux headers, which is how they were found.

use super::types::{UserTrapFrame, IRQ_FRAME_SIZE};
use core::mem::{offset_of, size_of};

/// `_aarch64_ctx.magic` identifying the FPSIMD extension record.
pub const FPSIMD_MAGIC: u32 = 0x4650_8001;

/// `_sifields._sigchld`. Reached at `siginfo_t + 16` — on LP64 the union is 8-byte
/// aligned, so it starts in `si_addr`'s slot.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigchldFields {
    pub si_pid: u32,
    pub si_uid: u32,
    pub si_status: i32,
    _pad: u32,
}

impl SigchldFields {
    /// The three fields a `SIGCHLD` delivery fills; the union's remaining bytes stay
    /// zero. A constructor rather than a literal because the trailing pad is private —
    /// it is layout, not payload.
    #[must_use]
    pub const fn new(si_pid: u32, si_uid: u32, si_status: i32) -> Self {
        Self { si_pid, si_uid, si_status, _pad: 0 }
    }
}

/// `_sifields._sigfault`, the arm every non-`SIGCHLD` delivery fills.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigfaultFields {
    pub si_addr: u64,
}

/// `siginfo_t::_sifields`. A union, because that is what it is: the two arms below
/// overlap in C and the old code open-coded that by writing `si_pid` at +16 for
/// `SIGCHLD` and `si_addr` at +16 for everything else.
#[repr(C)]
#[derive(Clone, Copy)]
pub union SiFields {
    pub chld: SigchldFields,
    pub fault: SigfaultFields,
    _pad: [u8; 112],
}

/// `siginfo_t` (128 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    /// C gets this from the union's 8-byte alignment; spelled out so the layout does
    /// not depend on what the compiler chooses to insert.
    _pad0: u32,
    pub fields: SiFields,
}

/// `stack_t` (24 bytes), `ucontext_t::uc_stack`. Go reads it to decide whether the
/// signal arrived on the sigaltstack.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StackT {
    pub ss_sp: u64,
    pub ss_flags: i32,
    _pad: u32,
    pub ss_size: u64,
}

/// `SS_ONSTACK` — the signal was delivered on the alternate stack.
pub const SS_ONSTACK: i32 = 1;

/// `struct sigcontext` (280 bytes), `ucontext_t::uc_mcontext`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigContext {
    pub fault_address: u64,
    /// x0–x30.
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
}

/// `ucontext_t` header + `uc_mcontext` (176 + 280 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UContext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_stack: StackT,
    pub uc_sigmask: u64,
    /// `__u8 __unused[1024/8 - sizeof(sigset_t)]` — glibc's 1024-bit `sigset_t`.
    _unused: [u8; 120],
    /// Linux gets these 8 bytes from `sigcontext`'s `aligned(16)`; spelled out for the
    /// same reason as `SigInfo::_pad0`.
    _pad_mcontext_align: [u8; 8],
    pub uc_mcontext: SigContext,
}

/// `struct fpsimd_context` (528 bytes).
///
/// `vregs` is `[u64; 64]` rather than `[u128; 32]` **on purpose**: `u128` carries
/// 16-byte alignment, which would pad this record and move every offset after it.
/// The record lands at frame+584, which is 8 (mod 16) — the old code's comment about
/// `vregs_dst` being only 8-byte aligned was describing exactly this.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FpsimdContext {
    pub magic: u32,
    pub size: u32,
    pub fpsr: u32,
    pub fpcr: u32,
    pub vregs: [u64; 64],
}

/// The whole `rt_sigframe` as userspace sees it, 1120 bytes at the new `sp`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtSigFrame {
    pub info: SigInfo,
    pub uc: UContext,
    pub fpsimd: FpsimdContext,
    /// `_aarch64_ctx{magic: 0, size: 0}` terminating the extension chain.
    pub end_magic: u32,
    pub end_size: u32,
}

/// Size of the frame pushed onto the user stack.
pub const SIGFRAME_SIZE: usize = size_of::<RtSigFrame>();
/// Offset of `siginfo_t` — what `x1` points at under `SA_SIGINFO`.
pub const SIGFRAME_SIGINFO: usize = offset_of!(RtSigFrame, info);
/// Offset of `ucontext_t` — what `x2` points at under `SA_SIGINFO`.
pub const SIGFRAME_UCONTEXT: usize = offset_of!(RtSigFrame, uc);
/// Offset of `uc_mcontext`.
pub const SIGFRAME_MCONTEXT: usize = SIGFRAME_UCONTEXT + offset_of!(UContext, uc_mcontext);
/// Offset of the FPSIMD extension record.
pub const SIGFRAME_FPSIMD: usize = offset_of!(RtSigFrame, fpsimd);
/// Offset of `uc_sigmask`, the mask `rt_sigreturn` restores.
pub const SIGFRAME_UC_SIGMASK: usize = SIGFRAME_UCONTEXT + offset_of!(UContext, uc_sigmask);

// The literals the hand-written version encoded. These are the ABI, so a layout change
// that moves any of them is a build failure rather than a corrupted user context.
const _: () = assert!(SIGFRAME_SIZE == 1120);
const _: () = assert!(SIGFRAME_SIGINFO == 0);
const _: () = assert!(SIGFRAME_UCONTEXT == 128);
const _: () = assert!(SIGFRAME_MCONTEXT == 304);
const _: () = assert!(SIGFRAME_FPSIMD == 584);
const _: () = assert!(SIGFRAME_UC_SIGMASK == 168);
const _: () = assert!(size_of::<SigInfo>() == 128);
const _: () = assert!(size_of::<StackT>() == 24);
const _: () = assert!(size_of::<SigContext>() == 280);
const _: () = assert!(size_of::<FpsimdContext>() == 528);
// Within siginfo_t: si_code at +8, and the union arms both at +16.
const _: () = assert!(offset_of!(SigInfo, si_code) == 8);
const _: () = assert!(offset_of!(SigInfo, fields) == 16);
const _: () = assert!(offset_of!(SigchldFields, si_status) == 8);
// Within ucontext_t: uc_stack at +16 (ss_flags +24, ss_size +32).
const _: () = assert!(offset_of!(UContext, uc_stack) == 16);
const _: () = assert!(offset_of!(StackT, ss_size) == 16);
// Within sigcontext: regs at +8, sp/pc/pstate at +256/+264/+272.
const _: () = assert!(offset_of!(SigContext, regs) == 8);
const _: () = assert!(offset_of!(SigContext, sp) == 256);
const _: () = assert!(offset_of!(SigContext, pc) == 264);
const _: () = assert!(offset_of!(SigContext, pstate) == 272);
// Within the FPSIMD record: fpsr/fpcr at +8/+12, vregs at +16.
const _: () = assert!(offset_of!(FpsimdContext, fpsr) == 8);
const _: () = assert!(offset_of!(FpsimdContext, vregs) == 16);

impl RtSigFrame {
    /// An all-zero frame, matching the `write_bytes(base, 0, SIGFRAME_SIZE)` the
    /// hand-written builder opened with. `const` and unsafe-free: every field is an
    /// integer or an array of them, and the union's zero is spelled explicitly because
    /// a union has no `Default`.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            info: SigInfo {
                si_signo: 0,
                si_errno: 0,
                si_code: 0,
                _pad0: 0,
                fields: SiFields { _pad: [0; 112] },
            },
            uc: UContext {
                uc_flags: 0,
                uc_link: 0,
                uc_stack: StackT { ss_sp: 0, ss_flags: 0, _pad: 0, ss_size: 0 },
                uc_sigmask: 0,
                _unused: [0; 120],
                _pad_mcontext_align: [0; 8],
                uc_mcontext: SigContext {
                    fault_address: 0,
                    regs: [0; 31],
                    sp: 0,
                    pc: 0,
                    pstate: 0,
                },
            },
            fpsimd: FpsimdContext { magic: 0, size: 0, fpsr: 0, fpcr: 0, vregs: [0; 64] },
            end_magic: 0,
            end_size: 0,
        }
    }

    /// Save the interrupted user context into `uc_mcontext`.
    ///
    /// The 31 assignments are the transcription risk this whole module exists to
    /// bound: written once, host-tested for a round trip against
    /// [`Self::restore_regs`], instead of open-coded twice at two different offsets.
    pub fn save_regs(&mut self, f: &UserTrapFrame, fault_address: u64) {
        let mc = &mut self.uc.uc_mcontext;
        mc.fault_address = fault_address;
        mc.regs = [
            f.x0, f.x1, f.x2, f.x3, f.x4, f.x5, f.x6, f.x7,
            f.x8, f.x9, f.x10, f.x11, f.x12, f.x13, f.x14, f.x15,
            f.x16, f.x17, f.x18, f.x19, f.x20, f.x21, f.x22, f.x23,
            f.x24, f.x25, f.x26, f.x27, f.x28, f.x29, f.x30,
        ];
        mc.sp = f.sp_el0;
        mc.pc = f.elr_el1;
        mc.pstate = f.spsr_el1;
    }

    /// Restore x0–x30, `sp` and `pc` from `uc_mcontext` into the trap frame, and
    /// return the **raw** saved `pstate`.
    ///
    /// `spsr_el1` is deliberately *not* written here: the caller validates the saved
    /// pstate first (a corrupted `M[4:0]` would make `ERET` crash the kernel) and that
    /// decision prints, which is bin-crate work.
    pub fn restore_regs(&self, f: &mut UserTrapFrame) -> u64 {
        let mc = &self.uc.uc_mcontext;
        let r = &mc.regs;
        f.x0 = r[0]; f.x1 = r[1]; f.x2 = r[2]; f.x3 = r[3];
        f.x4 = r[4]; f.x5 = r[5]; f.x6 = r[6]; f.x7 = r[7];
        f.x8 = r[8]; f.x9 = r[9]; f.x10 = r[10]; f.x11 = r[11];
        f.x12 = r[12]; f.x13 = r[13]; f.x14 = r[14]; f.x15 = r[15];
        f.x16 = r[16]; f.x17 = r[17]; f.x18 = r[18]; f.x19 = r[19];
        f.x20 = r[20]; f.x21 = r[21]; f.x22 = r[22]; f.x23 = r[23];
        f.x24 = r[24]; f.x25 = r[25]; f.x26 = r[26]; f.x27 = r[27];
        f.x28 = r[28]; f.x29 = r[29]; f.x30 = r[30];
        f.sp_el0 = mc.sp;
        f.elr_el1 = mc.pc;
        mc.pstate
    }
}

/// The EL0 **sync** trap frame's NEON/FP save area.
///
/// `sync_el0_handler` saves Q0–Q31 at frame+304, then FPCR and FPSR; the signal path is
/// the only Rust code that reads them, and it did so at three bare literals (`+304`,
/// `+816`, `+824`). The offset is named once here and the field order is checked.
///
/// **Not the EL0 IRQ frame's layout**, which is also 832 bytes but puts its NEON block
/// at +288 with FPCR/FPSR at +800/+808 (`exceptions.rs`'s `irq_el0_handler` comment).
/// Only pass a sync-frame pointer to [`sync_frame_neon`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SyncFrameNeon {
    /// Q0–Q31, two `u64` per register — see [`FpsimdContext::vregs`] on why not `u128`.
    pub vregs: [u64; 64],
    /// The hardware registers are 32-bit; the handler stores each in a 64-bit slot.
    pub fpcr: u64,
    pub fpsr: u64,
}

/// Where [`SyncFrameNeon`] begins within the EL0 sync trap frame.
pub const SYNC_FRAME_NEON_OFFSET: usize = 304;

const _: () = assert!(size_of::<SyncFrameNeon>() == 528);
const _: () = assert!(offset_of!(SyncFrameNeon, fpcr) == 512);
// The GPR block, its padding and the NEON block are the whole 832-byte frame.
const _: () = assert!(SYNC_FRAME_NEON_OFFSET + size_of::<SyncFrameNeon>() == IRQ_FRAME_SIZE);
const _: () = assert!(size_of::<UserTrapFrame>() <= SYNC_FRAME_NEON_OFFSET);

/// The NEON save area of an EL0 **sync** trap frame.
///
/// # Safety
///
/// `frame` must point at an EL0 sync trap frame — 832 bytes, laid out by
/// `sync_el0_handler`. An EL0 *IRQ* frame has a different NEON offset and passing one
/// here reads the wrong registers.
#[must_use]
pub unsafe fn sync_frame_neon(frame: *mut UserTrapFrame) -> *mut SyncFrameNeon {
    // SAFETY: the caller guarantees an 832-byte sync frame, and the assertions above
    // place this area entirely inside it.
    unsafe { frame.cast::<u8>().add(SYNC_FRAME_NEON_OFFSET).cast::<SyncFrameNeon>() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `xN` = `MARK | (N+1)`, written out by field name rather than through
    /// `restore_regs`, so a transposition in the code under test cannot be cancelled
    /// out by the same transposition in the fixture. That is the whole failure mode of
    /// 31 hand-written assignments.
    const MARK: u64 = 0xAA00_0000_0000_0000;

    fn frame_with_marked_regs() -> UserTrapFrame {
        UserTrapFrame {
            x0: MARK | 1, x1: MARK | 2, x2: MARK | 3, x3: MARK | 4,
            x4: MARK | 5, x5: MARK | 6, x6: MARK | 7, x7: MARK | 8,
            x8: MARK | 9, x9: MARK | 10, x10: MARK | 11, x11: MARK | 12,
            x12: MARK | 13, x13: MARK | 14, x14: MARK | 15, x15: MARK | 16,
            x16: MARK | 17, x17: MARK | 18, x18: MARK | 19, x19: MARK | 20,
            x20: MARK | 21, x21: MARK | 22, x22: MARK | 23, x23: MARK | 24,
            x24: MARK | 25, x25: MARK | 26, x26: MARK | 27, x27: MARK | 28,
            x28: MARK | 29, x29: MARK | 30, x30: MARK | 31,
            sp_el0: 0x7FFF_0000, elr_el1: 0x40_0000, spsr_el1: 0x3c0,
            tpidr_el0: 0x71D_0000, _padding: 0,
        }
    }

    #[test]
    fn layout_matches_the_hand_written_offsets() {
        assert_eq!(SIGFRAME_SIZE, 1120);
        assert_eq!(SIGFRAME_SIGINFO, 0);
        assert_eq!(SIGFRAME_UCONTEXT, 128);
        assert_eq!(SIGFRAME_MCONTEXT, 304);
        assert_eq!(SIGFRAME_FPSIMD, 584);
        assert_eq!(SIGFRAME_UC_SIGMASK, 168);
        // The null terminator is the last 8 bytes.
        assert_eq!(offset_of!(RtSigFrame, end_magic), 1112);
    }

    #[test]
    fn regs_round_trip_through_mcontext() {
        let f = frame_with_marked_regs();
        let mut sf = RtSigFrame::zeroed();
        sf.save_regs(&f, 0xDEAD_BEEF);

        assert_eq!(sf.uc.uc_mcontext.fault_address, 0xDEAD_BEEF);
        assert_eq!(sf.uc.uc_mcontext.regs[0], f.x0);
        assert_eq!(sf.uc.uc_mcontext.regs[8], f.x8);
        assert_eq!(sf.uc.uc_mcontext.regs[30], f.x30);
        assert_eq!(sf.uc.uc_mcontext.sp, f.sp_el0);
        assert_eq!(sf.uc.uc_mcontext.pc, f.elr_el1);
        assert_eq!(sf.uc.uc_mcontext.pstate, f.spsr_el1);

        let mut back = frame_with_marked_regs();
        back.x0 = 0;
        back.x30 = 0;
        back.sp_el0 = 0;
        back.spsr_el1 = 0xDEAD;
        let pstate = sf.restore_regs(&mut back);
        assert_eq!(pstate, f.spsr_el1);
        assert_eq!(back.x0, f.x0);
        assert_eq!(back.x30, f.x30);
        assert_eq!(back.sp_el0, f.sp_el0);
        assert_eq!(back.elr_el1, f.elr_el1);
        // `restore_regs` returns the saved pstate and leaves `spsr_el1` alone: the
        // caller validates `M[4:0]` before committing it.
        assert_eq!(back.spsr_el1, 0xDEAD);
    }

    /// Every GPR reaches its own slot: the whole array, not three spot checks.
    #[test]
    fn every_gpr_lands_in_its_own_slot() {
        let f = frame_with_marked_regs();
        let mut sf = RtSigFrame::zeroed();
        sf.save_regs(&f, 0);
        for (i, r) in sf.uc.uc_mcontext.regs.iter().enumerate() {
            assert_eq!(*r, 0xAA00_0000_0000_0000 | (i as u64 + 1), "regs[{i}]");
        }
    }

    /// The two `_sifields` arms overlap, and both start at `siginfo_t + 16`.
    #[test]
    fn siginfo_union_arms_overlap_at_16() {
        let mut sf = RtSigFrame::zeroed();
        sf.info.fields.fault = SigfaultFields { si_addr: 0x1234_5678_9ABC_DEF0 };
        // SAFETY: both arms are plain integers over the same 112 bytes; reading the
        // other arm is exactly what the C union permits and what this asserts.
        let (pid, uid) = unsafe { (sf.info.fields.chld.si_pid, sf.info.fields.chld.si_uid) };
        assert_eq!(pid, 0x9ABC_DEF0);
        assert_eq!(uid, 0x1234_5678);

        let bytes = frame_bytes(&sf);
        assert_eq!(&bytes[16..24], &0x1234_5678_9ABC_DEF0u64.to_le_bytes());
    }

    /// The frame is copied out as bytes, so what matters is where each write lands in
    /// the buffer — the property the old `base.add(N)` writes asserted by construction.
    #[test]
    fn fields_land_at_the_documented_byte_offsets() {
        let mut sf = RtSigFrame::zeroed();
        sf.info.si_signo = 11;
        sf.info.si_code = 1;
        sf.uc.uc_stack.ss_sp = 0xC0DE_0000;
        sf.uc.uc_stack.ss_flags = SS_ONSTACK;
        sf.uc.uc_stack.ss_size = 0x4000;
        sf.uc.uc_sigmask = 0x8000_0000_0000_0001;
        sf.uc.uc_mcontext.sp = 0x7FFF_0000;
        sf.fpsimd.magic = FPSIMD_MAGIC;
        sf.fpsimd.size = 528;
        sf.fpsimd.fpsr = 0x1234;

        let b = frame_bytes(&sf);
        assert_eq!(u32_at(&b, 0), 11, "si_signo");
        assert_eq!(u32_at(&b, 8), 1, "si_code");
        assert_eq!(u64_at(&b, 144), 0xC0DE_0000, "uc_stack.ss_sp");
        assert_eq!(u32_at(&b, 152), 1, "uc_stack.ss_flags");
        assert_eq!(u64_at(&b, 160), 0x4000, "uc_stack.ss_size");
        assert_eq!(u64_at(&b, 168), 0x8000_0000_0000_0001, "uc_sigmask");
        assert_eq!(u64_at(&b, 560), 0x7FFF_0000, "mcontext.sp");
        assert_eq!(u32_at(&b, 584), FPSIMD_MAGIC, "fpsimd.magic");
        assert_eq!(u32_at(&b, 588), 528, "fpsimd.size");
        assert_eq!(u32_at(&b, 592), 0x1234, "fpsimd.fpsr");
        // The terminator stays zero.
        assert_eq!(&b[1112..1120], &[0u8; 8], "_aarch64_ctx terminator");
    }

    fn frame_bytes(sf: &RtSigFrame) -> [u8; SIGFRAME_SIZE] {
        // SAFETY: `RtSigFrame` is `#[repr(C)]` over integers and arrays of them, with
        // no padding the assertions above do not name, so every byte is initialised.
        unsafe { core::mem::transmute_copy(sf) }
    }

    // The `expect`s below cannot fire — the ranges are constant and in bounds — and a
    // panic in a test is the failure report anyway. They exist once each rather than at
    // every assertion so the offsets stay the readable part.
    fn u32_at(b: &[u8; SIGFRAME_SIZE], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
    }

    fn u64_at(b: &[u8; SIGFRAME_SIZE], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
    }
}
