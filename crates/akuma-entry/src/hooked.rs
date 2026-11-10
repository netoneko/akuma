//! The root kernel's trap front end under the hooked scheduler (`hooked-sched`, M6).
//!
//! D4 (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`): the AArch64 full kernel takes the
//! tokio kernels' vector table, `akuma-vectors-aarch64`, instead of growing a second
//! hooked front end beside [`crate::exceptions`]. Its stubs save the whole register
//! file on the current thread's kernel stack and hand a `&mut Frame` to the handler
//! registered here; a switch inside a handler is an ordinary call, and the frame stays
//! where it is until the thread is resumed and the stub restores it.
//!
//! The handlers are thin: the logic is `akuma-exceptions`' — [`exc::sync_el0`] for
//! syscalls and user faults, [`exc::sync_el1`] for the kernel's own faults (the
//! user-copy fixup, lazy and copy-on-write pages touched from EL1), and
//! [`exc::irq_hooked`] for interrupts. What is here is the translation from the kit's
//! frame to what those take.
//!
//! # The frame is the root kernel's frame
//!
//! The kit's `Frame` and the SGI path's EL0 sync frame are the same 832 bytes: x0-x30
//! at 0, `SP_EL0` 248, `ELR_EL1` 256, `SPSR_EL1` 264, `TPIDR_EL0` 272, the NEON area at
//! `SYNC_FRAME_NEON_OFFSET` (304) with `FPCR`/`FPSR` at 816/824. The kit keeps the
//! syndrome at 280/288, where the SGI frame has padding and its scratch word. So
//! [`FrameHandle`] — `akuma-exceptions`' typed view, `UserTrapFrame` plus the NEON save
//! area — names a kit frame with no copy; the assertions below pin that.

use akuma_exceptions as exc;
use akuma_trap_aarch64::{FrameHandle, UserTrapFrame};
use akuma_vectors_aarch64::{Frame, Kind};

const _: () = {
    assert!(core::mem::size_of::<UserTrapFrame>() == 288);
    assert!(core::mem::offset_of!(UserTrapFrame, sp_el0) == core::mem::offset_of!(Frame, sp));
    assert!(core::mem::offset_of!(UserTrapFrame, elr_el1) == core::mem::offset_of!(Frame, elr));
    assert!(core::mem::offset_of!(UserTrapFrame, spsr_el1) == core::mem::offset_of!(Frame, spsr));
    assert!(core::mem::offset_of!(UserTrapFrame, tpidr_el0) == core::mem::offset_of!(Frame, tpidr));
    assert!(core::mem::offset_of!(Frame, q) == akuma_exec::threading::sigframe::SYNC_FRAME_NEON_OFFSET);
    assert!(core::mem::size_of::<Frame>() == akuma_trap_aarch64::FRAME_BYTES);
    assert!(core::mem::align_of::<Frame>() >= core::mem::align_of::<UserTrapFrame>());
};

/// Install the kit's vectors with this kernel's handlers on the boot core, and open
/// interrupts: the [`crate::exceptions::init`] of this path. Secondaries take the same
/// table with [`install_on_this_core`].
pub fn init() {
    crate::exceptions::init_exception_stack();
    crate::exceptions::register_services();
    akuma_vectors_aarch64::register(Kind::El1Sync, el1_sync);
    akuma_vectors_aarch64::register(Kind::El1Irq, el1_irq);
    akuma_vectors_aarch64::register(Kind::El0Sync, el0_sync);
    akuma_vectors_aarch64::register(Kind::El0Irq, el0_irq);
    akuma_vectors_aarch64::register(Kind::Unexpected, unexpected);
    install_on_this_core();
    // The root kernel runs EL1 with interrupts open, as on the SGI path: device and
    // timer interrupts are serviced while kernel code runs, and only an interrupt that
    // arrived from EL0 switches threads.
    akuma_primitives::irq::unmask_irqs();
}

/// Point this core's `VBAR_EL1` at the kit's table.
pub fn install_on_this_core() {
    akuma_vectors_aarch64::install();
}

/// A syscall or a user fault. `akuma-exceptions` takes and releases the BKL itself.
/// Interrupts are open for the excursion, as the SGI path's stub opens them, and closed
/// again before the stub restores the frame.
fn el0_sync(f: &mut Frame) {
    let (esr, far) = (f.esr, f.far);
    // SAFETY: `f` is the frame the kit's stub pushed for this entry on the current
    // thread's kernel stack, exclusively this handler's until it returns (the kit's one
    // contract), and its first 288 bytes and NEON area are `UserTrapFrame` and the sync
    // save area at the offsets the assertions above pin. The handle is not used after
    // `sync_el0` returns.
    let frame = unsafe { FrameHandle::new(core::ptr::from_mut(f).cast::<UserTrapFrame>()) };
    akuma_primitives::irq::unmask_irqs_sync();
    let ret = exc::sync_el0(frame, esr, far);
    akuma_primitives::irq::mask_irqs_sync();
    f.x[0] = ret;
}

/// A fault taken by kernel code. The registers `akuma-exceptions` reads are the ones the
/// SGI path's stub hands it (`[x2, x3, x0, x1, x29, x30]`); a resumption elsewhere is a
/// new `ELR`, which the kit's stub restores.
fn el1_sync(f: &mut Frame) {
    let regs = [f.x[2], f.x[3], f.x[0], f.x[1], f.x[29], f.x[30]];
    match exc::sync_el1(Some(regs)) {
        exc::El1Resume::Retry => {}
        exc::El1Resume::At(addr) => f.elr = addr,
        exc::El1Resume::RecoveryPad => f.elr = crate::exceptions::el1_fault_recovery_pad as *const () as usize as u64,
    }
}

fn el0_irq(_f: &mut Frame) {
    exc::irq_hooked(true);
}

fn el1_irq(_f: &mut Frame) {
    exc::irq_hooked(false);
}

fn unexpected(f: &mut Frame) {
    akuma_primitives::safe_print!(
        160,
        "[Fault] unexpected vector esr=0x{:x} elr=0x{:016x} spsr=0x{:x}\n",
        f.esr,
        f.elr,
        f.spsr
    );
    loop {
        akuma_cpu::park::wfi();
    }
}
