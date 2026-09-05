//! Stage P: the networking stack.
//!
//! `akuma-net` (smoltcp, the AF_INET socket table, DNS) and `akuma-net-nic` (the
//! virtio-net device and its DMA arenas) on the amd64 kernel. Both build for
//! `x86_64-unknown-none` unchanged; what this module supplies is the twelve
//! function pointers in [`NetRuntime`] — the stack's entire upward surface.
//!
//! # What each hook costs on a target with one core and no interrupts
//!
//! The AArch64 kernel fills these with real scheduler primitives: a park that
//! marks a thread WAITING so a socket wake can target it, an interrupt check
//! that honours `tkill`, a netpoll doorbell rung by the NIC's IRQ handler. This
//! target has one core, no device interrupts and a cooperative round-robin, so
//! most of them collapse:
//!
//! | hook | here | why |
//! |---|---|---|
//! | `park_until` | yield in a loop until the deadline | no WAITING state to enter |
//! | `current_waker` | a no-op waker | nothing polls a future; the loop re-checks |
//! | `wake_netpoll` | no-op | there is no parked core to ring a doorbell at |
//! | `is_current_interrupted` | `false` | no signals |
//! | `current_box_id` | `0` | no containers |
//!
//! Collapsing them is correct *for this machine* and would be wrong the moment
//! it grows a second core or an IOAPIC. They are written out one by one rather
//! than defaulted so that each is a decision with a reason attached.
//!
//! # The clock
//!
//! `uptime_us` comes from the LAPIC tick counter, not the TSC. The TSC is finer
//! and would need calibrating against a known-rate source; the tick period is
//! already known because `lapic::start_timer` set it. Coarse but honest — and
//! the stack uses this for timeouts, where a 10 ms granularity is fine.

use akuma_net::NetRuntime;
use akuma_net::smoltcp_net::StaticIpv4;
use akuma_selftest::Suite;

use crate::serial;

/// Microseconds per LAPIC tick. Set by `lapic::start_timer`.
const US_PER_TICK: u64 = 10_000;

/// `clock.rs`'s SNTP bootstrap (2026-09-05) uses this same coarse-but-honest
/// tick clock for its own round-trip timing — one uptime source for the
/// whole target, not two that could disagree. `pub` rather than `pub(crate)`
/// because `mod net;` is itself private, which already bounds this to the
/// crate (clippy's `redundant_pub_crate`).
pub fn uptime_us() -> u64 {
    crate::lapic::ticks() * US_PER_TICK
}

/// Wall clock, via `clock.rs`'s SNTP bootstrap (2026-09-05) — `None` until it
/// succeeds (or hasn't run yet), matching this function's original contract:
/// a wrong answer is worse than no answer, so an unsynced clock stays `None`
/// rather than a plausible-looking zero. Nothing in `akuma-net` reads this
/// hook yet (checked: zero call sites), so wiring it doesn't change network
/// behavior today — it makes the *hook* honest for whenever something does.
fn utc_seconds() -> Option<u64> {
    crate::clock::utc_seconds()
}

/// Spin until `deadline_us`, yielding so anything else runnable makes progress.
///
/// The AArch64 version marks the thread WAITING so a socket wake can target it
/// directly. There is no such state here, so a parked waiter polls — which is
/// the same trade the block driver makes, and for the same reason.
fn park_until(deadline_us: u64) {
    // Cap the spin so a stalled clock (the LAPIC timer masked, e.g. between two
    // self-test stages) degrades to "return and let the caller re-check" rather
    // than "hang": `wait_until` re-evaluates its condition and re-drains the
    // stack immediately after this returns, so an early return is always safe.
    let mut guard = 0u32;
    while uptime_us() < deadline_us && guard < 100_000 {
        guard += 1;
        crate::sched::yield_now();
    }
}

/// A waker that does nothing.
///
/// `NetRuntime` wants one because the AArch64 stack parks threads and wakes them
/// through it. Nothing here is ever parked in a way a waker could resume — the
/// wait loops poll — so waking is a no-op rather than an unimplemented panic: a
/// stack that calls `wake()` on a waiter that is already spinning is behaving
/// correctly, and should not take the kernel down for it.
fn noop_waker() -> core::task::Waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: the vtable's four functions are all no-ops that ignore the data
    // pointer, so a null pointer is never dereferenced. This is the standard
    // no-op waker construction.
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// Does this CPU implement `RDRAND`?
///
/// CPUID leaf 1, `ECX` bit 30. Checked rather than assumed, because it is **not**
/// universal on the machines this target runs on: QEMU's default `microvm` CPU
/// model does not expose it, and executing the instruction there raises `#UD` —
/// which, before this check existed, took the kernel down immediately after
/// "net: stack up" with an invalid-opcode exception that read like a compiler
/// bug.
fn has_rdrand() -> bool {
    let ecx: u32;
    // SAFETY: `cpuid` is unprivileged and side-effect-free. `rbx` is
    // callee-saved and LLVM reserves it, so it is saved and restored by hand
    // rather than named as a clobber — naming it is a compile error.
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "mov rbx, {tmp:r}",
            tmp = out(reg) _,
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    ecx & (1 << 30) != 0
}

/// A counter-based fallback for machines without `RDRAND`.
///
/// **This is not a cryptographic source and must not be used as one.** It is
/// seeded from the timestamp counter and mixed with a SplitMix64 step, which is
/// enough to keep TCP initial sequence numbers from being identical every boot
/// and is nowhere near enough for key exchange.
///
/// It exists so that a machine without `RDRAND` still boots and still runs the
/// network stack, rather than taking a `#UD` at init. Anything that needs real
/// entropy has to check [`has_rdrand`] itself and refuse.
fn weak_fill(buf: &mut [u8]) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);

    let tsc: u64;
    // SAFETY: `rdtsc` is unprivileged, reads no memory and is available on
    // every x86_64 part.
    unsafe {
        let (lo, hi): (u32, u32);
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
        tsc = (u64::from(hi) << 32) | u64::from(lo);
    }
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ tsc;
    for chunk in buf.chunks_mut(8) {
        // SplitMix64.
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let n = chunk.len();
        chunk.copy_from_slice(&z.to_le_bytes()[..n]);
    }
}

/// Fill `buf` with random bytes, from `RDRAND`.
///
/// The stack needs randomness for TCP initial sequence numbers, and `sshd`
/// needs it for key exchange. `RDRAND` is the right source on this machine —
/// hardware, no seeding, available on every CPU this target can run on (it
/// predates the Zen and Skylake parts both machines use).
///
/// **A failed `RDRAND` is not papered over.** The instruction sets CF on
/// success and returns zero on failure, and hardware that is out of entropy
/// fails transiently. Retrying ten times is Intel's own recommendation; giving
/// up after that fills nothing and lets the caller fail, rather than handing a
/// key-exchange path a buffer of zeros that looks like randomness.
///
/// `pub` since Stage R: ring 3's `getrandom(2)` routes here, which is what
/// `sshd`'s key exchange rests on.
pub fn rng_fill(buf: &mut [u8]) {
    if !has_rdrand() {
        weak_fill(buf);
        return;
    }
    let mut i = 0;
    while i < buf.len() {
        let mut value: u64 = 0;
        let mut ok: u8 = 0;
        for _ in 0..10 {
            // SAFETY: `rdrand` writes a general register and sets CF. It reads
            // no memory and faults only on a CPU without the feature, which
            // every x86_64 part these machines run on has.
            unsafe {
                core::arch::asm!(
                    "rdrand {v}",
                    "setc {ok}",
                    v = out(reg) value,
                    ok = out(reg_byte) ok,
                    options(nomem, nostack),
                );
            }
            if ok != 0 {
                break;
            }
        }
        if ok == 0 {
            // Out of entropy after ten tries. Stop rather than continue with a
            // stale `value`: a short fill is a visible failure, a repeated
            // qword is not.
            return;
        }
        let n = (buf.len() - i).min(8);
        buf[i..i + n].copy_from_slice(&value.to_le_bytes()[..n]);
        i += n;
    }
}

/// The twelve `NetRuntime` hooks this target fills — see the module header for
/// why most collapse to yields and no-ops on a one-core, no-IRQ machine.
fn net_runtime() -> NetRuntime {
    NetRuntime {
        uptime_us,
        utc_seconds,
        yield_now: crate::sched::yield_now,
        // A yield: it switches to any runnable task, and when there is none it
        // drops the BKL for a moment so the other cores' syscalls get in — which
        // is exactly what "relax while blocked" has to mean under one lock.
        blocking_relax: crate::sched::yield_now,
        park_until,
        current_waker: noop_waker,
        current_core_id: crate::smp::cpu_index_u32,
        current_box_id: || 0,
        is_current_interrupted: || false,
        rng_fill,
        current_thread_id: || crate::sched::current_task() as u32,
        wake_netpoll: || {},
    }
}

/// Bring the networking stack up on virtio-net.
///
/// Returns false when there is no NIC, which is not an error — every boot before
/// this stage had none, and `run.sh` without a tap still has to work.
pub fn init(enable_dhcp: bool) -> bool {
    report_init(akuma_net::init(net_runtime(), enable_dhcp))
}

/// Bring the stack up with **no NIC** — loopback only.
///
/// The bare-metal (`multiboot2.rs`) path when there is no Realtek NIC: there is
/// no virtio-net either, but `socket(AF_INET)` still has to work for
/// `busybox ifconfig` and anything talking to `127.0.0.1`.
pub fn init_loopback_only() -> bool {
    report_init(akuma_net::init_loopback_only(net_runtime()))
}

/// The address this target carries on a real LAN when DHCP does not answer.
///
/// **Hardcoded, and deliberately not `10.0.2.15`.** Every other Akuma target is
/// a VMM guest whose user-mode network hands out that address; this one is a
/// desktop plugged into a household switch, where `10.0.2.15` is unroutable and
/// unreachable — the first bare-metal boot with the Realtek driver up came back
/// with exactly that, on a `192.168.1.0/24` LAN, which is how this constant came
/// to exist. `.220` is above the usual DHCP pool and below the broadcast
/// address, so it does not collide with a lease.
///
/// DHCP still runs and still wins when it answers: this is the fallback the
/// interface carries until then, and reverts to if the lease lapses. Override
/// it for one boot with `ip=` on the kernel command line — see [`parse_ip_arg`].
///
/// The resolver is **Cloudflare's `1.1.1.1`, not the gateway.** A VMM target
/// resolves through its own hypervisor's proxy (`10.0.2.3`) and can count on
/// it answering; a household router may or may not run a resolver, may hand out
/// one only over DHCP, and is the piece most likely to be replaced between two
/// boots of this machine. A public resolver is one fewer thing that has to be
/// true for name resolution to work on a box with no keyboard to fix it from —
/// and `ip=192.168.1.220/24,192.168.1.1,192.168.1.1` puts it back.
const BARE_METAL_STATIC_V4: StaticIpv4 = StaticIpv4 {
    addr: [192, 168, 1, 220],
    prefix_len: 24,
    gateway: [192, 168, 1, 1],
    dns: [1, 1, 1, 1],
};

/// Parse an `ip=` command-line token into a [`StaticIpv4`].
///
/// `ip=<addr>[/<prefix>][,<gateway>[,<dns>]]` — anything omitted is taken from
/// `default`, so `ip=192.168.1.77` moves only the address and
/// `ip=10.1.2.3/16,10.1.0.1,1.1.1.1` sets all four. Returns `None` for anything
/// it cannot parse, and the caller then uses `default`: a typo on the command
/// line of a machine with no keyboard must not leave it with no address at all.
fn parse_ip_arg(arg: &str, default: StaticIpv4) -> Option<StaticIpv4> {
    fn quad(s: &str) -> Option<[u8; 4]> {
        let mut out = [0u8; 4];
        let mut parts = s.split('.');
        for slot in &mut out {
            *slot = parts.next()?.parse().ok()?;
        }
        parts.next().is_none().then_some(out)
    }

    let mut fields = arg.split(',');
    let (addr_str, prefix_str) = match fields.next()?.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (arg.split(',').next()?, None),
    };
    let mut cfg = default;
    cfg.addr = quad(addr_str)?;
    if let Some(p) = prefix_str {
        let bits: u8 = p.parse().ok()?;
        if bits > 32 {
            return None;
        }
        cfg.prefix_len = bits;
    }
    if let Some(g) = fields.next() {
        cfg.gateway = quad(g)?;
    }
    if let Some(d) = fields.next() {
        cfg.dns = quad(d)?;
    }
    fields.next().is_none().then_some(cfg)
}

/// Bring the stack up on the Realtek RTL8169/8168 at the mapped register BAR
/// `bar` (from `pci::map_bar`), with `static_v4` as the pre-DHCP address.
/// DHCP on. Falls back to loopback-only if the chip does not come up.
///
/// # Safety
/// `bar` must be the NIC's device-mapped register BAR, live for the rest of
/// the boot; called once.
pub unsafe fn init_rtl8169(bar: *mut u8, static_v4: StaticIpv4) -> bool {
    // SAFETY: the caller's obligation on `bar`; called once, from `kmain_mb2`.
    let device = match unsafe { akuma_net::ExternalDevice::probe_rtl8169(bar) } {
        Ok(d) => d,
        Err(e) => {
            serial::puts("  nic:  RTL8169 bring-up failed (");
            serial::puts(e);
            serial::puts("); loopback only\n");
            return init_loopback_only();
        }
    };
    RTL8169_UP.store(true, core::sync::atomic::Ordering::Relaxed);
    report_init(akuma_net::init_with_external(net_runtime(), true, device, Some(static_v4)))
}

fn report_init(r: Result<(), &'static str>) -> bool {
    match r {
        Ok(()) => {
            serial::puts("  net:  stack up\n");
            true
        }
        Err(e) => {
            serial::puts("  net:  no stack: ");
            serial::puts(e);
            serial::puts("\n");
            false
        }
    }
}

/// Laps the netpoll daemon has completed. Read by the self-test to prove the
/// spawned task is actually being scheduled.
static NETPOLL_LAPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// One netpoll lap: drain `smoltcp_net::poll()` until it stops making progress
/// or the safety cap is hit. Returns the productive-poll count.
///
/// This is the amd64 analogue of `netpoll_drain_step` in `akuma-kernel-glue`.
/// Each `poll()` moves at most one RX frame (one virtio buffer), so a burst
/// needs the loop; the cap keeps one busy interface from starving the
/// round-robin. There is no `wfi` here and no NIC IRQ to end one early — on the
/// AArch64 side the interrupt only *shortens* the wait, so the loop is the
/// load-bearing part and this target has exactly the loop.
fn drain_step() -> u32 {
    let mut polls = 0u32;
    while akuma_net::smoltcp_net::poll() {
        polls += 1;
        if polls >= 64 {
            break;
        }
    }
    polls
}

/// A live NIC status line, printed by [`netpoll_daemon`] itself.
///
/// **Inside netpoll rather than beside it, deliberately.** The thing that
/// answers ARP, ICMP and a listening socket is the netpoll daemon being
/// scheduled; if it is not, the machine is off the network. A probe in its own
/// task can go quiet for two unrelated reasons — its own starvation, or
/// netpoll's — and those look identical from a photograph. Printing from
/// netpoll collapses that: **a line appearing proves the network is being
/// driven, and no line at all is itself the diagnosis.**
///
/// This exists because of one boot. On the HP box the only console is a
/// framebuffer, and the only network diagnostic available was `busybox
/// ifconfig` — which prints an address, a set of flags, and **counters that are
/// hardcoded zeros** (`akuma_syscalls_net::write_proc_net_dev` writes literal
/// `0`s; busybox reads them from `/proc/net/dev`). So a boot that showed
/// `eth0 UP ... RX packets:0 TX packets:0` was consistent with a NIC moving
/// nothing *and* with one moving thousands of frames a second, and there was no
/// way to tell which from the screen. That is a diagnostic that cannot fail,
/// which makes it worse than none.
///
/// Every number here is real and read from the device layer:
///
/// * `link` — the PHY, sampled by the Realtek glue (`akuma_net::smoltcp_net::link_state`).
///   Distinguishes "the cable is not carrying" from "the driver is not
///   receiving", which look identical from `ifconfig` and have completely
///   different fixes.
/// * `rx` — frames that actually came off the ring. On any real LAN this climbs
///   within seconds from broadcast traffic alone, with nothing configured; a
///   flat zero next to a link that is up is a receive-path bug and nothing else.
/// * `tx` / `drop` — frames the chip accepted / refused. DHCP retries on its own,
///   so `tx` climbing proves the transmit path without anything having to ask.
/// * `isr` / `dry` — every Realtek `ISR` bit seen so far, OR-ed, and how many
///   times the receive ring ran dry (`RDU`). `RDU` is a stall, not a status:
///   the chip stops receiving until it is written back.
/// * `polls` — `smoltcp_net::poll()` laps, the proof the stack is still being
///   driven at all. `cycle_forever` used to stop driving it the moment `init`
///   exited, which is how a machine that had answered nothing looked like a
///   dead NIC.
/// * `ticks` / `laps` — the raw LAPIC tick count and netpoll's own lap count.
///   Without these, "the line stopped appearing" has three explanations that
///   look identical in a photograph: the clock stopped, the scheduler stopped
///   running netpoll, or the kernel died.
//
///
/// The netpoll daemon: the thing that calls `smoltcp_net::poll()` between socket
/// calls, so DHCP completes and a listening server is actually serviced.
///
/// Spawned once (via [`spawn_netpoll`]) and never returns. It is a
/// [`crate::sched::spawn_daemon`] task, so the round-robin schedules it
/// alongside whatever `init=` program is running and the timer preempts a
/// compute-bound task onto it; `all_user_tasks_finished` ignores it so a shell
/// exiting still ends the boot.
extern "C" fn netpoll_daemon() -> ! {
    let mut next_lap = 0u64;
    let mut next_us = 0u64;
    loop {
        drain_step();
        let laps = NETPOLL_LAPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if PROBE_ON.load(core::sync::atomic::Ordering::Relaxed) {
            let now = uptime_us();
            if laps >= next_lap || now >= next_us {
                next_lap = laps + PROBE_LAPS;
                next_us = now + PROBE_PERIOD_US;
                print_probe_line(now, laps);
            }
        }
        crate::sched::yield_now();
    }
}

// The network probe
// ============================================================================

/// How often [`probe_daemon`] prints, in microseconds — when the clock works.
const PROBE_PERIOD_US: u64 = 2_000_000;

/// How many scheduler laps between prints when it does not.
///
/// **A probe must not depend on the thing it is there to diagnose.** The first
/// version of this daemon gated only on [`uptime_us`], and on the HP box it
/// printed exactly one line and never another — which is consistent with a
/// stalled clock *and* with a starved daemon, and told us which was happening:
/// neither. That is the same failure as `busybox ifconfig`'s hardcoded zeros,
/// committed one function after complaining about it.
///
/// The lap arm fires whatever the clock does; the time arm keeps a fast machine
/// from flooding the screen. Whichever comes first.
const PROBE_LAPS: u64 = 2_000_000;

/// Whether the netpoll daemon prints [`print_probe_line`] as it goes. Set by
/// [`enable_probe`] from the `netprobe` command-line flag.
static PROBE_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the Realtek came up, so [`enable_probe`] knows there is a DMA layout
/// worth dumping.
static RTL8169_UP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Turn the probe on. Not a separate task **on purpose** — see
/// [`netpoll_daemon`].
///
/// This is also where the NIC's DMA layout is printed, and the placement is the
/// point: it used to print during bring-up, which is **forty lines above where
/// the boot log ends**. On a machine whose console is a television being
/// photographed, anything printed before the self-tests has scrolled off by the
/// time there is something to photograph. `enable_probe` runs immediately
/// before `run_init`, so these four lines land at the bottom, next to the probe
/// output they explain.
pub fn enable_probe() {
    PROBE_ON.store(true, core::sync::atomic::Ordering::Relaxed);
    if RTL8169_UP.load(core::sync::atomic::Ordering::Relaxed) {
        // Where the chip has been told to write, against where we read. A wrong
        // translation here is invisible until it is catastrophic: the chip
        // writes descriptors and frames at an address the driver never looks
        // at, so the ring reads as permanently chip-owned, receive stops dead,
        // and whatever does live at that physical address is overwritten.
        for (name, va, pa) in akuma_net::smoltcp_net::Rtl8169Device::dma_layout() {
            serial::puts("  nic:  ");
            serial::puts(name);
            serial::puts(" va=0x");
            serial::put_hex(va as u64);
            serial::puts(" pa=0x");
            serial::put_hex(pa);
            serial::puts("\n");
        }
        // The block that keeps getting overwritten, so the two can be compared
        // by eye without anyone having to look up a symbol.
        serial::puts("  nic:  counters va=0x");
        serial::put_hex(akuma_net::smoltcp_net::counter_block_addr() as u64);
        serial::puts("\n");
    }
    serial::puts("  net:  probe enabled (netprobe)\n");
}

/// One status line. Split out so a caller with no scheduler (a one-shot dump)
/// can print the same thing.
pub fn print_probe_line(now_us: u64, laps: u64) {
    // **Two short lines, not one long one.** This console is 146 cells wide and
    // is read by photographing it; a 160-character line wraps, and a wrapped
    // line in a scrolling log does not read as one message continued — it reads
    // as two, with the second half of every field pushed onto a row that starts
    // mid-word. Worse, a photograph framed on the readable half then silently
    // omits whatever ran off the edge, which is how a shot of this very probe
    // arrived with `rx=`, `isr=` and `dry=` — the three fields it exists for —
    // outside the frame.
    //
    // The wire counters go on the second line and lead with the ones that
    // decide something, so a partial view still carries the answer.
    let info = akuma_net::smoltcp_net::interface_snapshot();
    serial::puts("[probe] t=");
    serial::put_dec(now_us / 1_000_000);
    serial::puts("s ticks=");
    serial::put_dec(crate::lapic::ticks());
    serial::puts(if crate::lapic::is_calibrated() { "(cal)" } else { "(GUESS)" });
    serial::puts(" link=");
    match akuma_net::smoltcp_net::link_state() {
        None => serial::puts("unsampled"),
        Some((up, mbit, fd)) => {
            serial::puts(if up { "up/" } else { "DOWN/" });
            serial::put_dec(u64::from(mbit));
            serial::puts(if fd { "M/full" } else { "M/half" });
        }
    }
    serial::puts(" ip=");
    for (i, o) in info.ip.iter().enumerate() {
        if i > 0 {
            serial::puts(".");
        }
        serial::put_dec(u64::from(*o));
    }
    serial::puts("/");
    serial::put_dec(u64::from(info.prefix_len));
    serial::puts(" dhcp=");
    serial::puts(if !akuma_net::smoltcp_net::is_dhcp_enabled() {
        "off"
    } else if akuma_net::smoltcp_net::is_dhcp_configured() {
        "leased"
    } else {
        "pending"
    });
    serial::puts("\n");

    let (canary_lo, canary_hi) = akuma_net::smoltcp_net::canaries_intact();
    if !canary_lo || !canary_hi {
        // Loud, and on its own line above the numbers it invalidates: once
        // something else has written into the counter block, every figure on
        // the next line is a reading of whatever that was.
        serial::puts("[probe] !! COUNTER MEMORY OVERWRITTEN: canary lo=");
        serial::puts(if canary_lo { "ok" } else { "CLOBBERED" });
        serial::puts(" hi=");
        serial::puts(if canary_hi { "ok" } else { "CLOBBERED" });
        serial::puts("\n");
    }

    let (posted, begin_fail, received) = akuma_net::smoltcp_net::rx_counters();
    let (isr, dry, kicks) = akuma_net::smoltcp_net::isr_history();
    serial::puts("[probe]   rx=");
    serial::put_dec(received as u64);
    serial::puts(" tx=");
    serial::put_dec(akuma_net::smoltcp_net::tx_frames_sent() as u64);
    serial::puts(" drop=");
    serial::put_dec(akuma_net::smoltcp_net::tx_drop_count() as u64);
    serial::puts(" isr=0x");
    serial::put_hexn(u64::from(isr), 4);
    serial::puts(" dry=");
    serial::put_dec(dry as u64);
    serial::puts(" kicks=");
    serial::put_dec(kicks as u64);
    serial::puts(" polls=");
    serial::put_dec(akuma_net::smoltcp_net::poll_count() as u64);
    serial::puts(" posted=");
    serial::put_dec(posted as u64);
    serial::puts(" rxfail=");
    serial::put_dec(begin_fail as u64);
    serial::puts(" irq=");
    serial::put_dec(akuma_net::smoltcp_net::nic_irq_count());
    serial::puts(" laps=");
    serial::put_dec(laps);
    serial::puts("\n");
}

/// Drive the stack by hand until DHCP settles, or `budget_ms` elapses.
///
/// The window this fills is narrow and load-bearing. The wall clock comes from
/// SNTP, SNTP needs an address and a route, and an address comes from DHCP —
/// so the clock cannot be fetched until the stack has settled. But SNTP also
/// **cannot run once the netpoll daemon exists**: it makes kernel-side socket
/// calls, and the first thing that ever did that concurrently with the daemon
/// deadlocked on a spinlock the daemon's own poll step takes, on this single
/// core, the instant preemption switched into it
/// (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30).
///
/// So the order is: drain here, fetch the clock, *then* spawn the daemon.
///
/// Returns whether DHCP actually configured. `false` is not fatal — the static
/// address stands and the boot goes on — it just means the clock will stay at
/// the epoch, and every TLS certificate on earth will look not-yet-valid.
pub fn settle_for_dhcp(budget_ms: u64) -> bool {
    let deadline = uptime_us() + budget_ms * 1000;
    while uptime_us() < deadline {
        drain_step();
        if akuma_net::smoltcp_net::is_dhcp_configured() {
            return true;
        }
    }
    akuma_net::smoltcp_net::is_dhcp_configured()
}

/// Spawn the netpoll daemon. Returns false only if the task table is full, which
/// is a bug (the table is sized for it) rather than a condition to handle.
pub fn spawn_netpoll() -> bool {
    let ok = crate::sched::spawn_daemon(netpoll_daemon).is_some();
    if ok {
        serial::puts("  net:  netpoll daemon spawned\n");
    } else {
        serial::puts("  net:  [WARN] no task slot for the netpoll daemon\n");
    }
    ok
}

/// Bring networking up on the multiboot2 (bare-metal) path.
///
/// A VMM announces a virtio-MMIO NIC; a real machine has a PCI Ethernet
/// controller instead. This finds it, enables decode + bus mastering, maps its
/// register BAR, and:
///
/// * a **Realtek RTL8169/8168** (`10ec:*`) → the real driver
///   (`akuma-net-nic`'s `rtl8169` glue over `akuma-net-rtl8169`), DHCP on, with
///   [`BARE_METAL_STATIC_V4`] (or `cmdline`'s `ip=`) as the pre-DHCP address;
/// * **anything else** (QEMU's e1000 in the OVMF rig) → loopback only, after
///   reading the vendor's ID register to prove the BAR mapping works;
/// * **no NIC at all** → loopback only.
///
/// Returns whether a socket layer is up (always true unless `akuma-net` itself
/// fails).
pub fn init_bare_metal(cmdline: &str) -> bool {
    use akuma_pci::{Bar, class, subclass};

    // Resolved before the NIC is even found, so a bad `ip=` is reported next to
    // the command line that carried it rather than three screens later.
    let mut static_v4 = BARE_METAL_STATIC_V4;
    if let Some(arg) = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("ip=")) {
        if let Some(cfg) = parse_ip_arg(arg, BARE_METAL_STATIC_V4) {
            static_v4 = cfg;
        } else {
            serial::puts("  nic:  [WARN] cannot parse ip=");
            serial::puts(arg);
            serial::puts("; using the built-in address\n");
        }
    }

    let Some(dev) = crate::pci::find_class(class::NETWORK, subclass::ETHERNET) else {
        return init_loopback_only();
    };
    serial::puts("  nic:  ");
    serial::put_hexn(u64::from(dev.header.vendor_id), 4);
    serial::puts(":");
    serial::put_hexn(u64::from(dev.header.device_id), 4);
    serial::puts(" at ");
    serial::put_hexn(u64::from(dev.addr.bus), 2);
    serial::puts(":");
    serial::put_hexn(u64::from(dev.addr.device), 2);
    serial::puts(".");
    serial::put_hexn(u64::from(dev.addr.function), 1);

    crate::pci::enable(dev.addr, true);

    // The register window: the first memory BAR (BAR2 on the Realtek part).
    let Some((idx, mem_bar)) = dev
        .bars
        .iter()
        .enumerate()
        .find_map(|(i, b)| b.filter(Bar::is_memory).map(|b| (i as u8, b)))
    else {
        serial::puts(" — no memory BAR; loopback only\n");
        return init_loopback_only();
    };
    let (size, _) = crate::pci::probe_bar_size(dev.addr, idx);
    let Some(regs) = crate::pci::map_bar(mem_bar, size.max(0x1000)) else {
        serial::puts(" — BAR map failed; loopback only\n");
        return init_loopback_only();
    };

    if dev.header.vendor_id == 0x10ec {
        serial::puts(" [RTL8169]\n");
        // SAFETY: `regs` is the just-mapped, device-attributed register BAR of
        // the Realtek NIC enumerated above; this is the one call.
        return unsafe { init_rtl8169(regs, static_v4) };
    }

    // Not a chip we drive. Read IDR0 to show the mapping works, then loopback.
    serial::puts(" id ");
    for i in 0..4u8 {
        // SAFETY: as above; offset 0 is a read-only ID register on every NIC.
        let b = unsafe { regs.add(i as usize).read_volatile() };
        serial::put_hexn(u64::from(b), 2);
    }
    serial::puts(" (unsupported NIC; loopback only)\n");
    init_loopback_only()
}

/// Check the pieces that do not need a NIC, plus the NIC if there is one.
pub fn smoke_test(t: &mut Suite, up: bool) {
    // `RDRAND` first, and independently of the stack: it is what `sshd`'s key
    // exchange will rest on, and a silent failure there is the worst kind.
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    rng_fill(&mut a);
    rng_fill(&mut b);
    t.check("net: the RNG fills a buffer", a.iter().any(|&x| x != 0));
    t.check("net: two draws differ", a != b);
    // Which source answered. Not a pass/fail — a machine without RDRAND is a
    // legitimate machine — but it decides whether anything here may be trusted
    // with a key, so it is reported every boot rather than inferred.
    if has_rdrand() {
        t.note("net: RDRAND present (hardware entropy)", 1);
    } else {
        serial::puts("  net:  [WARN] no RDRAND; using a NON-CRYPTOGRAPHIC fallback\n");
        t.note("net: RDRAND absent (weak fallback)", 0);
    }

    // The clock the stack times out against must move.
    let t0 = uptime_us();
    crate::sched::yield_now();
    t.check("net: the uptime clock is readable", uptime_us() >= t0);

    // `ip=`. Checked every boot on every entry (not only the bare-metal one)
    // because it is the one piece of network configuration a machine with no
    // keyboard cannot correct after the fact: a parser that silently widened a
    // prefix or dropped a gateway would strand the box, and it costs
    // microseconds to prove it did not.
    let d = BARE_METAL_STATIC_V4;
    t.check(
        "net: ip= takes an address alone",
        parse_ip_arg("192.168.1.77", d)
            == Some(StaticIpv4 { addr: [192, 168, 1, 77], ..d }),
    );
    t.check(
        "net: ip= takes address/prefix,gateway,dns",
        parse_ip_arg("10.1.2.3/16,10.1.0.1,1.1.1.1", d)
            == Some(StaticIpv4 {
                addr: [10, 1, 2, 3],
                prefix_len: 16,
                gateway: [10, 1, 0, 1],
                dns: [1, 1, 1, 1],
            }),
    );
    t.check("net: ip= rejects a prefix over 32", parse_ip_arg("10.0.0.1/33", d).is_none());
    t.check("net: ip= rejects an octet over 255", parse_ip_arg("10.0.0.256", d).is_none());
    t.check("net: ip= rejects a three-part address", parse_ip_arg("10.0.1", d).is_none());
    t.check("net: ip= rejects trailing junk", parse_ip_arg("10.0.0.1,10.0.0.2,8.8.8.8,x", d).is_none());
    t.check_eq(
        "net: the built-in bare-metal address is 192.168.1.220",
        u64::from(u32::from_be_bytes(BARE_METAL_STATIC_V4.addr)),
        u64::from(u32::from_be_bytes([192, 168, 1, 220])),
    );

    // A machine with no virtio-net device is legitimate — `DISK=none` under
    // QEMU, and Firecracker without `FC_NET=1` — so "no stack" is a skip, not a
    // failure, exactly as `sock::smoke_test` treats it. The RNG and clock checks
    // above still run because neither needs a NIC.
    if up {
        t.check("net: stack initialised", true);
    } else {
        t.note("net: no virtio-net device; stack skipped", 0);
    }
}

/// Stage Q: the netpoll daemon, the loop that was missing.
///
/// Split in two 2026-09-05 (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30)
/// — one call used to do both; `main.rs` now calls
/// [`netpoll_drain_selftest`] then [`netpoll_spawn_selftest`] itself, with
/// `clock.rs`'s SNTP fetch run in the gap between them: after DHCP (this
/// drain is where it actually finishes) but **before** the netpoll daemon
/// task exists. Calling any kernel-side `akuma_net::socket` function while
/// that daemon is alive and cooperatively scheduled is new territory: every
/// earlier kernel-side caller (`sock::smoke_test` included) ran with it not
/// yet spawned, and the first thing that *did* run concurrently with it —
/// `clock.rs`'s own DNS query — deadlocked on a spinlock the daemon's own
/// poll step also takes, on this single core, the instant `sti` let the
/// scheduler preempt into the daemon mid-critical-section. Not fixed at the
/// lock; avoided by not creating the second concurrent locker until this
/// window is closed.
///
/// Two things are checked, one per half. First
/// ([`netpoll_drain_selftest`]), that the drain **terminates** — the bug this
/// stage replaced was a settle loop keyed on a clock that had not started
/// yet, which spun `poll()` until QEMU's TX ring collapsed. With nothing
/// generating traffic, 64 back-to-back drains must end on a zero-progress
/// lap. Second ([`netpoll_spawn_selftest`]), that the spawned daemon is
/// actually **scheduled**: its lap counter must climb while the boot task
/// does nothing but `yield_now`.
///
/// [`netpoll_spawn_selftest`] leaves the daemon running (that is the point;
/// `run_init` needs it), so both run last in the self-test sequence, after
/// the leak and preemption checks.
///
/// The drain half. `false` (and a `t.note`, not a `t.check` — "no stack" is
/// not a failure) means the caller should not go on to
/// [`netpoll_spawn_selftest`].
pub fn netpoll_drain_selftest(t: &mut Suite, up: bool) -> bool {
    if !up {
        t.note("net: netpoll skipped (no stack)", 0);
        return false;
    }
    let mut last = u32::MAX;
    for _ in 0..64 {
        last = drain_step();
    }
    t.check_eq("net: the netpoll drain reaches quiescence", u64::from(last), 0);
    true
}

/// The spawn half — see [`netpoll_drain_selftest`]'s doc. Only call this once
/// the drain half has run (`netpoll_drain_selftest` returned `true`) — it
/// does not check `up` itself.
pub fn netpoll_spawn_selftest(t: &mut Suite) {
    use core::sync::atomic::Ordering;

    let before = NETPOLL_LAPS.load(Ordering::Relaxed);
    if t.check("net: netpoll daemon spawned", spawn_netpoll()) {
        for _ in 0..4_000 {
            crate::sched::yield_now();
        }
        let laps = NETPOLL_LAPS.load(Ordering::Relaxed) - before;
        t.check("net: the netpoll daemon is being scheduled", laps > 100);
        t.note("net: netpoll laps per 4000 boot-task yields", laps);
    }
}
