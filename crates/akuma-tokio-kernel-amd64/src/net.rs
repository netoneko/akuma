//! Stage 4: the network path.
//!
//! `akuma-net` (smoltcp, the AF_INET socket table) and `akuma-net-nic` (the
//! virtio-net device and its DMA arenas) pulled in as crates — the socket
//! *decisions* were already extracted, and the device layer with them; what
//! this module supplies is the twelve function pointers in [`NetRuntime`]
//! (the stack's entire upward surface) plus the two things a no-scheduler,
//! no-interrupt kernel has to add by hand: a clock for smoltcp's timers, and
//! a place to pump the NIC while a task sleeps or waits.
//!
//! # The clock
//!
//! smoltcp stamps everything (retransmit, DHCP, ARP ageing) with `now()` in
//! microseconds. The source is the TSC, calibrated once at boot against the
//! PIT — the same
//! 8254 reference `akuma-amd64` uses, and the one device kvm0 emulates well
//! enough to answer the channel-2 gate/output handshake through port 0x61
//! (`amd64/src/kvm/linux.rs` wires 0x40–0x43 and 0x61). The calibration is
//! bounded: if the PIT never reports its output pin, the TSC rate stays a
//! guess and the boot says so — timers that drift make TCP slower, not
//! wrong.
//!
//! # Pumping the NIC
//!
//! The guest has no IOAPIC and kvm0 never raises the NIC's interrupt line:
//! frames arrive only when somebody calls [`netpoll`]. The idle thread drives
//! it once per lap, the timer tick once per tick, and `wait_until`'s laps
//! drive it from inside `akuma-net` while a blocking socket call waits. A
//! sleeping task (glue's `nanosleep`) parks, so the core idles and the idle
//! lap pumps.

use akuma_tokio_kernel_core::net::{self, NetMachine};

// ---------------------------------------------------------------------------
// Port I/O — x86_64-only by construction (this crate never joins the root
// `default-members`), still written as cfg-gated helpers so a stray host
// build fails at compile time instead of at instruction decode.
// ---------------------------------------------------------------------------

/// One spin-loop hint.
#[inline]
fn pause() {
    core::hint::spin_loop();
}

/// One WAIT lap that hands the host core back: the vCPU halts (a VM exit)
/// until the next interrupt. The relay thread shares this core with the
/// vCPU — the one-pinned-core note in KVM_RELAY_SYN_STALL.md — so a guest
/// that only pause()-spins pegs the core and the relay starves: boot
/// ph-6's P3 stall showed ~10 s of frozen gw_frames/rx delivery behind
/// `hlt=0` and the host's guest core at 100%. `sti; hlt; cli` opens IF
/// for exactly one instruction, so the LAPIC tick (armed before any
/// caller) always wakes the lap. Wait loops only: the calibrator's PIT
/// gate poll keeps the bare pause (an hlt would skew the TSC window).
#[inline]
pub fn wait_lap() {
    // `sti; hlt; cli`: IF is open for the one hlt and restored right after; the
    // only unmasked source is the tick, whose handler is reentrant with every
    // caller of this fn.
    crate::smp::park_halt();
}

/// Read the timestamp counter.
fn rdtsc() -> u64 {
    akuma_cpu::tsc::rdtsc()
}

// ---------------------------------------------------------------------------
// The TSC clock, calibrated once against the PIT.
// ---------------------------------------------------------------------------

/// TSC increments per microsecond; 0 until [`calibrate`] has run.
static TSC_PER_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Whether the PIT was there to calibrate against (`SYSTEM_CONTROL_B.present()` measured
/// port 0x61 answering, and the window closed before the TSC drift could).
static TSC_CALIBRATED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Fallback rate for a machine with no PIT: 2.5 GHz, the rough order of the
/// box's parts. Timer-driven behaviour (TCP retransmit, DHCP retry, the
/// serve loop's 1 ms tick) scales with the error; the gate does not measure
/// time, so a wrong guess costs latency, never correctness. The boot says
/// the rate is a guess.
const TSC_PER_US_GUESS: u64 = 2_500;

/// The 8254's crystal frequency, Hz.
const PIT_HZ: u64 = 1_193_182;
/// Calibration window: ~50 ms of PIT counts (short enough to not slow the
/// boot, long enough that TSC noise is rounding error).
const PIT_COUNT: u32 = (PIT_HZ * 50_000 / 1_000_000) as u32;
/// Bound on the OUT-pin wait, in spin iterations: the window above is 50 ms,
/// so a wait ten times that means the gate never opened (no PIT behind port
/// 0x61 — QEMU's `microvm` machine is wired exactly that way) and the boot
/// should move on with the guessed rate rather than hang.
const OUT_WAIT_BUDGET: u32 = 20_000_000;

/// Calibrate the TSC against PIT channel 2, polled — no interrupts involved.
///
/// Called once, from `kernel_main`, after `lapic_init` (whose own calibration is the
/// fallback rate) and **before** `exec_twin::init` registers [`uptime_us`] as the
/// kernel's clock: registered first, the clock would read `rdtsc / 1` until this ran
/// and then jump backwards under every deadline already computed against it.
///
/// Channel 2 in mode 0 (interrupt-on-terminal-count), gate enabled through
/// port B, then the OUT pin polled until the count expires. `tsc / elapsed`
/// is the rate. Every step is bounded so a missing PIT costs one guessed
/// clock, never a hang.
/// No usable PIT: take the TSC rate the LAPIC timer calibration already measured
/// (`akuma_lapic_amd64::tsc_hz`, which runs before this), and only guess if that has not run.
///
/// The fixed guess alone made every guest clock wrong by `real / 2500`: under KVM there is no PIT
/// behind port 0x61, the host TSC is ~3.8 GHz, and the "3m09" a build printed took 129 s on the
/// host - 1.5x fast (`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` section 3).
fn use_lapic_rate(why: &str) {
    let hz = akuma_lapic_amd64::tsc_hz();
    if hz >= 1_000_000 {
        let per_us = hz / 1_000_000;
        TSC_PER_US.store(per_us, core::sync::atomic::Ordering::Relaxed);
        TSC_CALIBRATED.store(true, core::sync::atomic::Ordering::Relaxed);
        akuma_primitives::safe_print!(96, "  net: {why} -- TSC rate from the LAPIC calibration: {} cyc/us\n", per_us);
    } else {
        TSC_PER_US.store(TSC_PER_US_GUESS, core::sync::atomic::Ordering::Relaxed);
        akuma_primitives::safe_print!(96, "  net: {why} -- TSC rate is a guess ({TSC_PER_US_GUESS}/us)\n");
    }
}

pub fn calibrate() {
    if !akuma_port::SYSTEM_CONTROL_B.present() {
        use_lapic_rate("no PIT behind port 0x61");
        return;
    }
    // Channel 2, mode 0, the count; then raise its gate (speaker off) so it starts down, and poll OUT.
    akuma_port::PIT.load_channel2_oneshot(PIT_COUNT as u16);
    let saved = akuma_port::SYSTEM_CONTROL_B.begin_channel2();
    let t0 = rdtsc();
    let mut guard = 0u32;
    while !akuma_port::SYSTEM_CONTROL_B.channel2_output_high() {
        pause();
        guard += 1;
        if guard > OUT_WAIT_BUDGET {
            break;
        }
    }
    let t1 = rdtsc();
    akuma_port::SYSTEM_CONTROL_B.end_channel2(saved);
    if guard > OUT_WAIT_BUDGET || t1 <= t0 {
        use_lapic_rate("PIT output never rose");
        return;
    }
    let elapsed_us = (u64::from(PIT_COUNT) * 1_000_000) / PIT_HZ;
    let per_us = (t1 - t0).max(1) / elapsed_us.max(1);
    TSC_PER_US.store(per_us.max(1), core::sync::atomic::Ordering::Relaxed);
    TSC_CALIBRATED.store(true, core::sync::atomic::Ordering::Relaxed);
    akuma_primitives::safe_print!(96, "  net: TSC calibrated vs PIT: {} cyc/us\n", per_us);
}


/// Microseconds since boot (the calibrated TSC): the kernel's one clock. The
/// scheduler, `akuma-exec`, `akuma_primitives::clock` (so glue's time syscalls),
/// futex deadlines and smoltcp all read this function (`exec_twin::init`).
pub fn uptime_us() -> u64 {
    let per_us = TSC_PER_US.load(core::sync::atomic::Ordering::Relaxed).max(1);
    rdtsc() / per_us
}

// ---------------------------------------------------------------------------
// Randomness.
// ---------------------------------------------------------------------------

/// Does this CPU implement `RDRAND`? CPUID leaf 1, ECX bit 30 — checked,
/// not assumed: executing the instruction without the feature is `#UD`.
#[cfg(target_arch = "x86_64")]
fn has_rdrand() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0
}

/// A counter-based fallback, seeded from the TSC and mixed with SplitMix64.
/// **Not a cryptographic source** — enough to keep TCP initial sequence
/// numbers from repeating every boot; not enough for key exchange. The
/// stage's honest answer for a CPU without `RDRAND`; the gate's byte
/// comparison does not depend on which one ran.
fn weak_fill(buf: &mut [u8]) {
    static STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let seed = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed) ^ rdtsc();
    akuma_primitives::rng::splitmix_fill(seed, buf);
}

/// One word from `RDRAND`, retrying transient failures ten times; `None`
/// when the hardware stays out of entropy. The instruction (and those
/// retries — Intel's own recommendation) is `akuma_cpu::rng::rdrand`'s, a
/// byte-identical twin of the copy this file and `amd64/src/net.rs` each
/// carried until it moved there.
#[cfg(target_arch = "x86_64")]
fn rdrand_word() -> Option<u64> {
    akuma_cpu::rng::rdrand()
}

/// Fill `buf` from `RDRAND`; falls back to [`weak_fill`] when the CPU lacks the
/// instruction, and for the tail if the hardware runs out of entropy (stated
/// weak). Registered as both the `NetRuntime` hook (TCP ISNs) and the
/// `getrandom(2)` arm (`sshd`'s key exchange reads it). The fill loop is
/// `akuma_primitives::rng`'s; the instruction and this policy stay here.
pub fn rng_fill(buf: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    if has_rdrand() {
        if let Err(filled) = akuma_primitives::rng::fill_from_words(buf, rdrand_word) {
            weak_fill(&mut buf[filled..]);
        }
        return;
    }
    weak_fill(buf);
}

// ---------------------------------------------------------------------------
// The socket layer's table, pump and bring-up are core's
// (`akuma_tokio_kernel_core::net`); this machine supplies the clock, the lap and
// the entropy.
// ---------------------------------------------------------------------------

/// The amd64 machine as `akuma_tokio_kernel_core::net` sees it.
pub struct Amd64Net;

impl NetMachine for Amd64Net {
    fn uptime_us() -> u64 {
        uptime_us()
    }
    fn wait_lap() {
        wait_lap();
    }
    fn rng_fill(buf: &mut [u8]) {
        rng_fill(buf);
    }
}

/// One lap of the stack: the tick and the idle thread call it.
pub fn netpoll() {
    net::netpoll::<Amd64Net>();
}

/// The socket layer with no NIC.
pub fn loopback_init() {
    net::loopback_init::<Amd64Net>();
}

/// The stack on virtio-net and the DHCP wait; the clock was calibrated earlier
/// ([`calibrate`], before `exec_twin::init`).
#[cfg(feature = "net")]
pub fn net_init() -> bool {
    net::net_init::<Amd64Net>()
}
