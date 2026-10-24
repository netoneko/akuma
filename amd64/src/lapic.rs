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

use akuma_cpu::msr::{self, Msr};
use akuma_primitives::{OnceCopy, mmio::MmioWindow};
use crate::idt;
use crate::paging;
use akuma_port::{PIC, PIT};
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::serial;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// `IA32_APIC_BASE` (`akuma_cpu::msr::Msr::ApicBase`). Bits 12:35 are the base
/// address; bit 11 is the global enable; bit 8 marks the bootstrap processor.
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

/// The LAPIC register page, in the device window, once [`init`] has mapped it.
static LAPIC: OnceCopy<MmioWindow> = OnceCopy::new();

/// The window, or a panic naming the bug: no LAPIC register is read before `init`
/// maps the page (the old code dereferenced address `offset` and faulted).
fn window() -> MmioWindow {
    LAPIC.get().expect("LAPIC register access before lapic::init mapped the page")
}

fn read(offset: usize) -> u32 {
    window().read::<u32>(offset)
}

fn write(offset: usize, val: u32) {
    window().write::<u32>(offset, val);
}

/// Send a fixed delivery-mode IPI (delivery mode 000, level clear, edge) with
/// `vector` in bits 7:0 to one core. This is the TLB shootdown's transport;
/// INIT/STARTUP above are the same ICR with different mode bits.
pub fn send_fixed(dest_apic_id: u32, vector: u8) {
    send_ipi(dest_apic_id, u32::from(vector));
}

/// Has [`init`] mapped and enabled this core's LAPIC? A shootdown broadcast
/// before this would write the ICR through a null base; the sender-side hook
/// consults this and degrades to a core-local flush instead.
#[must_use]
pub fn ready() -> bool {
    LAPIC.is_set()
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
    // A hundred samples a second, costing two atomics: which thread was on the
    // CPU when the tick landed. See `sched::note_tick_sample`.
    crate::sched::note_tick_sample();
    // An idle long shot fires here: credit the periodic ticks it replaced
    // before the BSP block below counts the firing tick and pets the
    // watchdog, so `ticks()` and the pet cadence stay on wall time.
    credit_shot_fired();
    if crate::smp::cpu_index() == 0 {
        let t = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        // The hardware watchdog (opt-in, `wdt`): the BSP taking timer
        // interrupts is what keeps the machine from being reset.
        crate::watchdog::pet(t);
    }
    crate::sched::set_need_resched();
    eoi();
    // **A tick is the one-shot firing, so this is where the periodic mode comes
    // back — not only after the `hlt`.** `arm_deadline` leaves the timer in
    // one-shot mode for the halt and `restore_periodic` undoes it *after* the
    // halt returns. But the halting task can be switched away from inside this
    // very interrupt (the idle loop is preempted by whatever the tick readied),
    // and then nothing runs `restore_periodic` until that task is resumed. The
    // core runs other threads on a timer that has fired once and will never
    // fire again; the next one to `allow_tick` finds the count at 0 ("the
    // periodic tick serves it sooner"), arms nothing, and halts forever. On
    // ryzen 2026-10-13 that was core 1 frozen at tick 11509 with an
    // unkillable `hget` parked in its connect wait.
    restore_periodic();
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
    PIC.mask_all();
}

/// Map the LAPIC, enable it, and start a periodic timer. Interrupts stay masked;
/// the caller decides when to `sti`.
pub fn init() -> bool {
    mask_legacy_pics();

    let base_msr = msr::read(Msr::ApicBase);
    let base = base_msr & APIC_BASE_MASK;
    if base == 0 {
        serial::puts("  [FATAL] IA32_APIC_BASE reports no LAPIC\n");
        return false;
    }

    // Set the global enable bit if firmware left it clear, preserving the base.
    // SAFETY: sets exactly one bit in an architectural MSR, leaving the address
    // field as found.
    unsafe { msr::write(Msr::ApicBase, base_msr | APIC_GLOBAL_ENABLE) };

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
    let Some(win) = paging::map_device_window(base, 4096) else {
        serial::puts("  [FATAL] could not map the LAPIC page\n");
        return false;
    };
    // Every access through the window is a naturally aligned dword at an
    // architectural register offset.
    LAPIC.set(win);

    // Software-enable, and park spurious deliveries on a vector of their own so
    // they are distinguishable from a real one.
    write(REG_SVR, (1 << 8) | u32::from(SPURIOUS_VECTOR));

    idt::set_handler(TIMER_VECTOR, idt::timer_interrupt_entry());
    // The shootdown vector rides on the same shared IDT; registering it here
    // puts it on both boot protocols (PVH and multiboot2 both reach `init`),
    // which is the trap `install_shared_sinks` exists for.
    crate::shootdown::install();
    crate::smp::install_wake_vector();
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
        serial::puts(" MHz); tsc ");
        serial::put_dec(tsc_hz() / 1_000_000);
        serial::puts(" MHz\n");
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
    let base_msr = msr::read(Msr::ApicBase);
    // SAFETY: as in `init` — one bit, the address left as found.
    unsafe { msr::write(Msr::ApicBase, base_msr | APIC_GLOBAL_ENABLE) };
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
    TIMER_RUNNING.store(true, Ordering::Relaxed);
}

/// Is the periodic tick armed?
///
/// **A daemon that wants to sleep has to ask this first.** `uptime_us` advances
/// only when the timer vector runs, and `main.rs` arms the timer only when there
/// is a network — so on a no-network boot every deadline is unreachable and a
/// thread parked against one never wakes. A poll loop therefore has two correct
/// behaviours, not one: park between laps when the clock moves, yield when it
/// does not. `console::pump_daemon` is the loop that made this needed
/// (2026-09-13); before it, "the timer might not be running" was a reason to
/// spin *always*, which cost a host core at idle.
#[must_use]
pub fn timer_running() -> bool {
    TIMER_RUNNING.load(Ordering::Relaxed)
}

/// Backs [`timer_running`]. Set by [`start_timer`], cleared by [`stop_timer`].
static TIMER_RUNNING: AtomicBool = AtomicBool::new(false);

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

/// The TSC rate in Hz, measured across the same PIT gate as the LAPIC count;
/// 0 until [`calibrate`] succeeds. Read by [`tsc_hz`].
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// The TSC value [`calibrate`] read as its `tsc0` — boot's zero point for
/// [`tsc_uptime_us`]. Meaningless while [`TSC_HZ`] is still 0.
static TSC_START: AtomicU64 = AtomicU64::new(0);

/// The TSC frequency in Hz, or 0 when no PIT was there to measure it against.
///
/// Every polled-hardware budget in this kernel that is written in TSC ticks
/// (the xHCI driver's transfer timeouts) converts through this. Before it
/// existed the USB driver assumed ">= 1 GHz" and called 1e9 ticks one second;
/// on the trashcan's 3.2 GHz Haswell that second was 0.31 s, and a drive
/// answering within a real half-second was a "transfer timeout"
/// (`docs/archive/AKUMA_AMD64_USB_XHCI.md` § "Clock finding"). A caller that
/// gets 0 must pick a conservative rate itself; this function does not guess.
#[must_use]
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Monotonic microseconds since boot, at TSC resolution — `None` until
/// [`calibrate`] has run and found a real rate.
///
/// [`net::uptime_us`](crate::net::uptime_us) is `lapic::ticks() *
/// US_PER_TICK_TARGET`, and every clock on this target derives from it,
/// `clock_gettime` included: 10 ms granularity, documented at the usermode
/// self-test that first measured its cost (`usermode.rs`'s `fault_cost_
/// tests`, 2026-09-08) as unable to see a single page fault at all — a
/// 1000-iteration control landed on exactly one tick, every 512-fault bracket
/// read 0 ns, and the note there says outright: "TSC has the resolution the
/// tick clock lacks, and the kernel is where the TSC is reachable." This is
/// that resolution, exposed so [`net::uptime_us`](crate::net::uptime_us) —
/// and everything downstream of it, `clock_gettime` among them — can use it.
///
/// `u128` for the multiply: at a ~1 GHz TSC, a delta more than about 213 days
/// old would overflow a `u64` multiplied by `1_000_000` before the divide.
/// This kernel does not run that long, but the wraparound would land as a
/// silently wrong timestamp rather than a panic, which is worse than the
/// extra multiply — the conversion runs once per call, not once per tick.
#[must_use]
pub fn tsc_uptime_us() -> Option<u64> {
    let hz = tsc_hz();
    if hz == 0 {
        return None;
    }
    let now = akuma_cpu::tsc::rdtsc();
    // This core's TSC, moved onto the BSP's ([`tsc_sync_bsp`]). The kernel
    // does not preempt itself, so the core the `rdtsc` ran on is the core
    // `cpu_index` names.
    let now = if TSC_OFFSETS_LIVE.load(Ordering::Relaxed) {
        now.wrapping_sub(TSC_OFFSET[crate::smp::cpu_index()].load(Ordering::Relaxed) as u64)
    } else {
        now
    };
    let delta = now.wrapping_sub(TSC_START.load(Ordering::Relaxed));
    // A reading taken just after boot on a core whose corrected TSC is a hair
    // behind `TSC_START` would wrap to ~2^64; clamp instead.
    if delta > u64::MAX / 2 {
        return Some(0);
    }
    Some((u128::from(delta) * 1_000_000 / u128::from(hz)) as u64)
}

/// Per core: how far its TSC reads ahead of the BSP's, in TSC cycles,
/// measured once at bring-up by [`tsc_sync_bsp`] / [`tsc_sync_ap`].
///
/// # Why
///
/// [`tsc_uptime_us`] is `CLOCK_MONOTONIC`, and it subtracts the BSP's
/// `TSC_START` from whatever core's `rdtsc` the caller happens to run on. On
/// a KVM guest the vCPUs' TSCs agree; on the ryzen's metal they did not —
/// `wakelat` read a 1 ms spin between two `clock_gettime`s as -1 011 819 us and
/// +1 019 192 us (2026-10-09), so a thread that migrated saw time run a second
/// backwards. Chromium's network service CHECKs that a stream request ended no
/// earlier than it started (`HttpNetworkTransaction`, the
/// `Net.NetworkTransaction.StreamRequestCompleteTime4` histogram) and died on
/// that `int3` ~4 s into every Chromium start on the metal, never under
/// Firecracker.
static TSC_OFFSET: [core::sync::atomic::AtomicI64; crate::smp::MAX_CPUS] =
    [const { core::sync::atomic::AtomicI64::new(0) }; crate::smp::MAX_CPUS];
/// Set once the first AP's offset is known. Before it the clock reads the raw
/// TSC and never touches `gs` — [`tsc_uptime_us`] runs on the BSP before its
/// per-CPU block exists.
static TSC_OFFSETS_LIVE: AtomicBool = AtomicBool::new(false);

/// Ping-pong rounds per AP; the round with the shortest round trip wins.
const TSC_SYNC_ROUNDS: u64 = 64;
/// BSP -> AP: "read your TSC for round N".
static TSC_SYNC_REQ: AtomicU64 = AtomicU64::new(0);
/// AP -> BSP: "round N answered".
static TSC_SYNC_ACK: AtomicU64 = AtomicU64::new(0);
/// The AP's TSC for the round in [`TSC_SYNC_ACK`].
static TSC_SYNC_T: AtomicU64 = AtomicU64::new(0);
/// BSP -> AP: offset stored for AP index N; it may now read the clock.
static TSC_SYNC_DONE: AtomicU64 = AtomicU64::new(0);

/// The BSP half: measure AP `idx`'s TSC against this core's and record the
/// offset. Call right after the AP reports online, with the AP inside
/// [`tsc_sync_ap`]. Returns the offset in cycles (AP minus BSP).
///
/// Each round brackets the AP's read between two of the BSP's; the AP's
/// reading is assumed to sit at the midpoint, so the error is at most half the
/// round trip, and the shortest of [`TSC_SYNC_ROUNDS`] trips is the one kept
/// (Linux's `tsc_sync` estimates the same way).
pub fn tsc_sync_bsp(idx: usize, base: i64) -> i64 {
    let mut best_rtt = u64::MAX;
    let mut best_off = 0i64;
    TSC_SYNC_REQ.store(0, Ordering::SeqCst);
    TSC_SYNC_ACK.store(0, Ordering::SeqCst);
    for r in 1..=TSC_SYNC_ROUNDS {
        let t0 = akuma_cpu::tsc::rdtsc();
        TSC_SYNC_REQ.store(r, Ordering::SeqCst);
        let mut spins = 0u32;
        while TSC_SYNC_ACK.load(Ordering::SeqCst) != r {
            spins += 1;
            if spins > 50_000_000 {
                // The AP never answered: leave its offset 0, as before.
                return 0;
            }
            core::hint::spin_loop();
        }
        let t2 = akuma_cpu::tsc::rdtsc();
        let t1 = TSC_SYNC_T.load(Ordering::SeqCst);
        let rtt = t2.wrapping_sub(t0);
        if rtt < best_rtt {
            best_rtt = rtt;
            let mid = t0.wrapping_add(rtt / 2);
            best_off = t1.wrapping_sub(mid).cast_signed().wrapping_add(base);
        }
    }
    TSC_OFFSET[idx].store(best_off, Ordering::Relaxed);
    TSC_OFFSETS_LIVE.store(true, Ordering::Release);
    TSC_SYNC_DONE.store(idx as u64, Ordering::SeqCst);
    best_off
}

/// The AP half of [`tsc_sync_bsp`]: answer every round, then wait until the
/// BSP has stored this core's offset, so nothing here reads the clock before
/// it is corrected.
pub fn tsc_sync_ap(idx: usize) {
    for r in 1..=TSC_SYNC_ROUNDS {
        let mut spins = 0u32;
        while TSC_SYNC_REQ.load(Ordering::SeqCst) != r {
            spins += 1;
            if spins > 50_000_000 {
                return;
            }
            core::hint::spin_loop();
        }
        let t1 = akuma_cpu::tsc::rdtsc();
        TSC_SYNC_T.store(t1, Ordering::SeqCst);
        TSC_SYNC_ACK.store(r, Ordering::SeqCst);
    }
    let mut spins = 0u32;
    while TSC_SYNC_DONE.load(Ordering::SeqCst) != idx as u64 && spins < 50_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
}

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
/// # Why channel 0, not channel 2
///
/// The PIT's 1_193_182 Hz crystal is the one reference every PC-compatible
/// machine agrees on. This used to poll channel 2 — its gate and output are
/// both reachable through port `0x61`, so a measurement needs no interrupt —
/// but that port is board glue, not part of the 8254 itself, and QEMU's
/// `microvm` machine wires up an `isa-pit` (ports 0x40/0x43) with no such
/// glue: `info qtree` shows the chip and nothing answering 0x61. Measured
/// 2026-09-17: `pit_present`'s old port-0x61 probe therefore always failed
/// there, `calibrate` never ran, and every timeout on that machine was scaled
/// by whatever `UNCALIBRATED_COUNT` happens to be worth under TCG — the
/// "6x fast, 1.6x slow, both called it 10 ms" case [`start_timer`] documents.
///
/// Channel 0 needs no gate at all: it is hardwired enabled on every PC, so
/// loading a new count starts it counting immediately. What it lacks is a
/// dedicated output pin on a legacy port — but the 8254's own **read-back
/// command** (`0xD2` on the command port, decoded below) latches a channel's
/// status, OUTPUT bit included, onto its own data port. That is standard
/// 8254 behaviour with no motherboard glue involved, so it works identically
/// on `microvm`, on real hardware, and (unlike a channel-0 IRQ) it needs no
/// interrupt, no IDT entry and no ordering against anything else — the same
/// property channel 2 had, reached a different way. The speaker bit that
/// channel 2's version had to keep clear does not exist here: nothing about
/// channel 0 or the read-back command ever touches port `0x61`.
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
    /// Spins before giving up on an output that is never going to latch high.
    /// One period is ~10 ms; this is orders of magnitude more than that and
    /// still a fraction of a second.
    const SPIN_LIMIT: u32 = 50_000_000;

    // Nothing else in this kernel programs PIT channel 0 — the legacy PICs are
    // masked before `calibrate` runs (`init`'s ordering), so even if this makes
    // it raise IRQ0 nothing delivers it. Mode 0, and the load itself starts the
    // countdown (`Pit::load_channel0_oneshot`).
    PIT.load_channel0_oneshot(PIT_COUNT as u16);

    // The LAPIC MMIO window is mapped by `init`, which `calibrate` follows.
    {
        // Arm the LAPIC at full scale, masked — this borrows the timer exactly
        // as `delay_counts` does, and `start_timer` re-arms it afterwards.
        write(REG_TIMER_DIV, TIMER_DIV_16);
        write(REG_LVT_TIMER, LVT_MASKED);
        write(REG_TIMER_INIT, u32::MAX);
        // The TSC rides along on the same load — one PIT period measures both
        // the LAPIC count and the TSC count, so the two clocks this kernel
        // keeps agree about what 10 ms is.
        let tsc0 = akuma_cpu::tsc::rdtsc();

        // A freshly loaded mode-0 count holds OUTPUT low until it expires. If
        // it reads high immediately, this is not a real PIT answering — the
        // same belt-and-braces the old port-0x61 version kept, against the
        // same 50x-clock failure, reached through the read-back command
        // instead of a floating board-glue port.
        if PIT.channel0_output_high() {
            write(REG_TIMER_INIT, 0);
            return false;
        }

        let mut spins: u32 = 0;
        while !PIT.channel0_output_high() {
            spins += 1;
            if spins >= SPIN_LIMIT {
                write(REG_TIMER_INIT, 0);
                return false;
            }
            core::hint::spin_loop();
        }
        let remaining = read(REG_TIMER_CUR);
        let tsc1 = akuma_cpu::tsc::rdtsc();
        write(REG_TIMER_INIT, 0);

        let elapsed = u32::MAX - remaining;
        // A plausibility band rather than a bare non-zero check. Below this the
        // measurement is noise (an output line that was already high, a PIT that
        // answered instantly); above it the counter wrapped or the divisor is
        // not what we asked for. Either way the old value is the safer answer.
        if !(1_000..=500_000_000).contains(&elapsed) {
            return false;
        }
        TIMER_COUNT.store(elapsed, Ordering::Relaxed);
        // Ticks in one US_PER_TICK_TARGET period -> Hz. Same plausibility
        // logic as the LAPIC: a TSC that "ran" fewer ticks than a 100 MHz
        // part would in 10 ms did not measure anything, and stays 0 so the
        // consumer falls back to its conservative assumption.
        let tsc_ticks = tsc1.wrapping_sub(tsc0);
        let hz = tsc_ticks * 1_000_000 / u64::from(US_PER_TICK_TARGET);
        if hz >= 100_000_000 {
            // `tsc0`, not `tsc1` or a fresh read: boot's zero point is the
            // instant this measurement started, and `tsc_uptime_us` needs to
            // agree with `ticks()` (0 until `start_timer` below) about what
            // "just now" meant at that instant.
            TSC_START.store(tsc0, Ordering::Relaxed);
            TSC_HZ.store(hz, Ordering::Relaxed);
        }
        CALIBRATED.store(true, Ordering::Relaxed);
    }
    true
}

/// Run PIT channel 0 for `us` microseconds and spin until it expires.
///
/// The measurement primitive [`clock_rate_check`] uses (separately from
/// [`calibrate`]'s own channel-0 use, so the two never share a live count).
/// `us` must be at most [`PIT_MAX_US`] — the channel's count is 16 bits, so
/// ~54.9 ms is the longest interval it can express, and asking for more would
/// silently wrap to a short one.
///
/// Returns false if OUTPUT never latches high within a bounded spin, which is
/// how a machine with no PIT answers — **`inb` on an unimplemented port
/// returns `0xFF`, not zero**, so an absent chip would otherwise read as an
/// output already high and return instantly having measured nothing; the
/// immediate-high check catches that the same way [`calibrate`]'s does.
fn pit_wait_us(us: u32) -> bool {
    const SPIN_LIMIT: u32 = 500_000_000;

    if us == 0 || us > PIT_MAX_US {
        return false;
    }
    let count = (u64::from(PIT_HZ) * u64::from(us) / 1_000_000) as u32;
    if count == 0 || count > 0xFFFF {
        return false;
    }
    // `count <= 0xFFFF` was checked above.
    PIT.load_channel0_oneshot(count as u16);

    if PIT.channel0_output_high() {
        return false;
    }
    let mut spins: u32 = 0;
    while !PIT.channel0_output_high() {
        spins += 1;
        if spins >= SPIN_LIMIT {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// The PIT's input frequency, in Hz. Fixed by the hardware.
const PIT_HZ: u32 = 1_193_182;

/// The longest interval PIT channel 2's 16-bit count can express.
const PIT_MAX_US: u32 = 54_000;

#[cfg(not(feature = "no-tests"))]
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

/// Per core: is the timer in one-shot mode for a deadline ([`arm_deadline`])?
static ONESHOT: [AtomicBool; crate::smp::MAX_CPUS] =
    [const { AtomicBool::new(false) }; crate::smp::MAX_CPUS];

/// Times [`arm_deadline`] put a core's timer into one-shot mode.
pub static ONESHOT_ARMS: AtomicU64 = AtomicU64::new(0);

/// Before a `hlt`: if a parked thread's deadline falls before this core's next
/// periodic tick, make the timer fire **at** that deadline instead.
///
/// # Why
///
/// A timed wait (`FUTEX_WAIT` with a timeout, so every
/// `pthread_cond_timedwait`) parks with its deadline in `WAKE_TIMES`, and only
/// `x86_wake_pass` moves it out — which runs when a core next schedules. A
/// halted core schedules at its next interrupt, and with the periodic tick
/// alone that is up to 10 ms after the deadline: `timerlat` measured a 1 ms
/// futex wait at 10.0 ms median, 16 ms at 26.3 (2026-10-09; Linux +0.12 ms),
/// and Chromium's frame pipeline is a chain of such waits.
///
/// # How
///
/// The LVT timer is switched to one-shot with an initial count equal to the
/// time left, and [`restore_periodic`] puts the periodic tick back after the
/// halt, whatever woke it. That restarts the period, so ticks shift phase;
/// nothing reads the tick *count* as a clock once the TSC is calibrated, and
/// this is a no-op otherwise. A shot that fires a hair before the deadline (the
/// LAPIC and TSC rates come from one PIT measurement, not one oscillator) costs
/// one more halt with a shorter shot, not a tick.
///
/// Only the halting core's own timer is touched, from that core with
/// interrupts off; no lock, no allocation.
pub fn arm_deadline() {
    // **Interrupts off for the whole arm.** Callers are meant to arrive masked, but `net_blocking_relax` ->
    // `allow_tick` reaches here with `IF` set (the BKL release restores the flags it was taken under). The tick
    // is then free to land between the count write and the `ONESHOT` store: `on_tick` sees the flag clear, does
    // not restore the periodic tick, and the `sti; hlt` that follows sleeps past a one-shot that has already
    // fired and nothing left to wake it. Under a hypervisor the window is a whole VM exit wide (the host
    // yields between entries), so a 0.8 ms shot expires inside it (`docs/archive/FIRECRACKER_GUEST_HANG.md`).
    akuma_primitives::irq::with_irqs_disabled(|| {
        if !TIMER_RUNNING.load(Ordering::Relaxed) || !is_calibrated() {
            return;
        }
        // A one-shot left over from an earlier halt (see `on_tick`) would make the
        // `REG_TIMER_CUR` comparison below read an expired timer as "the periodic
        // tick serves it sooner". Put the periodic mode back first.
        restore_periodic();
        let Some(now) = tsc_uptime_us() else { return };
        let Some(due) = akuma_threading::x86_earliest_wake_time() else { return };
        let per_tick = u64::from(TIMER_COUNT.load(Ordering::Relaxed));
        // Already due: the wake pass will find it without a halt; arm the minimum
        // so the halt ends at once if the caller halts anyway.
        let left_us = due.saturating_sub(now);
        let counts = (left_us * per_tick).div_ceil(u64::from(US_PER_TICK_TARGET)).max(1);
        // The periodic tick serves it as soon: leave the timer alone.
        if counts >= u64::from(read(REG_TIMER_CUR)) {
            return;
        }
        write(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
        write(REG_TIMER_INIT, counts as u32);
        ONESHOT[crate::smp::cpu_index()].store(true, Ordering::Relaxed);
        ONESHOT_ARMS.fetch_add(1, Ordering::Relaxed);
    })
}

/// Make this core's next timer interrupt arrive in `us` microseconds, if that is
/// sooner than the periodic tick would. Returns whether it armed.
///
/// The SVM hypervisor's use (`kvm/linux.rs`): a guest's timer deadline is only
/// visible to the host as a VM exit, and a guest that never exits on its own gets
/// one every host tick — 10 ms, which is coarser than the 4 ms tick Linux asks of
/// its APIC timer and lost the difference as coalesced interrupts. Arming the
/// host's one-shot at the guest's deadline makes the exit land there.
///
/// Same machinery as [`arm_deadline`] (the `ONESHOT` flag, and [`on_tick`]'s
/// [`restore_periodic`] bringing the periodic tick back once the shot fires), so
/// the host tick resumes by itself; and like it, called with interrupts off on the
/// core whose timer it touches. A shot already in flight that fires sooner stays.
pub fn arm_oneshot_in_us(us: u64) -> bool {
    // Interrupts off for the whole arm, whatever the caller's `IF` was: see `arm_deadline`.
    akuma_primitives::irq::with_irqs_disabled(|| {
        if !TIMER_RUNNING.load(Ordering::Relaxed) || !is_calibrated() {
            return false;
        }
        let per_tick = u64::from(TIMER_COUNT.load(Ordering::Relaxed));
        let counts = (us * per_tick).div_ceil(u64::from(US_PER_TICK_TARGET)).max(1);
        // The periodic tick (or a shot already armed) serves it as soon: leave it.
        if counts >= u64::from(read(REG_TIMER_CUR)) {
            return false;
        }
        write(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
        write(REG_TIMER_INIT, counts as u32);
        ONESHOT[crate::smp::cpu_index()].store(true, Ordering::Relaxed);
        ONESHOT_ARMS.fetch_add(1, Ordering::Relaxed);
        true
    })
}

/// After the `hlt` [`arm_deadline`] prepared: back to the periodic tick, if
/// this core left it.
pub fn restore_periodic() {
    if ONESHOT[crate::smp::cpu_index()].swap(false, Ordering::Relaxed) {
        start_timer();
    }
}

// ============================================================================
// The idle long shot: sleeping past the tick
// ============================================================================
//
// A halted core wakes every periodic tick and, when the lap finds nothing to
// do, halts again. Each wake costs power for no work — the whole reason a
// battery laptop idles warm. [`arm_idle_shot`] lets the idle loop replace a
// run of ticks with one one-shot of up to [`MAX_IDLE_SHOT_TICKS`], waking at
// the earliest parked deadline inside the budget exactly as [`arm_deadline`]
// does, and at the budget itself otherwise.
//
// The cost is ticks that never fire. `TICKS` *is* the kernel's clock
// (`net::uptime_us` multiplies it by [`US_PER_TICK_TARGET`], and every DHCP
// retransmit, TCP timeout and `clock_gettime` rides on it), so every skipped
// tick is credited back — [`credit_shot_fired`] when the shot itself fires,
// [`settle_idle_shot`] when a device IRQ wakes the core first. Opt-in
// (`idlesleep` on the command line); until a metal A/B measures it, the
// default is today's behaviour.

/// Per core: how many ticks the idle long shot in flight was armed with, 0 =
/// none. Written by [`arm_idle_shot`], consumed exactly once — by
/// [`on_tick`] (via [`credit_shot_fired`]) when the shot fires, or by
/// [`settle_idle_shot`] when something else wakes the core first.
static ARMED_IDLE_TICKS: [AtomicU32; crate::smp::MAX_CPUS] =
    [const { AtomicU32::new(0) }; crate::smp::MAX_CPUS];
/// The [`tsc_uptime_us`] reading [`arm_idle_shot`] armed at, for
/// [`settle_idle_shot`]'s elapsed-time math.
static ARMED_IDLE_AT_US: [AtomicU64; crate::smp::MAX_CPUS] =
    [const { AtomicU64::new(0) }; crate::smp::MAX_CPUS];

/// Long one-shots [`arm_idle_shot`] armed.
pub static IDLE_SHOTS: AtomicU64 = AtomicU64::new(0);
/// Ticks credited back for periodic ticks a long shot replaced.
pub static IDLE_SHOT_TICKS_CREDITED: AtomicU64 = AtomicU64::new(0);

/// The longest sleep [`arm_idle_shot`] will take, in ticks — one second, a
/// tenth of the FCH watchdog's shortest allowed timeout (`wdt=<s>` clamps at
/// 10 s), with `pet` firing once a second from the tick path.
pub const MAX_IDLE_SHOT_TICKS: u32 = 100;

/// The one idle long shot in flight fired: credit the periodic ticks it
/// replaced, before the pet below reads the count.
fn credit_shot_fired() {
    let cpu = crate::smp::cpu_index();
    let armed = ARMED_IDLE_TICKS[cpu].swap(0, Ordering::Relaxed);
    if armed <= 1 || cpu != 0 {
        return;
    }
    // The fire itself is the one tick `on_tick` adds below; the rest never ran.
    let extra = u64::from(armed - 1);
    TICKS.fetch_add(extra, Ordering::Relaxed);
    IDLE_SHOT_TICKS_CREDITED.fetch_add(extra, Ordering::Relaxed);
}

/// Before an idle `hlt`: sleep up to `max_ticks` ticks in one one-shot, waking
/// at a parked thread's deadline when it falls sooner. Returns the ticks
/// armed, 0 when the periodic tick serves as well (or better).
///
/// Call **after** [`arm_deadline`], not instead of it: a sub-tick deadline
/// arms an exact one-shot there, and the `ONESHOT` flag it sets is what makes
/// this return 0 instead of arming a longer shot over it.
///
/// Callers hold `IF` clear (the idle loop's contract), so the shot cannot fire
/// between the arm and the `sti`.
pub fn arm_idle_shot(max_ticks: u32) -> u32 {
    // Interrupts off for the whole arm, whatever the caller's `IF` was: see `arm_deadline`.
    akuma_primitives::irq::with_irqs_disabled(|| {
        let cpu = crate::smp::cpu_index();
        if !TIMER_RUNNING.load(Ordering::Relaxed) || !is_calibrated() {
            return 0;
        }
        if ONESHOT[cpu].load(Ordering::Relaxed) {
            return 0;
        }
        // A one-shot left over from an earlier halt would make the
        // `REG_TIMER_CUR` comparison below read an expired timer as "the periodic
        // tick serves sooner" (the same trap `arm_deadline` documents).
        restore_periodic();
        let Some(now) = tsc_uptime_us() else { return 0 };
        let per_tick = u64::from(TIMER_COUNT.load(Ordering::Relaxed));
        let cap_counts = u64::from(max_ticks.max(1)) * per_tick;
        // Wake at the earliest parked deadline when it falls inside the budget —
        // in exact counts, not rounded ticks, or a 3 ms futex deadline would wake
        // 10 ms late and undo `arm_deadline`'s measured win. Past the cap, sleep
        // the cap.
        let shot_counts = match akuma_threading::x86_earliest_wake_time() {
            Some(due) => {
                let left_us = due.saturating_sub(now);
                let exact = (left_us * per_tick).div_ceil(u64::from(US_PER_TICK_TARGET)).max(1);
                if exact < cap_counts { exact } else { cap_counts }
            }
            None => cap_counts,
        };
        // The periodic tick fires this soon or sooner anyway: leave it alone.
        if shot_counts <= u64::from(read(REG_TIMER_CUR)) {
            return 0;
        }
        let armed_ticks = shot_counts.div_ceil(per_tick) as u32;
        write(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
        write(REG_TIMER_INIT, shot_counts as u32);
        ARMED_IDLE_TICKS[cpu].store(armed_ticks, Ordering::Relaxed);
        ARMED_IDLE_AT_US[cpu].store(now, Ordering::Relaxed);
        ONESHOT[cpu].store(true, Ordering::Relaxed);
        ONESHOT_ARMS.fetch_add(1, Ordering::Relaxed);
        IDLE_SHOTS.fetch_add(1, Ordering::Relaxed);
        armed_ticks
    })
}

/// The halt [`arm_idle_shot`] prepared ended on something that was not the
/// timer — a device IRQ woke the core before the shot fired, and the
/// [`restore_periodic`] the idle loop ran next has reloaded periodic mode,
/// cancelling the shot. The periodic ticks the sleep skipped still never ran,
/// so credit what really elapsed, capped at the shot: the clock resumes
/// counting on its own from now.
pub fn settle_idle_shot() {
    let cpu = crate::smp::cpu_index();
    let armed = ARMED_IDLE_TICKS[cpu].swap(0, Ordering::Relaxed);
    if armed == 0 {
        return;
    }
    let at = ARMED_IDLE_AT_US[cpu].swap(0, Ordering::Relaxed);
    if cpu != 0 {
        return;
    }
    let Some(now) = tsc_uptime_us() else { return };
    let missed = now
        .saturating_sub(at)
        .saturating_div(u64::from(US_PER_TICK_TARGET))
        .min(u64::from(armed - 1));
    if missed == 0 {
        return;
    }
    let t = TICKS.fetch_add(missed, Ordering::Relaxed) + missed;
    // The sleep skipped this second's pet boundary; reload now, so a long
    // idle never eats into the watchdog's window.
    crate::watchdog::pet(t);
    IDLE_SHOT_TICKS_CREDITED.fetch_add(missed, Ordering::Relaxed);
}

/// Stop the timer. Used after the smoke test so later output is not interleaved
/// with ticks.
pub fn stop_timer() {
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_INIT, 0);
    TIMER_RUNNING.store(false, Ordering::Relaxed);
}

#[cfg(not(feature = "no-tests"))]
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

/// Core `idx`'s TSC offset from the BSP's (cycles; 0 for the BSP and for an unsynced core).
#[must_use]
pub fn tsc_offset_of(idx: usize) -> i64 {
    TSC_OFFSET.get(idx).map_or(0, |o| o.load(Ordering::Relaxed))
}
