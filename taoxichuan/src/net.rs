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
//! microseconds. Taoxichuan has no LAPIC driver and no timer tick, so the
//! source is the TSC, calibrated once at boot against the PIT — the same
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
//! frames arrive only when somebody calls [`netpoll`]. Every path a task can
//! block on goes through here — the `nanosleep` arms drive it per lap (this
//! is what runs sshd's serve loop), and `wait_until`'s laps drive it from
//! inside `akuma-net` while a blocking socket call waits. Nothing drives it
//! while a spawned child runs to completion inside its SPAWN syscall; that
//! is accepted (the stage's child model), not hidden — see `child.rs`.

use akuma_net::NetRuntime;

// ---------------------------------------------------------------------------
// Port I/O — x86_64-only by construction (this crate never joins the root
// `default-members`), still written as cfg-gated helpers so a stray host
// build fails at compile time instead of at instruction decode.
// ---------------------------------------------------------------------------

/// Write one byte to an x86 I/O port.
#[cfg(target_arch = "x86_64")]
fn outb(port: u16, val: u8) {
    // SAFETY: a device port this module owns the use of (the PIT block
    // below); kernel-only, no side effects outside the timer.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

/// Read one byte from an x86 I/O port.
#[cfg(target_arch = "x86_64")]
fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: as `outb` — the PIT block only.
    unsafe {
        core::arch::asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    val
}

/// One spin-loop hint.
#[inline]
fn pause() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a hint instruction, no state.
    unsafe { core::arch::asm!("pause", options(nomem, nostack, preserves_flags)) }
    #[cfg(not(target_arch = "x86_64"))]
    core::hint::spin_loop();
}

/// Read the timestamp counter.
#[cfg(target_arch = "x86_64")]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: reads a counter register.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

#[cfg(not(target_arch = "x86_64"))]
fn rdtsc() -> u64 {
    0
}

// ---------------------------------------------------------------------------
// The TSC clock, calibrated once against the PIT.
// ---------------------------------------------------------------------------

/// TSC increments per microsecond; 0 until [`calibrate`] has run.
static TSC_PER_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Whether the PIT was there to calibrate against (`pit_present` measured
/// port 0x61 answering, and the window closed before the TSC drift could).
static TSC_CALIBRATED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Fallback rate for a machine with no PIT: 2.5 GHz, the rough order of the
/// box's parts. Timer-driven behaviour (TCP retransmit, DHCP retry, the
/// serve loop's 1 ms tick) scales with the error; the gate does not measure
/// time, so a wrong guess costs latency, never correctness. The boot says
/// the rate is a guess.
const TSC_PER_US_GUESS: u64 = 2_500;

/// The PIT ports: channel 2 count/gate, mode register, and port B (gate
/// enable in bit 0, the channel's OUT pin readable in bit 5).
const PIT_CH2: u16 = 0x42;
const PIT_MODE: u16 = 0x43;
const PIT_PORT_B: u16 = 0x61;
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
/// Channel 2 in mode 0 (interrupt-on-terminal-count), gate enabled through
/// port B, then the OUT pin polled until the count expires. `tsc / elapsed`
/// is the rate. Every step is bounded so a missing PIT costs one guessed
/// clock, never a hang.
fn calibrate() {
    if !pit_present() {
        TSC_PER_US.store(TSC_PER_US_GUESS, core::sync::atomic::Ordering::Relaxed);
        akuma_primitives::safe_print!(
            96,
            "  net: no PIT behind port 0x61 -- TSC rate is a guess ({TSC_PER_US_GUESS}/us)\n"
        );
        return;
    }
    // Program channel 2: mode 0, lobyte/hibyte, the count.
    outb(PIT_MODE, 0b1011_0000);
    let count = PIT_COUNT as u16;
    outb(PIT_CH2, count as u8);
    outb(PIT_CH2, (count >> 8) as u8);
    // Gate on (bit 0), speaker off (bit 1) — the channel starts counting.
    let pb = inb(PIT_PORT_B);
    outb(PIT_PORT_B, (pb & 0xFD) | 0x01);
    let t0 = rdtsc();
    let mut guard = 0u32;
    while inb(PIT_PORT_B) & 0x20 == 0 {
        pause();
        guard += 1;
        if guard > OUT_WAIT_BUDGET {
            break;
        }
    }
    let t1 = rdtsc();
    // Gate off.
    outb(PIT_PORT_B, pb & 0xFC);
    if guard > OUT_WAIT_BUDGET || t1 <= t0 {
        TSC_PER_US.store(TSC_PER_US_GUESS, core::sync::atomic::Ordering::Relaxed);
        akuma_primitives::safe_print!(
            96,
            "  net: PIT output never rose -- TSC rate is a guess ({TSC_PER_US_GUESS}/us)\n"
        );
        return;
    }
    let elapsed_us = (u64::from(PIT_COUNT) * 1_000_000) / PIT_HZ;
    let per_us = (t1 - t0).max(1) / elapsed_us.max(1);
    TSC_PER_US.store(per_us.max(1), core::sync::atomic::Ordering::Relaxed);
    TSC_CALIBRATED.store(true, core::sync::atomic::Ordering::Relaxed);
    akuma_primitives::safe_print!(96, "  net: TSC calibrated vs PIT: {} cyc/us\n", per_us);
}

/// Does port B answer at all? A write of the gate bit read back is the
/// cheapest honest probe; a bus with no device behind it reads back what the
/// last write to any port left, which this kernel never does — so a stuck
/// value reads as "no PIT", the safe answer (a guessed clock, one line).
#[cfg(target_arch = "x86_64")]
fn pit_present() -> bool {
    let orig = inb(PIT_PORT_B);
    let flipped = orig ^ 0x01;
    outb(PIT_PORT_B, flipped);
    let readback = inb(PIT_PORT_B);
    outb(PIT_PORT_B, orig);
    readback & 0x01 == flipped & 0x01
}

#[cfg(not(target_arch = "x86_64"))]
fn pit_present() -> bool {
    false
}

/// Microseconds since boot (the calibrated TSC), the stack's one clock.
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
    let tsc = rdtsc();
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed) ^ tsc;
    for chunk in buf.chunks_mut(8) {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let n = chunk.len();
        chunk.copy_from_slice(&z.to_le_bytes()[..n]);
    }
}

/// Fill `buf` from `RDRAND`, retrying transient failures ten times (Intel's
/// own recommendation); falls back to [`weak_fill`] when the CPU lacks the
/// instruction. Registered as both the `NetRuntime` hook (TCP ISNs) and the
/// `getrandom(2)` arm (`sshd`'s key exchange reads it).
pub fn rng_fill(buf: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    if has_rdrand() {
        let mut i = 0;
        while i < buf.len() {
            let mut value: u64 = 0;
            let mut ok: u8 = 0;
            for _ in 0..10 {
                // SAFETY: `rdrand` writes a general register and sets CF; no
                // memory, no stack.
                unsafe {
                    core::arch::asm!(
                        "rdrand {v}", "setc {ok}",
                        v = out(reg) value, ok = out(reg_byte) ok,
                        options(nomem, nostack),
                    );
                }
                if ok != 0 {
                    break;
                }
            }
            if ok == 0 {
                weak_fill(&mut buf[i..]); // out of entropy: stated-weak tail
                return;
            }
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&value.to_le_bytes()[..n]);
            i += n;
        }
        return;
    }
    weak_fill(buf);
}

// ---------------------------------------------------------------------------
// The NetRuntime table — twelve function pointers, each collapsed to what a
// one-core, no-interrupt, no-scheduler kernel can honestly answer. The same
// collapses `akuma-amd64` makes (see `amd64/src/net.rs`); written out one by
// one so each is a decision with a reason attached.
// ---------------------------------------------------------------------------

/// Spin until `deadline_us`. The wait loops call this as a *backstop* between
/// re-checks, and every lap pumps the NIC — on this guest there is no
/// interrupt to wake anybody, so a backstop that did not poll would add its
/// whole duration to every socket latency.
fn park_until(deadline_us: u64) {
    while uptime_us() < deadline_us {
        netpoll();
        pause();
    }
}

/// One lap of a socket waiter with nothing to do. `wait_until` drives
/// `poll()` itself at the top of every lap; this hook only has to burn a
/// bounded moment before the next re-check.
fn blocking_relax() {
    for _ in 0..64 {
        pause();
    }
}

/// A waker that does nothing: nothing here is ever parked in a way a waker
/// could resume — the wait loops poll.
fn noop_waker() -> core::task::Waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: the vtable's four functions are all no-ops that ignore the
    // data pointer, so a null pointer is never dereferenced.
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

fn runtime() -> NetRuntime {
    /// No parked core to ring a doorbell at: loopback frames ride the next
    /// poll lap, and there is no `wfi` sleeper to end early.
    fn noop() {}
    NetRuntime {
        uptime_us,
        utc_seconds: || None, // no wall clock; nothing in akuma-net reads this hook
        yield_now: pause,     // nothing to yield to: one task, one core
        blocking_relax,
        park_until,
        current_waker: noop_waker,
        current_core_id: || 0,
        current_box_id: || 0,
        is_current_interrupted: || false, // no signal delivery in this kernel
        rng_fill,
        current_thread_id: || 0,
        wake_netpoll: noop,
    }
}

// ---------------------------------------------------------------------------
// Bring-up and the poll pump.
// ---------------------------------------------------------------------------

/// One lap of the stack: drain the NIC into smoltcp, run its timers. The
/// everything-path for inbound frames on this guest.
// [tmp] wedge-Q5: does the pump keep laps during the stall? (remove before final)
static NP_LAPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn netpoll() {
    NP_LAPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if NP_LAPS.load(core::sync::atomic::Ordering::Relaxed) & 0x3FFF == 0 {
        akuma_primitives::safe_print!(
            96,
            "[tmp] np lap={}\n",
            NP_LAPS.load(core::sync::atomic::Ordering::Relaxed)
        );
    }
    let _ = akuma_net::smoltcp_net::poll();
}

/// Calibration, then the stack itself (probe virtio-net, bring up smoltcp,
/// start DHCP). Then the DHCP wait: bounded, pumping the NIC, so the guest
/// has its address before init asks the socket layer for a port.
pub fn net_init() -> bool {
    akuma_primitives::safe_print!(96, "  net: [1] calibrate begin\n");
    calibrate();
    akuma_primitives::safe_print!(96, "  net: [2] stack init begin\n");
    if akuma_net::init(runtime(), true).is_err() {
        akuma_comconsole::puts("  net: stack init failed -- no virtio-net device\n");
        return false;
    }
    akuma_comconsole::puts("  net: smoltcp stack up, DHCP requested\n");
    let deadline = uptime_us() + 10_000_000; // 10 s
    while uptime_us() < deadline {
        netpoll();
        if akuma_net::smoltcp_net::is_dhcp_configured() {
            let ip = akuma_net::smoltcp_net::static_ipv4().addr;
            akuma_primitives::safe_print!(
                96,
                "  net: DHCP configured, {}.{}.{}.{}\n",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
            return true;
        }
        pause();
    }
    akuma_comconsole::puts("  net: DHCP not answered in 10 s -- continuing unaddressed\n");
    false
}

/// Nanosleep the only way this kernel can: a bounded loop that pumps the
/// NIC every lap. This is the sshd serve loop's `sleep_ms(1)` — which makes
/// it, not any daemon, the thing that keeps TCP alive between events.
pub fn sleep_until(deadline_us: u64) {
    while uptime_us() < deadline_us {
        netpoll();
        pause();
    }
}
