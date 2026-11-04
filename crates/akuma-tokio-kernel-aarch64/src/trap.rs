//! Traps and the tick: `akuma-vectors-aarch64`'s four entries, wired to this
//! kernel's handlers, and the virtual timer (CNTV, PPI 27) through the GIC.
//!
//! The kernel runs with IRQs masked; userspace runs with them unmasked, and the
//! idle loop opens one-instruction windows. So an EL1 IRQ is the idle window's
//! tick and an EL0 IRQ interrupted a user thread.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use akuma_primitives::safe_print;
use akuma_vectors_aarch64::{Frame, Kind, ec};

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
    if id == TIMER_INTID {
        TICKS.fetch_add(1, Relaxed);
        akuma_timer::arm_periodic_tick();
    }
    akuma_gic::end_of_interrupt(id);
    let _ = f;
}

fn el1_sync(f: &mut Frame) {
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
    let what = match f.ec() {
        ec::SVC64 => "svc",
        ec::DABT_LOWER => "data abort",
        ec::IABT_LOWER => "instruction abort",
        _ => "trap",
    };
    safe_print!(
        192,
        "[Fault] EL0 {} ec=0x{:x} elr=0x{:016x} far=0x{:016x} esr=0x{:x} -- no userspace yet\n",
        what,
        f.ec(),
        f.elr,
        f.far,
        f.esr
    );
    crate::halt();
}

fn unexpected(f: &mut Frame) {
    safe_print!(160, "[Fault] unexpected vector esr=0x{:x} elr=0x{:016x}\n", f.esr, f.elr);
    crate::halt();
}
