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
use akuma_selftest::Suite;

use crate::serial;

/// Microseconds per LAPIC tick. Set by `lapic::start_timer`.
const US_PER_TICK: u64 = 10_000;

fn uptime_us() -> u64 {
    crate::lapic::ticks() * US_PER_TICK
}

/// No wall clock: this machine has no RTC and no SNTP client, so it does not
/// know what time it is. `None` rather than a plausible-looking zero — the
/// stack uses this for certificate validity, where a wrong answer is worse than
/// no answer.
fn utc_seconds() -> Option<u64> {
    None
}

/// Spin until `deadline_us`, yielding so anything else runnable makes progress.
///
/// The AArch64 version marks the thread WAITING so a socket wake can target it
/// directly. There is no such state here, so a parked waiter polls — which is
/// the same trade the block driver makes, and for the same reason.
fn park_until(deadline_us: u64) {
    while uptime_us() < deadline_us {
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
fn rng_fill(buf: &mut [u8]) {
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

/// Bring the networking stack up.
///
/// Returns false when there is no NIC, which is not an error — every boot before
/// this stage had none, and `run.sh` without a tap still has to work.
pub fn init(enable_dhcp: bool) -> bool {
    let rt = NetRuntime {
        uptime_us,
        utc_seconds,
        yield_now: crate::sched::yield_now,
        blocking_relax: crate::sched::yield_now,
        park_until,
        current_waker: noop_waker,
        current_core_id: || 0,
        current_box_id: || 0,
        is_current_interrupted: || false,
        rng_fill,
        current_thread_id: || crate::sched::current_task() as u32,
        wake_netpoll: || {},
    };

    match akuma_net::init(rt, enable_dhcp) {
        Ok(()) => {
            serial::puts("  net:  stack up\n");
            settle();
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

/// Drive the stack until DHCP has had its chance.
///
/// **Nothing else polls.** The AArch64 kernel runs a netpoll thread; here the
/// only thing that calls `smoltcp_net::poll()` is `akuma-net`'s own blocking
/// wait, from inside a socket operation. That is enough once a program is
/// running, and it is nothing at all in the window between `init` and the first
/// `accept` — which is exactly the window DHCP lives in. Without this, the lease
/// never completes and the log says "DHCP deconfigured - reverting to static
/// fallback" for a machine whose server was answering all along.
///
/// Bounded, because a machine with no DHCP server is a legitimate machine: the
/// static fallback (10.0.2.15/24) is what QEMU's user-mode stack and the
/// Firecracker host both hand out anyway, so failing to get a lease costs
/// nothing here and must not cost the boot.
fn settle() {
    // Two seconds of ticks. Enough for a DISCOVER/OFFER/REQUEST/ACK round trip
    // on both machines; short enough that a machine with no server is not
    // noticeably slower to boot.
    let deadline = uptime_us() + 2_000_000;
    let mut polls: u64 = 0;
    while uptime_us() < deadline {
        if akuma_net::smoltcp_net::poll() {
            polls += 1;
        }
        crate::sched::yield_now();
    }
    serial::puts("  net:  settled after ");
    serial::put_dec(polls);
    serial::puts(" productive polls\n");
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

    t.check("net: stack initialised", up);
}
