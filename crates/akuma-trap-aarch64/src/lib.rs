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
