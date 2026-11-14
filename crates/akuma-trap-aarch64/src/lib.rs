#![cfg_attr(not(test), no_std)]
//! The AArch64 EL0 trap frame, as a handle.
//!
//! # What this replaces
//!
//! `akuma-exceptions` reached the frame the vector asm pushed through a raw
//! `*mut UserTrapFrame` and wrote `unsafe { (*frame).x0 }` / `unsafe { &*frame }`
//! at some fifty places - reads of `x0`/`x8`/`ELR`, the `ELR -= 4` of a restarted
//! syscall, a register fetched by number to emulate a trapped instruction, and a
//! shared reference held across a diagnostic print - each vouching for the same
//! fact. That fact is not about the line; it is about the *frame*, and it is true
//! once, at the entry point.
//!
//! # The one contract
//!
//! > A [`FrameHandle`] is made from the pointer the vector asm passes its handler:
//! > the frame it just pushed on **this thread's kernel stack**, live and
//! > exclusively the handler's until the asm epilogue `eret`s. It is `!Send` and
//! > `!Sync` - a frame does not leave the thread that took the trap.
//!
//! [`FrameHandle::new`] is the single `unsafe` call a handler makes; everything
//! after it is safe.
//!
//! # Why accessors and a snapshot, not `&mut UserTrapFrame`
//!
//! The handlers read and write the frame at many points while also passing its
//! address on (to the signal-frame builder, to the asm epilogue that restores from
//! it). A long-lived `&mut` held across those is exactly the aliasing a raw pointer
//! was used to avoid. So each access here is one **volatile** load or store of one
//! field - the same instruction the raw deref was - and a diagnostic that wants
//! several fields at once takes a [`snapshot`](FrameHandle::snapshot), a by-value
//! copy, instead of a reference that could be invalidated by a later write.

use core::marker::PhantomData;
use core::ptr::NonNull;

pub use akuma_exec_core::thread::UserTrapFrame;

/// Bytes in an EL0 trap frame (sync and IRQ frames are both this size).
pub const FRAME_BYTES: usize = 832;

/// Number of general registers in the frame (`x0`..`x30`).
pub const GPRS: usize = 31;

/// A handle on the trap frame the vector asm pushed for the current handler.
#[derive(Clone, Copy)]
pub struct FrameHandle {
    ptr: NonNull<UserTrapFrame>,
    /// `!Send + !Sync`: see the module contract.
    _not_send: PhantomData<*mut ()>,
}

impl FrameHandle {
    /// Name the frame at `frame`.
    ///
    /// # Safety
    /// `frame` is the frame the vector asm just pushed for this handler on the
    /// current thread's kernel stack, and stays live and unaliased by any other
    /// writer until the handler returns to the asm epilogue.
    ///
    /// # Panics
    /// If `frame` is null - a boot-order bug, not a state to degrade through.
    #[must_use]
    pub unsafe fn new(frame: *mut UserTrapFrame) -> Self {
        Self { ptr: NonNull::new(frame).expect("EL0 trap handler entered with a null frame"), _not_send: PhantomData }
    }

    /// The raw pointer, for the few callees that still take one (the signal-frame
    /// builder's NEON-area locator, the asm epilogue).
    #[must_use]
    pub fn as_ptr(self) -> *mut UserTrapFrame {
        self.ptr.as_ptr()
    }

    /// `x0`: the syscall's first argument on entry, its return value on exit.
    #[must_use]
    pub fn x0(self) -> u64 {
        // SAFETY: the module contract; one aligned u64 field.
        unsafe { core::ptr::addr_of!((*self.ptr.as_ptr()).x0).read_volatile() }
    }

    /// Store `x0`.
    pub fn set_x0(self, v: u64) {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of_mut!((*self.ptr.as_ptr()).x0).write_volatile(v) }
    }

    /// `x8`: the syscall number.
    #[must_use]
    pub fn x8(self) -> u64 {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of!((*self.ptr.as_ptr()).x8).read_volatile() }
    }

    /// `ELR_EL1` as saved at exception entry.
    #[must_use]
    pub fn elr(self) -> u64 {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of!((*self.ptr.as_ptr()).elr_el1).read_volatile() }
    }

    /// Store the saved `ELR_EL1` (what `eret` returns to).
    pub fn set_elr(self, v: u64) {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of_mut!((*self.ptr.as_ptr()).elr_el1).write_volatile(v) }
    }

    /// Store the user's `TPIDR_EL0` (its TLS base) for the `eret` to restore.
    pub fn set_tpidr_el0(self, v: u64) {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of_mut!((*self.ptr.as_ptr()).tpidr_el0).write_volatile(v) }
    }

    /// Store general register `n` (`0..=30`) - the signal path sets `x1`, `x2` and
    /// the link register `x30` for the handler it is about to enter.
    ///
    /// # Panics
    /// If `n >= 31`.
    #[track_caller]
    pub fn set_reg(self, n: usize, v: u64) {
        assert!(n < GPRS, "general register number out of range");
        // SAFETY: as [`Self::reg`].
        unsafe { self.ptr.as_ptr().cast::<u64>().add(n).write_volatile(v) }
    }

    /// Store `SP_EL0`, the user stack pointer the `eret` restores.
    pub fn set_sp_el0(self, v: u64) {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of_mut!((*self.ptr.as_ptr()).sp_el0).write_volatile(v) }
    }

    /// General register `n` (`0..=30`), for a handler that emulates the trapped
    /// instruction and has the register *number* from its encoding.
    ///
    /// # Panics
    /// If `n >= 31`.
    #[must_use]
    #[track_caller]
    pub fn reg(self, n: usize) -> u64 {
        assert!(n < GPRS, "general register number out of range");
        // SAFETY: `UserTrapFrame` is `repr(C)` with `x0`..`x30` as its first 31
        // `u64` fields, contiguous; `n < 31` stays inside them.
        unsafe { self.ptr.as_ptr().cast::<u64>().add(n).read_volatile() }
    }

    /// A by-value copy of the whole frame, read field by field.
    ///
    /// For diagnostics that print several registers: later writes to the live
    /// frame do not change the copy, and no reference into it outlives the call.
    #[must_use]
    pub fn snapshot(self) -> UserTrapFrame {
        let p = self.ptr.as_ptr();
        // SAFETY: the module contract; `repr(C)` plain `u64` fields, so a
        // volatile read of each is a valid value of the whole.
        unsafe {
            let mut regs = [0u64; GPRS];
            let mut i = 0;
            while i < GPRS {
                regs[i] = p.cast::<u64>().add(i).read_volatile();
                i += 1;
            }
            UserTrapFrame {
                x0: regs[0], x1: regs[1], x2: regs[2], x3: regs[3],
                x4: regs[4], x5: regs[5], x6: regs[6], x7: regs[7],
                x8: regs[8], x9: regs[9], x10: regs[10], x11: regs[11],
                x12: regs[12], x13: regs[13], x14: regs[14], x15: regs[15],
                x16: regs[16], x17: regs[17], x18: regs[18], x19: regs[19],
                x20: regs[20], x21: regs[21], x22: regs[22], x23: regs[23],
                x24: regs[24], x25: regs[25], x26: regs[26], x27: regs[27],
                x28: regs[28], x29: regs[29], x30: regs[30],
                sp_el0: core::ptr::addr_of!((*p).sp_el0).read_volatile(),
                elr_el1: core::ptr::addr_of!((*p).elr_el1).read_volatile(),
                spsr_el1: core::ptr::addr_of!((*p).spsr_el1).read_volatile(),
                tpidr_el0: core::ptr::addr_of!((*p).tpidr_el0).read_volatile(),
                _padding: 0,
            }
        }
    }

    /// `N` words of the frame's save area **past** the register block - the NEON
    /// registers the sync handler stores after `UserTrapFrame`'s fields.
    ///
    /// `off` is a byte offset into the 832-byte frame, so the caller names its own
    /// layout (`akuma-threading`'s `SYNC_FRAME_NEON_OFFSET`) and this crate does not
    /// duplicate it. A handle is only ever made for the EL0 **sync** handler's frame,
    /// so the sync layout is the only one a caller can mean; an IRQ frame has its
    /// NEON block 16 bytes earlier and has no handle.
    ///
    /// # Panics
    /// If `off` is not 8-aligned, starts inside the register block, or the range
    /// runs past the end of the frame.
    #[must_use]
    #[track_caller]
    pub fn read_save_words<const N: usize>(self, off: usize) -> [u64; N] {
        Self::check_save_range(off, N);
        let mut out = [0u64; N];
        let base = self.ptr.as_ptr().cast::<u64>();
        let mut i = 0;
        while i < N {
            // SAFETY: the module contract makes `base` an 832-byte live frame; the
            // range check above keeps word `i` inside it past the register block,
            // 8-aligned (`off % 8 == 0`, frame 16-aligned by the asm).
            out[i] = unsafe { base.add(off / 8 + i).read_volatile() };
            i += 1;
        }
        out
    }

    /// Store `words` at byte offset `off` of the save area: the other half of
    /// [`read_save_words`](Self::read_save_words), with the same range rules.
    #[track_caller]
    pub fn write_save_words<const N: usize>(self, off: usize, words: &[u64; N]) {
        Self::check_save_range(off, N);
        let base = self.ptr.as_ptr().cast::<u64>();
        let mut i = 0;
        while i < N {
            // SAFETY: as [`Self::read_save_words`]; one aligned `u64` store.
            unsafe { base.add(off / 8 + i).write_volatile(words[i]) };
            i += 1;
        }
    }

    #[track_caller]
    fn check_save_range(off: usize, words: usize) {
        assert!(off.is_multiple_of(8), "save-area offset must be 8-aligned");
        assert!(off >= core::mem::size_of::<UserTrapFrame>(), "save area starts inside the register block");
        assert!(off + words * 8 <= FRAME_BYTES, "save-area range runs past the frame");
    }

    /// Write every register field of `f` back to the live frame - the other half
    /// of [`snapshot`](Self::snapshot), for a handler (`rt_sigreturn`) that
    /// rebuilds the whole user context off to the side and installs it once.
    pub fn store(self, f: &UserTrapFrame) {
        let regs = [
            f.x0, f.x1, f.x2, f.x3, f.x4, f.x5, f.x6, f.x7, f.x8, f.x9, f.x10, f.x11, f.x12, f.x13,
            f.x14, f.x15, f.x16, f.x17, f.x18, f.x19, f.x20, f.x21, f.x22, f.x23, f.x24, f.x25,
            f.x26, f.x27, f.x28, f.x29, f.x30,
        ];
        let p = self.ptr.as_ptr();
        // SAFETY: as [`Self::snapshot`]; each store is one aligned `u64` field.
        unsafe {
            let mut i = 0;
            while i < GPRS {
                p.cast::<u64>().add(i).write_volatile(regs[i]);
                i += 1;
            }
            core::ptr::addr_of_mut!((*p).sp_el0).write_volatile(f.sp_el0);
            core::ptr::addr_of_mut!((*p).elr_el1).write_volatile(f.elr_el1);
            core::ptr::addr_of_mut!((*p).spsr_el1).write_volatile(f.spsr_el1);
            core::ptr::addr_of_mut!((*p).tpidr_el0).write_volatile(f.tpidr_el0);
        }
    }

    /// `SP_EL0` as saved at exception entry.
    #[must_use]
    pub fn sp_el0(self) -> u64 {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of!((*self.ptr.as_ptr()).sp_el0).read_volatile() }
    }

    /// `SPSR_EL1` as saved at exception entry.
    #[must_use]
    pub fn spsr(self) -> u64 {
        // SAFETY: as [`Self::x0`].
        unsafe { core::ptr::addr_of!((*self.ptr.as_ptr()).spsr_el1).read_volatile() }
    }
}

/// The AArch64 **IRQ trap frame**, single-sourced.
///
/// The 832-byte frame the IRQ/SGI vector asm pushes (and which
/// `setup_fake_irq_frame` synthesizes for a thread's first resume): the offsets
/// written once here, and one reader the handler crates share.
///
/// # Why a second frame type, next to [`FrameHandle`]
///
/// This is *not* the EL0 [`UserTrapFrame`] layout: there byte 240 is `x30` and
/// byte 248 is `sp_el0`; here they are `ELR_EL1` and `SPSR_EL1`, and the frame is
/// 832 bytes (a NEON/FP block sits behind the GPRs), not 288.
///
/// [`FrameHandle`]'s contract — the frame *this handler* just pushed, live and
/// exclusively its — does not fit a saved frame: it is read by *other* threads
/// (the scheduler on a peer core, a hang dump), exactly the aliasing
/// `FrameHandle` forbids. So: constants and a tiny `Copy` value type, with the
/// one `unsafe` at [`IrqFrame::at`], where the frame's liveness is promised. The
/// writer (`setup_fake_irq_frame`) goes through the same [`IrqFrame::write_elr`]
/// / [`IrqFrame::write_spsr`], so the synthetic frame and every reader cannot
/// drift apart.
pub mod irq_frame {
    /// Byte offset of the saved `ELR_EL1` in the frame: word 30 of the vector's
    /// store order (x30 first, x0/x1 last, then ELR, SPSR).
    pub const ELR: usize = 240;
    /// Byte offset of the saved `SPSR_EL1` in the frame: word 31.
    pub const SPSR: usize = 248;
    /// Size of the whole frame the SGI/IRQ vector pushes.
    ///
    /// 304 bytes of GPR block (x30..x0, ELR, SPSR, SP_EL0, TPIDR_EL0, x10/x11)
    /// plus the 528-byte NEON/FP block — the same constant `akuma-threading`'s
    /// `setup_fake_irq_frame` sizes and zeroes its synthetic frame with.
    pub const SIZE: usize = akuma_exec_core::thread::IRQ_FRAME_SIZE;

    /// A saved or live IRQ trap frame at `sp`.
    ///
    /// Reads and writes are volatile single words — the same instruction the raw
    /// `read_volatile((sp + 24x) as *const u64)` they replace was — so a frame a
    /// peer core is concurrently building or restoring cannot tear one.
    #[derive(Clone, Copy)]
    pub struct IrqFrame {
        sp: usize,
    }

    impl IrqFrame {
        /// Name the IRQ frame at `sp`.
        ///
        /// # Safety
        /// `sp` points at an IRQ frame of [`SIZE`] bytes on a kernel stack that
        /// outlives this value: the live frame the vector asm just built, or the
        /// `Context.sp` of a thread that is not running (its stack cannot be
        /// freed or reused while it is not running).
        #[must_use]
        pub unsafe fn at(sp: u64) -> Self {
            Self { sp: sp as usize }
        }

        /// The saved `ELR_EL1` — where the frame's `eret` resumes.
        #[must_use]
        pub fn elr(self) -> u64 {
            // SAFETY: `at`'s contract places `SIZE` bytes of frame at `sp`; `ELR`
            // is one aligned `u64` inside it.
            unsafe { core::ptr::read_volatile((self.sp + ELR) as *const u64) }
        }

        /// The saved `SPSR_EL1`.
        #[must_use]
        pub fn spsr(self) -> u64 {
            // SAFETY: as [`Self::elr`].
            unsafe { core::ptr::read_volatile((self.sp + SPSR) as *const u64) }
        }

        /// Does the frame's `eret` return to EL0 (userspace)?
        ///
        /// `SPSR.M[3:0] == 0` (EL0t) — the read every BKL reconcile and
        /// scheduler-displacement decision keys on.
        #[must_use]
        pub fn from_el0(self) -> bool {
            self.spsr().trailing_zeros() >= 4
        }

        /// Overwrite the saved `ELR_EL1` — the writer half
        /// (`setup_fake_irq_frame` plants the thread's entry point here), kept in
        /// this module so the synthetic frame and the readers share one layout.
        pub fn write_elr(self, v: u64) {
            // SAFETY: as [`Self::elr`], mirrored to a write.
            unsafe { core::ptr::write_volatile((self.sp + ELR) as *mut u64, v) }
        }

        /// Overwrite the saved `SPSR_EL1` — the writer half (`setup_fake_irq_frame`
        /// plants the EL/IRQ-mask state the first `eret` enters with).
        pub fn write_spsr(self, v: u64) {
            // SAFETY: as [`Self::elr`], mirrored to a write.
            unsafe { core::ptr::write_volatile((self.sp + SPSR) as *mut u64, v) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> UserTrapFrame {
        UserTrapFrame {
            x0: 100, x1: 101, x2: 102, x3: 103, x4: 104, x5: 105, x6: 106, x7: 107,
            x8: 108, x9: 109, x10: 110, x11: 111, x12: 112, x13: 113, x14: 114, x15: 115,
            x16: 116, x17: 117, x18: 118, x19: 119, x20: 120, x21: 121, x22: 122, x23: 123,
            x24: 124, x25: 125, x26: 126, x27: 127, x28: 128, x29: 129, x30: 130,
            sp_el0: 200, elr_el1: 201, spsr_el1: 202, tpidr_el0: 203, _padding: 0,
        }
    }

    #[test]
    fn accessors_read_and_write_the_named_fields() {
        let mut f = frame();
        // SAFETY: a live local frame, exclusively this test's.
        let h = unsafe { FrameHandle::new(&raw mut f) };
        assert_eq!((h.x0(), h.x8(), h.elr(), h.sp_el0(), h.spsr()), (100, 108, 201, 200, 202));
        h.set_x0(7);
        h.set_elr(h.elr() - 4);
        h.set_tpidr_el0(9);
        assert_eq!((f.x0, f.elr_el1, f.tpidr_el0), (7, 197, 9));
    }

    #[test]
    fn reg_indexes_x0_through_x30_in_order() {
        let mut f = frame();
        // SAFETY: as above.
        let h = unsafe { FrameHandle::new(&raw mut f) };
        for n in 0..GPRS {
            assert_eq!(h.reg(n), 100 + n as u64);
        }
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn reg_31_is_refused() {
        let mut f = frame();
        // SAFETY: as above.
        let h = unsafe { FrameHandle::new(&raw mut f) };
        let _ = h.reg(31);
    }

    #[test]
    fn snapshot_is_a_copy_not_a_view() {
        let mut f = frame();
        // SAFETY: as above.
        let h = unsafe { FrameHandle::new(&raw mut f) };
        let snap = h.snapshot();
        h.set_x0(0);
        assert_eq!((snap.x0, snap.x30, snap.sp_el0, snap.elr_el1, snap.spsr_el1, snap.tpidr_el0), (100, 130, 200, 201, 202, 203));
        assert_eq!(h.snapshot().x0, 0);
        // And `store` is the inverse: a rebuilt context goes back in whole.
        let mut rebuilt = h.snapshot();
        rebuilt.x1 = 1;
        rebuilt.x30 = 30;
        rebuilt.elr_el1 = 77;
        h.store(&rebuilt);
        assert_eq!((f.x1, f.x30, f.elr_el1, f.x2, f.sp_el0), (1, 30, 77, 102, 200));
    }

    #[test]
    #[should_panic(expected = "null frame")]
    fn a_null_frame_is_refused() {
        // SAFETY: the null check is what is under test.
        let _ = unsafe { FrameHandle::new(core::ptr::null_mut()) };
    }
}

#[cfg(test)]
mod irq_frame_tests {
    use super::irq_frame::{self, IrqFrame};
    use super::UserTrapFrame;

    #[test]
    fn round_trips_through_a_synthetic_frame() {
        // `setup_fake_irq_frame`'s shape: `SIZE` zeroed bytes, word 30 = ELR,
        // word 31 = SPSR. `[u64; 104]` is exactly the frame, so the size the
        // vector pushes and this test builds are pinned together.
        let mut frame = [0u64; 104];
        assert_eq!(core::mem::size_of_val(&frame), irq_frame::SIZE);
        frame[30] = 0x0001_2345_6789;
        frame[31] = 0x345; // EL1h, IRQs enabled — M[3:0] = 5, not EL0
        // SAFETY: `frame` is a live `SIZE`-byte frame for this whole test body.
        let f = unsafe { IrqFrame::at(frame.as_ptr() as u64) };
        assert_eq!(f.elr(), 0x0001_2345_6789);
        assert_eq!(f.spsr(), 0x345);
        assert!(!f.from_el0());
        // The writer half lands in exactly the words the readers read.
        f.write_elr(0x4000_0000);
        f.write_spsr(0x3C5); // EL1h, IRQs disabled
        assert_eq!((frame[30], frame[31]), (0x4000_0000, 0x3C5));
        assert_eq!((f.elr(), f.spsr()), (0x4000_0000, 0x3C5));
        assert!(!f.from_el0());
        // M[3:0] == 0 is the EL0t the BKL reconcile and scheduler decisions key on.
        f.write_spsr(0);
        assert!(f.from_el0());
    }

    #[test]
    fn offsets_are_not_user_trap_frame_offsets() {
        // The reason `IrqFrame` exists next to `FrameHandle`: 240/248 are IRQ-frame
        // words 30/31, and in the EL0 `UserTrapFrame` layout those same offsets are
        // `x30` and `sp_el0`. Reinterpreting an IRQ frame as a `UserTrapFrame` would
        // read the link register as the resume point and SP_EL0 as the PSR — and the
        // frame would not even be long enough (288 bytes vs 832). This pins the two
        // layouts apart so a change to either cannot silently reuse the other's
        // offsets.
        assert_eq!(core::mem::offset_of!(UserTrapFrame, x30), irq_frame::ELR);
        assert_eq!(core::mem::offset_of!(UserTrapFrame, sp_el0), irq_frame::SPSR);
        assert_ne!(core::mem::offset_of!(UserTrapFrame, elr_el1), irq_frame::ELR);
        assert_ne!(core::mem::offset_of!(UserTrapFrame, spsr_el1), irq_frame::SPSR);
        assert_ne!(core::mem::size_of::<UserTrapFrame>(), irq_frame::SIZE);
    }
}

#[cfg(test)]
mod save_area_tests {
    use super::{FRAME_BYTES, FrameHandle, UserTrapFrame};

    // SAFETY (all): a zeroed `[u64; 104]` is exactly the 832-byte frame, 8-aligned, live and
    // unaliased for the test.
    fn handle(buf: &mut [u64; 104]) -> FrameHandle {
        assert_eq!(core::mem::size_of_val(buf), FRAME_BYTES);
        unsafe { FrameHandle::new(buf.as_mut_ptr().cast::<UserTrapFrame>()) }
    }

    #[test]
    fn save_words_round_trip_and_touch_only_their_range() {
        let mut buf = [0u64; 104];
        let h = handle(&mut buf);
        let w: [u64; 66] = core::array::from_fn(|i| 0x1000 + i as u64);
        h.write_save_words(304, &w);
        assert_eq!(h.read_save_words::<66>(304), w);
        drop(h);
        assert!(buf[..38].iter().all(|&x| x == 0), "register block and padding untouched");
        assert_eq!(buf[38], 0x1000);
        assert_eq!(buf[103], 0x1000 + 65);
    }

    #[test]
    #[should_panic(expected = "past the frame")]
    fn save_words_past_the_frame_panic() {
        let mut buf = [0u64; 104];
        let _ = handle(&mut buf).read_save_words::<67>(304);
    }

    #[test]
    #[should_panic(expected = "inside the register block")]
    fn save_words_inside_the_registers_panic() {
        let mut buf = [0u64; 104];
        let _ = handle(&mut buf).read_save_words::<2>(8);
    }
}
