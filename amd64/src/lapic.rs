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
//!
//! # One page, one register block per core
//!
//! Every core's LAPIC answers at the same physical address — `0xFEE0_0000` is
//! not one device, it is each core's own, reached through one mapping. So the
//! page is mapped once ([`init`], on the BSP) and every core then talks to its
//! own APIC through the same virtual address: [`init_ap`] enables and starts the
//! timer on the core it runs on, [`eoi`] acknowledges on the core that took the
//! interrupt, and the ICR writes in [`send_init`]/[`send_startup`] go out from
//! whichever core issues them. `TICKS` counts the BSP's ticks only, because it is
//! the clock (`net::uptime_us`); with N cores counting into one word, time would
//! run N times fast.

use crate::idt;
use crate::paging::{self, MemAttr, Prot};
use crate::phys::DEVMAP_BASE;
use crate::port::outb;
use akuma_selftest::Suite;

use crate::serial;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
/// Interrupt Command Register, low half: delivery mode, destination shorthand,
/// vector, and the delivery-status bit (12) that says a previous IPI is still
/// being sent.
const REG_ICR_LOW: usize = 0x300;
/// ICR high half: the destination APIC id in bits 31:24.
const REG_ICR_HIGH: usize = 0x310;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
/// Current count — the timer counting down from `REG_TIMER_INIT`.
const REG_TIMER_CUR: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;

/// ICR delivery status: set while the LAPIC is still sending the last IPI.
const ICR_SEND_PENDING: u32 = 1 << 12;
/// ICR delivery mode INIT (bits 10:8 = 101), level-triggered, asserted.
const ICR_INIT_ASSERT: u32 = 0x0000_C500;
/// The matching de-assert: level bit clear, trigger mode level.
const ICR_INIT_DEASSERT: u32 = 0x0000_8500;
/// ICR delivery mode STARTUP (bits 10:8 = 110); the vector goes in bits 7:0.
const ICR_STARTUP: u32 = 0x0000_4600;

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

/// Called from the timer vector, on whichever core's timer fired.
///
/// The BSP's ticks are the kernel's clock; every core's are counted per core
/// (`smp::ticks_on`) and every core's request a reschedule of *that core*.
pub fn on_tick() {
    crate::smp::this_cpu_tick();
    if crate::smp::cpu_index() == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    crate::sched::set_need_resched();
    eoi();
}

/// This core's APIC id, from its own ID register.
#[must_use]
pub fn apic_id() -> u32 {
    read(REG_ID) >> 24
}

/// Spin until the last IPI has left this core's LAPIC.
fn icr_wait_idle() {
    while read(REG_ICR_LOW) & ICR_SEND_PENDING != 0 {
        core::hint::spin_loop();
    }
}

fn send_ipi(dest_apic_id: u32, low: u32) {
    icr_wait_idle();
    write(REG_ICR_HIGH, dest_apic_id << 24);
    write(REG_ICR_LOW, low);
    icr_wait_idle();
}

/// Send INIT to one core: reset it to the wait-for-STARTUP state. Assert then
/// de-assert, as the multiprocessor specification's sequence has it; a VMM
/// accepts either alone, and real parts have wanted both.
pub fn send_init(dest_apic_id: u32) {
    send_ipi(dest_apic_id, ICR_INIT_ASSERT);
    send_ipi(dest_apic_id, ICR_INIT_DEASSERT);
}

/// Send STARTUP to one core: begin executing in real mode at `vector << 12`.
pub fn send_startup(dest_apic_id: u32, vector: u8) {
    send_ipi(dest_apic_id, ICR_STARTUP | u32::from(vector));
}

/// Block for `counts` ticks of this core's APIC timer, delivering nothing.
///
/// Borrows the timer: one-shot, LVT masked, spin on the current count. The
/// caller's timer configuration is not preserved — [`start_timer`] re-arms it —
/// so this is for phases where the timer is stopped anyway (AP bring-up). The
/// unit is the same uncalibrated one `start_timer` uses; see there.
pub fn delay_counts(counts: u32) {
    write(REG_TIMER_DIV, TIMER_DIV_16);
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_INIT, counts);
    while read(REG_TIMER_CUR) != 0 {
        core::hint::spin_loop();
    }
    write(REG_TIMER_INIT, 0);
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
    // Not the physmap, and not because of cacheability alone: 0xFEE0_0000 is at
    // 3.98 GiB and the physmap stops at 1 GiB (`PHYSMAP_LIMIT`), so it does not
    // reach the LAPIC at all — `phys_to_virt` would assert rather than return a
    // cached alias. This comment claimed the opposite until 2026-09-04; see
    // `crate::phys`.
    //
    // `MemAttr::Device` is still load-bearing on its own terms: a writeback
    // mapping of a register page lets the CPU satisfy a read from cache and
    // never issue the access.
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
    crate::smp::set_bsp_lapic_id(read(REG_ID) >> 24);

    // Before the timer is armed for real: `calibrate` borrows it (masked,
    // one-shot, full scale) and `start_timer` below re-arms it with whatever
    // count came out.
    let calibrated = calibrate();
    start_timer();

    serial::puts("  lapic: base=0x");
    serial::put_hex(base);
    serial::puts(" id=");
    serial::put_dec(u64::from(read(REG_ID) >> 24));
    serial::puts(" timer vector=");
    serial::put_dec(u64::from(TIMER_VECTOR));
    serial::puts(" periodic\n  lapic: ");
    if calibrated {
        let counts = TIMER_COUNT.load(Ordering::Relaxed);
        serial::puts("calibrated vs PIT: ");
        serial::put_dec(u64::from(counts));
        serial::puts(" counts per ");
        serial::put_dec(u64::from(US_PER_TICK_TARGET));
        serial::puts("us (");
        // counts per 10 ms at divide-16 -> the APIC's own input, in kHz.
        serial::put_dec(u64::from(counts) * 16 / 10_000);
        serial::puts(" MHz)\n");
    } else {
        serial::puts("[WARN] no PIT to calibrate against; tick period is a GUESS ");
        serial::puts("and every network timeout is scaled by it\n");
    }
    true
}

/// Bring up the LAPIC of a secondary core: enable it and start its timer.
///
/// The page is already mapped (a shared kernel mapping) and the timer vector
/// already has its handler; what is per core is the enable bit in this core's
/// `IA32_APIC_BASE`, its spurious vector, and its own timer.
pub fn init_ap() {
    let base_msr = rdmsr(IA32_APIC_BASE);
    // SAFETY: as in `init` — one bit, the address left as found.
    unsafe { wrmsr(IA32_APIC_BASE, base_msr | APIC_GLOBAL_ENABLE) };
    write(REG_SVR, (1 << 8) | u32::from(SPURIOUS_VECTOR));
    start_timer();
}

/// Arm the periodic timer.
///
/// The count comes from [`calibrate`] when a PIT was there to calibrate
/// against, and from [`UNCALIBRATED_COUNT`] when there was not.
///
/// It used to be a flat `100_000`, and that doc said the value was
/// "deliberately uncalibrated ... nothing here needs wall time yet". That
/// stopped being true when `net::uptime_us` began multiplying this timer's
/// ticks by a fixed 10 000 µs and handing the result to smoltcp, which measures
/// every DHCP retransmit, TCP retransmit and connection timeout against it.
///
/// The error was not small and not in one direction: the LAPIC counts at the
/// core crystal, so `100_000` at divide-16 is 1.6 M counts, which is ~1.6 ms on
/// a KVM guest whose APIC is nominally 1 GHz and ~16 ms on this machine's
/// 100 MHz bus. One clock ran 6x fast, the other 1.6x slow, and both called it
/// 10 ms.
pub fn start_timer() {
    write(REG_TIMER_DIV, TIMER_DIV_16);
    write(REG_LVT_TIMER, u32::from(TIMER_VECTOR) | LVT_TIMER_PERIODIC);
    write(REG_TIMER_INIT, TIMER_COUNT.load(Ordering::Relaxed));
}

/// The initial count when calibration could not run: the historical value, kept
/// so a machine with no PIT behaves exactly as it did before calibration
/// existed rather than differently-wrongly.
const UNCALIBRATED_COUNT: u32 = 100_000;

/// Counts to load for one [`US_PER_TICK_TARGET`] period. Written once by
/// [`calibrate`], read by every [`start_timer`] including the APs'.
static TIMER_COUNT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(UNCALIBRATED_COUNT);

/// Whether [`calibrate`] found a PIT and succeeded.
static CALIBRATED: AtomicBool = AtomicBool::new(false);

/// The tick period the timer is calibrated to, in microseconds. `net::uptime_us`
/// multiplies by this same number, so the two must agree — hence one constant.
pub const US_PER_TICK_TARGET: u32 = 10_000;

/// Did [`calibrate`] succeed? When false the tick period is whatever the core
/// crystal makes of [`UNCALIBRATED_COUNT`] and `uptime_us` is a guess.
#[must_use]
pub fn is_calibrated() -> bool {
    CALIBRATED.load(Ordering::Relaxed)
}

/// LAPIC counts measured in one [`US_PER_TICK_TARGET`] period, or 0.
#[must_use]
pub fn calibrated_count() -> u32 {
    if CALIBRATED.load(Ordering::Relaxed) { TIMER_COUNT.load(Ordering::Relaxed) } else { 0 }
}

/// Measure the LAPIC timer against the 8254 PIT and set [`TIMER_COUNT`] so one
/// tick is [`US_PER_TICK_TARGET`].
///
/// # Why the PIT, and why channel 2
///
/// The PIT's 1_193_182 Hz crystal is the one reference every PC-compatible
/// machine agrees on, and **channel 2 is the only one that can be polled**: its
/// gate is software-controlled through port `0x61` and its output is readable
/// there as bit 5, so the whole measurement needs no interrupt, no IDT entry
/// and no ordering against anything else. (Channel 0 drives IRQ0 and would
/// mean taking interrupts during early boot, which is precisely the state this
/// runs before.) The speaker bit is left off throughout — this is the standard
/// trick, and it is worth saying out loud that it does not make a sound.
///
/// # Not every machine has one
///
/// QEMU `microvm` and Firecracker present a deliberately minimal device set and
/// may have no PIT at all. So the wait for the output line is **bounded**: if
/// it never rises, calibration reports failure and the caller keeps the old
/// uncalibrated count. A boot that hangs here would be a far worse outcome than
/// a clock that is merely wrong, and the failure is announced rather than
/// silent.
///
/// Returns whether it succeeded.
pub fn calibrate() -> bool {
    /// Ticks of the PIT in one target period.
    const PIT_COUNT: u32 = (PIT_HZ as u64 * US_PER_TICK_TARGET as u64 / 1_000_000) as u32;
    /// Spins before giving up on an output line that is never going to rise.
    /// One period is ~10 ms; this is orders of magnitude more than that and
    /// still a fraction of a second.
    const SPIN_LIMIT: u32 = 50_000_000;

    if !pit_present() {
        return false;
    }

    // SAFETY: the 8254 and the NMI status/control port are fixed legacy I/O
    // ports. The speaker bit is explicitly cleared in every write, so the gate
    // manipulation below cannot make a sound, and nothing else in this kernel
    // touches channel 2 — there is no other user to race with.
    unsafe {
        let saved = crate::port::inb(PORT_61);
        // Gate low, speaker off: channel 2 stopped and reset.
        crate::port::outb(PORT_61, (saved & !SPEAKER) & !GATE);
        // Channel 2, lobyte then hibyte, mode 0 (interrupt on terminal count),
        // binary. Mode 0 is what makes the output line stay low until the count
        // expires and then latch high, which is the edge this polls for.
        crate::port::outb(0x43, 0b1011_0000);
        crate::port::outb(0x42, (PIT_COUNT & 0xff) as u8);
        crate::port::outb(0x42, (PIT_COUNT >> 8) as u8);

        // Arm the LAPIC at full scale, masked — this borrows the timer exactly
        // as `delay_counts` does, and `start_timer` re-arms it afterwards.
        write(REG_TIMER_DIV, TIMER_DIV_16);
        write(REG_LVT_TIMER, LVT_MASKED);
        write(REG_TIMER_INIT, u32::MAX);

        // Gate high: channel 2 starts counting now, and so does the measurement.
        crate::port::outb(PORT_61, (saved & !SPEAKER) | GATE);

        // A freshly gated mode-0 count holds its output LOW until it expires.
        // If it is already high we are not talking to a PIT, whatever
        // `pit_present` concluded — belt and braces on the failure that cost a
        // 50x clock.
        if crate::port::inb(PORT_61) & OUT != 0 {
            write(REG_TIMER_INIT, 0);
            crate::port::outb(PORT_61, saved);
            return false;
        }

        let mut spins: u32 = 0;
        while crate::port::inb(PORT_61) & OUT == 0 {
            spins += 1;
            if spins >= SPIN_LIMIT {
                write(REG_TIMER_INIT, 0);
                crate::port::outb(PORT_61, saved);
                return false;
            }
            core::hint::spin_loop();
        }
        let remaining = read(REG_TIMER_CUR);
        write(REG_TIMER_INIT, 0);
        crate::port::outb(PORT_61, saved);

        let elapsed = u32::MAX - remaining;
        // A plausibility band rather than a bare non-zero check. Below this the
        // measurement is noise (an output line that was already high, a PIT that
        // answered instantly); above it the counter wrapped or the divisor is
        // not what we asked for. Either way the old value is the safer answer.
        if !(1_000..=500_000_000).contains(&elapsed) {
            return false;
        }
        TIMER_COUNT.store(elapsed, Ordering::Relaxed);
        CALIBRATED.store(true, Ordering::Relaxed);
    }
    true
}

/// Ports and bits of PIT channel 2's software gate (port `0x61`).
const PORT_61: u16 = 0x61;
/// Channel 2's gate: counting runs while this is high.
const GATE: u8 = 1 << 0;
/// The PC speaker. Cleared in every write here — none of this makes a sound.
const SPEAKER: u8 = 1 << 1;
/// Channel 2's output, readable at port `0x61`. Low while counting.
const OUT: u8 = 1 << 5;

/// Is there a PIT channel 2 that actually responds?
///
/// **`inb` on an unimplemented port returns `0xFF`, not zero**, and that is not
/// a detail — it is the bug this function exists to prevent. QEMU `microvm` and
/// Firecracker present no PIT, so port `0x61` floats: the [`OUT`] bit reads as
/// already high, a wait-for-expiry loop falls through on its first iteration,
/// and a "calibration" comes back having measured the handful of cycles between
/// two instructions. Measured: 1199 counts where the real answer was 626 088,
/// which sailed through a plausibility band and left `uptime_us` running about
/// fifty times fast — with `CALIBRATED` set, so nothing downstream doubted it.
///
/// The probe is to clear the gate bit and read it back. A real `0x61` returns
/// what was written; a floating bus returns the bit still set.
fn pit_present() -> bool {
    // SAFETY: a fixed legacy I/O port, restored before returning. The speaker
    // bit is cleared, so nothing audible happens.
    unsafe {
        let saved = crate::port::inb(PORT_61);
        crate::port::outb(PORT_61, (saved & !SPEAKER) & !GATE);
        let back = crate::port::inb(PORT_61);
        crate::port::outb(PORT_61, saved);
        back & GATE == 0
    }
}

/// Gate PIT channel 2 for `us` microseconds and spin until it expires.
///
/// The measurement primitive [`calibrate`] and [`clock_rate_check`] share. `us`
/// must be at most [`PIT_MAX_US`] — the channel's count is 16 bits, so ~54.9 ms
/// is the longest interval it can express, and asking for more would silently
/// wrap to a short one.
///
/// Returns false if the output line never rose within a bounded spin, which is
/// how a machine with no PIT answers.
fn pit_wait_us(us: u32) -> bool {
    const SPIN_LIMIT: u32 = 500_000_000;

    if !pit_present() {
        return false;
    }
    if us == 0 || us > PIT_MAX_US {
        return false;
    }
    let count = (u64::from(PIT_HZ) * u64::from(us) / 1_000_000) as u32;
    if count == 0 || count > 0xFFFF {
        return false;
    }
    // SAFETY: fixed legacy I/O ports. The speaker bit is cleared in every write
    // so nothing audible happens, and channel 2 has no other user in this
    // kernel to race with.
    unsafe {
        let saved = crate::port::inb(PORT_61);
        crate::port::outb(PORT_61, (saved & !SPEAKER) & !GATE);
        crate::port::outb(0x43, 0b1011_0000);
        crate::port::outb(0x42, (count & 0xff) as u8);
        crate::port::outb(0x42, ((count >> 8) & 0xff) as u8);
        crate::port::outb(PORT_61, (saved & !SPEAKER) | GATE);

        let mut spins: u32 = 0;
        while crate::port::inb(PORT_61) & OUT == 0 {
            spins += 1;
            if spins >= SPIN_LIMIT {
                crate::port::outb(PORT_61, saved);
                return false;
            }
            core::hint::spin_loop();
        }
        crate::port::outb(PORT_61, saved);
    }
    true
}

/// The PIT's input frequency, in Hz. Fixed by the hardware.
const PIT_HZ: u32 = 1_193_182;

/// The longest interval PIT channel 2's 16-bit count can express.
const PIT_MAX_US: u32 = 54_000;

/// Check the clock's **rate**, not just that it moves — with interrupts on, and
/// before any user process starts.
///
/// [`calibrate`] can succeed and still be wrong: on QEMU `microvm` it returned a
/// count roughly forty times too small, so `enable_and_check_clock` saw ticks
/// arriving, reported success, and `uptime_us` ran about fifty times fast. A
/// clock that is merely *moving* is not a clock — smoltcp scales every DHCP
/// retransmit, TCP retransmit and connect timeout by it, so a 50x error is a
/// stack whose every timeout fires 50x early.
///
/// So this counts real timer interrupts across a PIT-measured interval and
/// compares against what [`US_PER_TICK_TARGET`] promises. It runs inside the
/// self-test suite, which is **before `run_init`** — a wrong clock is caught on
/// the boot that has it, on the screen, rather than inferred later from an ssh
/// session that connects and then stalls.
pub fn clock_rate_check(t: &mut Suite) {
    const INTERVAL_US: u32 = 50_000;
    const EXPECTED: u64 = (INTERVAL_US / US_PER_TICK_TARGET) as u64;

    if !enable_and_check_clock() {
        t.check("lapic: the clock advances once interrupts are enabled", false);
        return;
    }
    t.check("lapic: the clock advances once interrupts are enabled", true);

    let before = ticks();
    if !pit_wait_us(INTERVAL_US) {
        t.note("lapic: no PIT; the tick RATE is unverified", 0);
        return;
    }
    let observed = ticks() - before;
    t.note("lapic: ticks per 50ms (expect 5)", observed);
    // A factor-of-two band each way. Tight enough to have caught the 50x error
    // that motivated this, loose enough that a slow emulated PIT or a tick
    // landing on a boundary does not fail a boot.
    t.check(
        "lapic: the tick rate matches US_PER_TICK_TARGET within 2x",
        observed * 2 >= EXPECTED && observed <= EXPECTED * 2,
    );
}

/// Unmask interrupts on this core, and prove the clock actually moves.
///
/// **Every self-test that enables interrupts also disables them again on the
/// way out** (`lapic::smoke_test`, `sched::smoke_test`, `usermode::preempt_test`
/// all end in `cli`), which is right for a test — it keeps ticks from
/// interleaving with the next stage's output — and left the kernel handing the
/// machine to `init` with `IF` clear. The timer was armed and delivering
/// nothing.
///
/// That is not a cosmetic problem. `net::uptime_us` is `ticks() *
/// US_PER_TICK_TARGET`, and it is the clock smoltcp measures **every** DHCP
/// retransmit, TCP retransmit and connect timeout against. With `TICKS` frozen,
/// time stops: DHCP sends one DISCOVER and never retries, a TCP connection
/// completes its handshake and then never retransmits anything, and from the
/// far end the machine "connects once and then dies". Observed on the HP box,
/// and visible in the probe as `ticks=` pinned while `laps=` ran to 78 million.
///
/// The PVH path had been getting away with it by accident: `clock::sync_via_
/// sntp` does its own `sti` and never puts it back. The bare-metal path calls
/// no SNTP, so nothing ever re-enabled them.
///
/// Returns whether the tick count advanced within a bounded spin — a live timer
/// is worth *checking* rather than assuming, because everything above depends
/// on it and the failure is silent.
pub fn enable_and_check_clock() -> bool {
    /// Spins to wait for one tick. One period is `US_PER_TICK_TARGET`; this is
    /// far more than that and still a fraction of a second.
    const BUDGET: u64 = 200_000_000;

    start_timer();
    // SAFETY: the IDT is loaded, the timer vector has a handler and the legacy
    // PICs are masked — the same preconditions `smoke_test` establishes for the
    // kernel's first `sti`. This one is deliberately not paired with a `cli`:
    // handing `init` a machine with interrupts enabled is the point.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
    let before = ticks();
    let mut spins = 0u64;
    while ticks() == before && spins < BUDGET {
        spins += 1;
        core::hint::spin_loop();
    }
    ticks() > before
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

    // The calibration. Reported every boot rather than only checked, because
    // its *value* is what every network timeout is scaled by: `net::uptime_us`
    // multiplies ticks by `US_PER_TICK_TARGET`, and if the timer is not
    // actually running at that period then DHCP retransmits, TCP retransmits
    // and connect timeouts are all off by the same factor. A machine with no
    // PIT is legitimate (QEMU `microvm`, Firecracker), so its absence is a
    // `note`, not a failure — but it is never silent.
    if is_calibrated() {
        let counts = calibrated_count();
        t.check("lapic: the timer is calibrated against the PIT", counts > 0);
        t.note("lapic: counts per 10ms tick", u64::from(counts));
        t.note("lapic: apic input MHz", u64::from(counts) * 16 / 10_000);
    } else {
        t.note("lapic: NOT calibrated (no PIT); the tick period is a guess", 0);
    }
    // A measurement, not an assertion: the count differs by an order of
    // magnitude between emulation and real silicon (QEMU ~6e4, Zen 4 ~1e7 for
    // the same five ticks), which is itself evidence the ticks come from a
    // clock rather than from anything correlated with instruction count.
    t.note("lapic: spins waiting for 5 ticks", spins);
}
