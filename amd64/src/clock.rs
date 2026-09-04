//! A wall clock, bootstrapped once at boot via SNTP.
//!
//! This target has no RTC (Firecracker's device model has none; QEMU
//! `microvm` doesn't either) and, until now, no way to learn what time it
//! is at all — every `clock_gettime`/`gettimeofday`/`time` answered with
//! epoch 0, and every real TLS certificate has a `notBefore` decades after
//! 1970, so every HTTPS connection out of this target failed validation
//! (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.29.5 — found while getting
//! `apk` working, which is what this exists to fix).
//!
//! # The clock this keeps is not a real clock
//!
//! It syncs **once**, right after the network comes up, and then just adds
//! elapsed [`net::uptime_us`] to that one reading forever — there is no
//! ongoing drift correction, no periodic re-sync, and the uptime source it
//! rides on is itself the "coarse but honest" 10 ms-granularity LAPIC tick
//! count `net.rs` already uses for network timeouts, not a calibrated
//! hardware clock. That is a deliberate scope cut, not an oversight: the
//! actual requirement — TLS certificate date validation — only needs the
//! answer to be right to within the *year*, arguably the *day*, never the
//! millisecond, and a stock-static-musl program's own TLS stack (`apk`,
//! `wget`) is the only consumer. A real clock (periodic re-sync, drift
//! slewing, an `adjtimex`-style gradual correction) is what `akuma-syscalls-
//! time` already implements properly for platforms with a calibrated timer —
//! see that crate's own header — and is not what this file is.
//!
//! # Why SNTP and not the kernel command line
//!
//! `docs/archive/MISSING_NTP_SYSCALLS.md` names a simpler alternative for the
//! same problem on the AArch64/Firecracker side: seed the clock from a
//! `boot_args` token the host writes at VM-launch time (`akuma.epoch=<unix
//! seconds>`), since the host already knows the time and QEMU/Firecracker
//! both hand the guest its command line. That is genuinely simpler and this
//! target could still grow it as a fallback later, but it was not chosen as
//! the *primary* mechanism here: it only works when whoever launches the VM
//! remembers to pass the flag, where SNTP works against any boot script
//! nobody thought to update, including a future Firecracker deployment this
//! code has not been run under yet.
//!
//! # Layering
//!
//! The SNTP wire protocol and the send/poll/receive/timeout retry loop are
//! [`akuma_sntp::sntp`]/[`akuma_sntp::boot`] — pure, host-tested, shared with
//! (not duplicated from) `akuma-syscalls-time`'s own copy; see that crate's
//! header for why the AArch64 kernel does not wire the same loop up yet. This
//! file is only the amd64-specific effects: a UDP socket via
//! `akuma_net::socket`, DNS resolution via `akuma_net::dns`, and the local
//! uptime/yield hooks `net.rs` already has.

use akuma_net::socket::socket_const::SOCK_DGRAM;
use akuma_net::socket::SocketAddrV4;
use akuma_sntp::boot::{bootstrap_over_udp, BootstrapEffects};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::serial;

/// Unix epoch microseconds at the instant [`ANCHOR_UPTIME_US`] was read.
/// `0` means "never synced" — see [`is_synced`]. Never legitimately `0`
/// itself (that would be 1970-01-01T00:00:00Z, not a value SNTP will ever
/// hand back for the present day), so the sentinel cannot collide with a
/// real reading.
static ANCHOR_UNIX_US: AtomicU64 = AtomicU64::new(0);
/// [`net::uptime_us`] at the same instant as `ANCHOR_UNIX_US`.
static ANCHOR_UPTIME_US: AtomicU64 = AtomicU64::new(0);

/// Has [`sync_via_sntp`] ever succeeded?
#[must_use]
pub fn is_synced() -> bool {
    ANCHOR_UNIX_US.load(Ordering::Relaxed) != 0
}

/// Current wall-clock time, in microseconds since the Unix epoch. `0` if
/// never synced — matching `fs::no_clock`'s "0 is the honest answer, not a
/// guess dressed up as one" stance elsewhere on this target, rather than a
/// plausible-looking but wrong value.
#[must_use]
pub fn now_us() -> u64 {
    let anchor_unix = ANCHOR_UNIX_US.load(Ordering::Relaxed);
    if anchor_unix == 0 {
        return 0;
    }
    let anchor_uptime = ANCHOR_UPTIME_US.load(Ordering::Relaxed);
    let elapsed = crate::net::uptime_us().saturating_sub(anchor_uptime);
    anchor_unix.saturating_add(elapsed)
}

/// As [`now_us`], in whole seconds — for `net.rs`'s `NetRuntime::utc_seconds`
/// hook, which wants `Option<u64>` seconds rather than `u64` microseconds and
/// `None` (not `Some(0)`) for "unsynced".
#[must_use]
pub fn utc_seconds() -> Option<u64> {
    if is_synced() {
        Some(now_us() / 1_000_000)
    } else {
        None
    }
}

/// SNTP server. A public pool rather than a fixed IP: `pool.ntp.org` round-
/// robins across many operators' servers, so this does not depend on one
/// server's continued existence the way a hardcoded address would — the same
/// reasoning `MISSING_NTP_SYSCALLS.md` gives for preferring it over routing
/// AWS's link-local time service.
const NTP_HOST: &str = "pool.ntp.org";

/// Generous relative to how fast this actually resolves and round-trips in
/// practice (well under a second on a local/NAT network) — measured in
/// `net::uptime_us`'s own 10 ms-granularity units, so this is "5 real
/// seconds" only approximately, per this module's own header.
const NTP_TIMEOUT_US: u64 = 5_000_000;

/// Best-effort: resolve [`NTP_HOST`], fetch the time once, and record it if
/// it worked. Never fatal — this target booted with no clock at all for
/// every stage before this one, and a network that cannot reach an NTP pool
/// (no route, a captive/offline network, DNS blocked) is not a reason to
/// fail the boot over. Called once from `main.rs`, after DHCP.
pub fn sync_via_sntp() {
    // `sti`, explicitly. Measured 2026-09-05: at this exact point in boot —
    // after every process/spawn/execve/fork self-test, right after
    // `netpoll_selftest` — `RFLAGS.IF` is already `0`. Something in that
    // stretch clears it and nothing before this restores it (`lapic::
    // stop_timer` only masks the LVT entry, `sti`/`cli` was never its job);
    // tracked in `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.30 as a real,
    // separate bug worth finding rather than fixed here. Without this,
    // `lapic::start_timer`'s freshly-armed count never actually interrupts,
    // `lapic::ticks()` never advances, and `bootstrap_over_udp`'s own
    // uptime-based deadline check can never fire — a network that genuinely
    // cannot reach an NTP server hangs the boot at 100% CPU forever instead
    // of giving up, which is exactly what happened before this line existed.
    // SAFETY: unconditionally safe at ring 0; enables maskable interrupts.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }

    // `crate::dns::resolve_a`, not `akuma_net::dns::resolve_host_blocking` —
    // see that module's own header for why: the latter hung indefinitely the
    // first time this feature tried it, on a hostname that resolves in
    // seconds through this target's other, already-proven DNS path.
    let Some(ip) = crate::dns::resolve_a(NTP_HOST, NTP_TIMEOUT_US) else {
        serial::puts("  clock: could not resolve ");
        serial::puts(NTP_HOST);
        serial::puts(" — no wall clock this boot\n");
        return;
    };
    let server = SocketAddrV4::new(ip, akuma_sntp::sntp::NTP_PORT);

    let Some(idx) = akuma_net::socket::alloc_socket(SOCK_DGRAM) else {
        serial::puts("  clock: no socket free for SNTP\n");
        return;
    };

    let mut send = |req: &[u8]| akuma_net::socket::socket_send_udp(idx, req, server).is_ok();
    let mut recv = |buf: &mut [u8]| {
        // Non-blocking: `bootstrap_over_udp` drives its own poll/retry loop
        // and calls this once per iteration, exactly as `BootstrapEffects`
        // documents.
        akuma_net::socket::socket_recv_udp(idx, buf, true)
            .ok()
            .map(|(n, _from)| n)
    };
    let mut poll_network = || {
        akuma_net::smoltcp_net::poll();
    };
    let mut uptime_us = crate::net::uptime_us;
    let mut yield_now = crate::sched::yield_now;

    let mut effects = BootstrapEffects {
        send: &mut send,
        recv: &mut recv,
        poll_network: &mut poll_network,
        uptime_us: &mut uptime_us,
        yield_now: &mut yield_now,
    };

    let result = bootstrap_over_udp(&mut effects, NTP_TIMEOUT_US);
    akuma_net::socket::remove_socket(idx);

    match result {
        Ok(r) => {
            ANCHOR_UNIX_US.store(r.unix_epoch_us.max(1), Ordering::Relaxed);
            ANCHOR_UPTIME_US.store(r.anchor_uptime_us, Ordering::Relaxed);
            serial::puts("  clock: synced via SNTP (");
            serial::puts(NTP_HOST);
            serial::puts(")\n");
        }
        Err(_) => {
            serial::puts("  clock: SNTP round trip failed — no wall clock this boot\n");
        }
    }
}
