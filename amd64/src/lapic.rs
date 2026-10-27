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
use crate::idt;
use crate::paging;
use akuma_port::PIC;
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::serial;
use akuma_lapic_amd64 as timer;
// One implementation, two consumers (composition rule 3): everything
// re-exported here moved to `akuma-lapic-amd64`; the amd64 consumers
// (`boot.rs`, `clock.rs`, `console.rs`, `exec_runtime.rs`, `idt.rs`,
// `net.rs`, `smp.rs`) compile unchanged against these names.
pub use akuma_lapic_amd64::{
    apic_id, calibrated_count, delay_counts, eoi, is_calibrated, pit_wait_us,
    ready, start_timer, stop_timer, ticks, timer_running, tsc_hz,
    tsc_uptime_us, TIMER_VECTOR, US_PER_TICK_TARGET,
};
use akuma_lapic_amd64::{APIC_BASE_MASK, APIC_GLOBAL_ENABLE, SPURIOUS_VECTOR};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Interrupt Command Register, low half: delivery mode, destination shorthand,
/// vector, and the delivery-status bit (12) that says a previous IPI is still
/// being sent.
const REG_ICR_LOW: usize = 0x300;
/// ICR high half: the destination APIC id in bits 31:24.
const REG_ICR_HIGH: usize = 0x310;
/// ICR delivery status: set while the LAPIC is still sending the last IPI.
const ICR_SEND_PENDING: u32 = 1 << 12;
/// ICR delivery mode INIT (bits 10:8 = 101), level-triggered, asserted.
const ICR_INIT_ASSERT: u32 = 0x0000_C500;
/// The matching de-assert: level bit clear, trigger mode level.
const ICR_INIT_DEASSERT: u32 = 0x0000_8500;
/// ICR delivery mode STARTUP (bits 10:8 = 110); the vector goes in bits 7:0.
const ICR_STARTUP: u32 = 0x0000_4600;

/// Send a fixed delivery-mode IPI (delivery mode 000, level clear, edge) with
/// `vector` in bits 7:0 to one core. This is the TLB shootdown's transport;
/// INIT/STARTUP above are the same ICR with different mode bits.
pub fn send_fixed(dest_apic_id: u32, vector: u8) {
    send_ipi(dest_apic_id, u32::from(vector));
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
        let t = timer::advance_tick();
        // The hardware watchdog (opt-in, `wdt`): the BSP taking timer
        // interrupts is what keeps the machine from being reset.
        crate::watchdog::pet(t);
    }
    crate::sched::set_need_resched();
    timer::eoi();
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

/// Spin until the last IPI has left this core's LAPIC.
fn icr_wait_idle() {
    while timer::read_reg(REG_ICR_LOW) & ICR_SEND_PENDING != 0 {
        core::hint::spin_loop();
    }
}

fn send_ipi(dest_apic_id: u32, low: u32) {
    icr_wait_idle();
    timer::write_reg(REG_ICR_HIGH, dest_apic_id << 24);
    timer::write_reg(REG_ICR_LOW, low);
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
    if base_msr & APIC_BASE_MASK == 0 {
        serial::puts("  [FATAL] IA32_APIC_BASE reports no LAPIC\n");
        return false;
    }

    // SAFETY: sets exactly one bit in an architectural MSR, leaving the address
    // field as found.
    unsafe { msr::write(Msr::ApicBase, base_msr | APIC_GLOBAL_ENABLE) };

    // Map the register page into the *device* window, uncached — not the
    // physmap: 0xFEE0_0000 is at 3.98 GiB and the physmap stops at 1 GiB
    // (`crate::phys`), and a writeback mapping would let the CPU satisfy a
    // register read from cache and never issue the access.
    let Some(win) = paging::map_device_window(base_msr & APIC_BASE_MASK, 4096) else {
        serial::puts("  [FATAL] could not map the LAPIC page\n");
        return false;
    };
    // One implementation, two consumers: the window, the software enable, the
    // PIT calibration and the periodic arm are the crate's now. What stays here
    // is the IDT registration and the SMP wiring the crate must not know about.
    let Ok(info) = timer::init(base_msr | APIC_GLOBAL_ENABLE, win) else {
        serial::puts("  [FATAL] IA32_APIC_BASE reports no LAPIC\n");
        return false;
    };

    idt::set_handler(TIMER_VECTOR, idt::timer_interrupt_entry());
    // The shootdown vector rides on the same shared IDT; registering it here
    // puts it on both boot protocols (PVH and multiboot2 both reach `init`),
    // which is the trap `install_shared_sinks` exists for.
    crate::shootdown::install();
    crate::smp::install_wake_vector();
    crate::smp::set_bsp_lapic_id(info.id);

    serial::puts("  lapic: base=0x");
    serial::put_hex(info.base);
    serial::puts(" id=");
    serial::put_dec(u64::from(info.id));
    serial::puts(" timer vector=");
    serial::put_dec(u64::from(TIMER_VECTOR));
    serial::puts(" periodic\n  lapic: ");
    if info.calibrated {
        serial::puts("calibrated vs PIT: ");
        serial::put_dec(u64::from(info.timer_count));
        serial::puts(" counts per ");
        serial::put_dec(u64::from(US_PER_TICK_TARGET));
        serial::puts("us (");
        // counts per 10 ms at divide-16 -> the APIC's own input, in kHz.
        serial::put_dec(u64::from(info.timer_count) * 16 / 10_000);
        serial::puts(" MHz); tsc ");
        serial::put_dec(info.tsc_hz / 1_000_000);
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
    timer::software_enable(SPURIOUS_VECTOR);
    timer::start_timer();
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
        if !timer::timer_running() || !is_calibrated() {
            return;
        }
        // A one-shot left over from an earlier halt (see `on_tick`) would make the
        // `REG_TIMER_CUR` comparison below read an expired timer as "the periodic
        // tick serves it sooner". Put the periodic mode back first.
        restore_periodic();
        let Some(now) = tsc_uptime_us() else { return };
        let Some(due) = akuma_threading::x86_earliest_wake_time() else { return };
        let per_tick = u64::from(timer::calibrated_count());
        // Already due: the wake pass will find it without a halt; arm the minimum
        // so the halt ends at once if the caller halts anyway.
        let left_us = due.saturating_sub(now);
        let counts = (left_us * per_tick).div_ceil(u64::from(US_PER_TICK_TARGET)).max(1);
        // The periodic tick serves it as soon: leave the timer alone.
        if counts >= u64::from(timer::timer_current_count()) {
            return;
        }
        timer::arm_oneshot(counts as u32);
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
        if !timer::timer_running() || !is_calibrated() {
            return false;
        }
        let per_tick = u64::from(timer::calibrated_count());
        let counts = (us * per_tick).div_ceil(u64::from(US_PER_TICK_TARGET)).max(1);
        // The periodic tick (or a shot already armed) serves it as soon: leave it.
        if counts >= u64::from(timer::timer_current_count()) {
            return false;
        }
        timer::arm_oneshot(counts as u32);
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
    timer::credit_ticks(extra);    IDLE_SHOT_TICKS_CREDITED.fetch_add(extra, Ordering::Relaxed);
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
        if !timer::timer_running() || !is_calibrated() {
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
        let per_tick = u64::from(timer::calibrated_count());
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
        if shot_counts <= u64::from(timer::timer_current_count()) {
            return 0;
        }
        let armed_ticks = shot_counts.div_ceil(per_tick) as u32;
        timer::arm_oneshot(shot_counts as u32);
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
    let t = timer::credit_ticks(missed);    // The sleep skipped this second's pet boundary; reload now, so a long
    // idle never eats into the watchdog's window.
    crate::watchdog::pet(t);
    IDLE_SHOT_TICKS_CREDITED.fetch_add(missed, Ordering::Relaxed);
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
