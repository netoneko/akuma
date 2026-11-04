//! Traps and the tick: `akuma-vectors-aarch64`'s four entries, wired to this
//! kernel's handlers, and the virtual timer (CNTV, PPI 27) through the GIC.
//!
//! The kernel runs with IRQs masked; userspace runs with them unmasked, and the
//! idle loop opens one-instruction windows. So an EL1 IRQ is the idle window's
//! tick and an EL0 IRQ interrupted a user thread.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::task::USER_TOP;
use akuma_vectors_aarch64::{Frame, Kind, ec};

use crate::mm::Aarch64;

/// The virtual timer's PPI.
pub const TIMER_INTID: u32 = 27;
/// Spurious / no pending interrupt.
const SPURIOUS: u32 = 1023;
/// The tick: 10 ms, the amd64 half's LAPIC rate.
pub const TICK_US: u64 = 10_000;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Ticks since the timer was armed.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Relaxed)
}

/// Install the vectors and their handlers. Before anything can trap.
pub fn init() {
    akuma_vectors_aarch64::register(Kind::El1Sync, el1_sync);
    akuma_vectors_aarch64::register(Kind::El1Irq, irq);
    akuma_vectors_aarch64::register(Kind::El0Sync, el0_sync);
    akuma_vectors_aarch64::register(Kind::El0Irq, irq);
    akuma_vectors_aarch64::register(Kind::Unexpected, unexpected);
    akuma_vectors_aarch64::install();
}

/// The GIC and the periodic tick. After the device map is installed.
pub fn start_tick() {
    akuma_gic::init();
    akuma_gic::enable_irq(TIMER_INTID);
    akuma_timer::set_tick_us(TICK_US);
    akuma_timer::arm_periodic_tick();
}

fn irq(f: &mut Frame) {
    let Some(id) = akuma_gic::acknowledge_irq() else {
        return;
    };
    if id == SPURIOUS {
        return;
    }
    let tick = id == TIMER_INTID;
    if tick {
        TICKS.fetch_add(1, Relaxed);
        akuma_timer::arm_periodic_tick();
    }
    akuma_gic::end_of_interrupt(id);
    // A tick that interrupted EL0 is the preemption point: the thread's user
    // state is in `f`, on its own kernel stack, so switching away here and
    // coming back later resumes it exactly. A tick in the kernel (an idle
    // window) only counts.
    if tick && f.from_el0() {
        let _ = akuma_threading::x86_yield();
        if akuma_tokio_kernel_core::proc::kill_requested::<Aarch64>() {
            akuma_tokio_kernel_core::proc::finish_killed::<Aarch64>();
        }
        akuma_tokio_kernel_core::signal::deliver_fatal_on_tick::<Aarch64>();
    }
    // The pump's backstop: a user thread that never enters the kernel would
    // otherwise starve the NIC (no interrupt of its own here). A lap per tick
    // bounds that at 10 ms; `netpoll` guards its own reentry.
    if tick {
        crate::net::netpoll();
    }
}

/// Data/instruction fault status: translation (a page not mapped yet).
const fn is_translation(esr: u64) -> bool {
    esr & 0x3C == 0x04
}

/// Data/instruction fault status: permission (mapped, not for this access).
const fn is_permission(esr: u64) -> bool {
    esr & 0x3C == 0x0C
}

/// A data abort that wrote (`WnR`).
const fn is_write(esr: u64) -> bool {
    esr & (1 << 6) != 0
}

/// A fault on a user address, from either level: a lazy page filled, or a
/// copy-on-write share broken. `true` when the access can be retried.
fn user_fault(far: u64, esr: u64, data: bool, files: bool) -> bool {
    use akuma_tokio_kernel_core::{cow, lazy};
    if far >= USER_TOP {
        return false;
    }
    let root = akuma_boot_aarch64::user::live_root();
    if is_translation(esr) && lazy::fill::<Aarch64>(root, far, files) == lazy::Fill::Present {
        return true;
    }
    data && is_permission(esr) && is_write(esr) && cow::resolve::<Aarch64>(root, far).resolves()
}

fn el1_sync(f: &mut Frame) {
    // The kernel touching user memory: glue writes user buffers directly, so a
    // copy-on-write page or a lazy one can fault here. Only a zero fill from
    // EL1 (a file fill reads the filesystem, which this path may hold).
    if f.ec() == ec::DABT_SAME && user_fault(f.far, f.esr, true, false) {
        return;
    }
    // A fault inside the user-copy loop is a bad user pointer: `EFAULT`.
    let fixup = akuma_primitives::preempt::get_user_copy_fault_handler();
    if f.ec() == ec::DABT_SAME && f.far < USER_TOP && fixup != 0 {
        f.elr = fixup;
        return;
    }
    safe_print!(
        192,
        "[Fault] EL1 ec=0x{:x} elr=0x{:016x} far=0x{:016x} esr=0x{:x} sp=0x{:x}\n",
        f.ec(),
        f.elr,
        f.far,
        f.esr,
        f.sp
    );
    crate::halt();
}

fn el0_sync(f: &mut Frame) {
    match f.ec() {
        ec::SVC64 => return crate::syscall::svc(f),
        ec::DABT_LOWER if user_fault(f.far, f.esr, true, true) => return,
        ec::IABT_LOWER if user_fault(f.far, f.esr, false, true) => return,
        _ => {}
    }
    safe_print!(
        192,
        "[Fault] EL0 ec=0x{:x} elr=0x{:016x} sp=0x{:016x} far=0x{:016x} esr=0x{:x} -- task killed\n",
        f.ec(),
        f.elr,
        f.sp,
        f.far,
        f.esr
    );
    // Signals are A5.8: a fault ends the process as SIGSEGV would.
    crate::proc::die_segv()
}

fn unexpected(f: &mut Frame) {
    safe_print!(160, "[Fault] unexpected vector esr=0x{:x} elr=0x{:016x}\n", f.esr, f.elr);
    crate::halt();
}
