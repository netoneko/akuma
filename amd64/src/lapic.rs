//! Local APIC: the first hardware interrupt source.
//!
//! Stage D. Everything before this ran with `IF` clear from `boot.s` onward —
//! the kernel could fault, but nothing could ever *interrupt* it. A timer tick
//! is the prerequisite for preemption, and therefore for a scheduler.
//!
//! # No ACPI, again
//!
//! The LAPIC does not need to be discovered. Its physical base is in
//! `IA32_APIC_BASE` (MSR `0x1B`), so unlike the IOAPIC — which genuinely needs
//! the ACPI MADT — this is one `rdmsr` away. That is why a preemption timer sits
//! *before* ACPI in the plan rather than after it.
//!
//! # The two things that are easy to get wrong
//!
//! **The LAPIC page must be mapped uncacheable.** It lives at `0xFEE0_0000`,
//! above the 1 GiB `boot.s` identity-maps, so it has to be mapped explicitly —
//! and mapped [`MemAttr::Device`]. A writeback-cached device mapping lets the CPU
//! satisfy a read from cache and never issue the access, which makes a polled
//! register appear frozen. This is the first consumer of `MemAttr`, and it is why
//! that type exists.
//!
//! **The legacy 8259 PICs must be masked before `sti`.** They power on with
//! interrupts unmasked and vectors overlapping the CPU exception range, so
//! enabling interrupts without masking them invites a spurious IRQ that decodes
//! as, say, a `#GP` with a garbage error code. Masking is four `outb`s and it is
//! not optional.

use crate::idt;
use crate::paging::{self, MemAttr, Prot};
use crate::phys::DEVMAP_BASE;
use crate::port::outb;
use akuma_selftest::Suite;

use crate::serial;
use core::sync::atomic::{AtomicU64, Ordering};

/// `IA32_APIC_BASE`. Bits 12:35 are the base address; bit 11 is the global
/// enable; bit 8 marks the bootstrap processor.
const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_MASK: u64 = 0x000f_ffff_ffff_f000;
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;

// Register offsets from the LAPIC base.
const REG_ID: usize = 0x020;
const REG_EOI: usize = 0x0B0;
/// Spurious Interrupt Vector Register. Bit 8 is the software enable.
const REG_SVR: usize = 0x0F0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_DIV: usize = 0x3E0;

/// Vector for the LAPIC timer. Must be >= 32; 0..31 are CPU exceptions.
pub const TIMER_VECTOR: u8 = 32;
/// Vector the LAPIC raises when it has nothing better to deliver.
const SPURIOUS_VECTOR: u8 = 0xFF;

/// Periodic mode: bits 18:17 = 01.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// LVT mask bit, present in every local vector table entry.
const LVT_MASKED: u32 = 1 << 16;

/// Divide by 16 (`0b0011` across bits 3,1:0 — bit 2 is reserved and stays 0).
const TIMER_DIV_16: u32 = 0b0011;

/// Ticks observed. Written only by the handler, read by everyone else.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The LAPIC's *virtual* base in the device window, or 0 before [`init`].
static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reading an architectural MSR; `IA32_APIC_BASE` exists on every
    // x86_64 part and reading it has no side effect.
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// # Safety
/// Writing an MSR can reconfigure the CPU. The one write here sets the LAPIC
/// global-enable bit, leaving the base address as firmware chose it.
unsafe fn wrmsr(msr: u32, val: u64) {
    // SAFETY: caller's obligation.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") val as u32,
                         in("edx") (val >> 32) as u32,
                         options(nostack, preserves_flags));
    }
}

fn reg_ptr(offset: usize) -> *mut u32 {
    (LAPIC_BASE.load(Ordering::Relaxed) as usize + offset) as *mut u32
}

fn read(offset: usize) -> u32 {
    // SAFETY: the LAPIC page is mapped by `init` before any call reaches here,
    // and every offset is an architectural LAPIC register.
    unsafe { reg_ptr(offset).read_volatile() }
}

fn write(offset: usize, val: u32) {
    // SAFETY: as `read`.
    unsafe { reg_ptr(offset).write_volatile(val) };
}

/// Signal end-of-interrupt. Every handler for a LAPIC-delivered vector must do
/// this before returning, or the LAPIC will not deliver that priority again.
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Ticks seen so far.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called from the timer vector.
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::sched::set_need_resched();
    eoi();
}

/// Mask both legacy 8259 PICs.
///
/// Firecracker and QEMU both present them, and they power up able to deliver on
/// vectors that overlap the CPU exception range. Nothing in this kernel uses
/// them — the serial console is polled — so they are masked rather than
/// remapped.
fn mask_legacy_pics() {
    // SAFETY: 0x21 and 0xA1 are the architectural 8259 data ports; writing all
    // ones masks every line, which is the conservative direction.
    unsafe {
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
    }
}

/// Map the LAPIC, enable it, and start a periodic timer. Interrupts stay masked;
/// the caller decides when to `sti`.
pub fn init() -> bool {
    mask_legacy_pics();

    let base_msr = rdmsr(IA32_APIC_BASE);
    let base = base_msr & APIC_BASE_MASK;
    if base == 0 {
        serial::puts("  [FATAL] IA32_APIC_BASE reports no LAPIC\n");
        return false;
    }

    // Set the global enable bit if firmware left it clear, preserving the base.
    // SAFETY: sets exactly one bit in an architectural MSR, leaving the address
    // field as found.
    unsafe { wrmsr(IA32_APIC_BASE, base_msr | APIC_GLOBAL_ENABLE) };

    // Map the register page into the *device* window, uncached.
    //
    // Not the physmap: that covers the first GiB with 2 MiB writeback pages, and
    // 0xFEE0_0000 falls inside it — so the LAPIC already has a cached alias
    // there. Splitting that 2 MiB page to change one 4 KiB entry's cacheability
    // would work; a second window is simpler and states the intent. Two mappings
    // of one physical page with different cacheability is the whole reason
    // `MemAttr` exists.
    let va = DEVMAP_BASE + base;
    if !paging::map_page(va as usize, base, Prot::KERNEL_RW, MemAttr::Device) {
        serial::puts("  [FATAL] could not map the LAPIC page\n");
        return false;
    }
    LAPIC_BASE.store(va, Ordering::Relaxed);

    // Software-enable, and park spurious deliveries on a vector of their own so
    // they are distinguishable from a real one.
    write(REG_SVR, (1 << 8) | u32::from(SPURIOUS_VECTOR));

    idt::set_handler(TIMER_VECTOR, idt::timer_interrupt_entry());

    start_timer();

    serial::puts("  lapic: base=0x");
    serial::put_hex(base);
    serial::puts(" id=");
    serial::put_dec(u64::from(read(REG_ID) >> 24));
    serial::puts(" timer vector=");
    serial::put_dec(u64::from(TIMER_VECTOR));
    serial::puts(" periodic\n");
    true
}

/// Arm the periodic timer.
///
/// The initial count is deliberately **uncalibrated**. The LAPIC counts at the
/// core crystal frequency, which needs CPUID leaf `0x15` or calibration against
/// another clock to convert into wall time — and nothing here needs wall time
/// yet. This value only has to tick fast enough for a bounded spin loop to
/// observe without waiting all day — it was 1_000_000 until the scheduler test
/// needed several ticks inside one short workload and saw none.
pub fn start_timer() {
    write(REG_TIMER_DIV, TIMER_DIV_16);
    write(REG_LVT_TIMER, u32::from(TIMER_VECTOR) | LVT_TIMER_PERIODIC);
    write(REG_TIMER_INIT, 100_000);
}

/// Stop the timer. Used after the smoke test so later output is not interleaved
/// with ticks.
pub fn stop_timer() {
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_INIT, 0);
}

/// Enable interrupts and confirm ticks actually arrive.
///
/// Bounded by a spin budget rather than trusting the timer: if interrupts never
/// arrive this must report a failure, not hang the boot.
pub fn smoke_test(t: &mut Suite) {
    const WANT: u64 = 5;
    const BUDGET: u64 = 500_000_000;

    // SAFETY: the IDT is loaded, the timer vector has a handler, the legacy PICs
    // are masked, and the LAPIC is configured. This is the first `sti` in the
    // kernel's life and everything it can deliver is accounted for.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }

    let mut spins = 0u64;
    while ticks() < WANT && spins < BUDGET {
        spins += 1;
        core::hint::spin_loop();
    }

    // SAFETY: masking interrupts is always the conservative direction.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    stop_timer();

    t.check("lapic: timer interrupts arrive", ticks() >= WANT);
    // A measurement, not an assertion: the count differs by an order of
    // magnitude between emulation and real silicon (QEMU ~6e4, Zen 4 ~1e7 for
    // the same five ticks), which is itself evidence the ticks come from a
    // clock rather than from anything correlated with instruction count.
    t.note("lapic: spins waiting for 5 ticks", spins);
}
