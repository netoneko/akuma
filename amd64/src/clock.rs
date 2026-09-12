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
//! # What this file is, after C3
//!
//! **Only the SNTP client.** The clock it used to *keep* — a private
//! `(anchor_unix, anchor_uptime)` pair, and the `clock_gettime`/`clock_settime`
//! /`adjtimex` arms in `usermode.rs` that read it — is gone (2026-09-12,
//! `docs/archive/AKUMA_AMD64_C3_CLOCK.md`). The anchor is
//! `akuma_primitives::clock`'s, which is what `akuma-syscalls-time` reads, so
//! there is one wall clock on this target instead of two that disagreed. What
//! is left here is the part that is genuinely platform-specific: this machine
//! has no RTC, so the only way it can learn the date is to ask the network.
//!
//! It is still not a *disciplined* clock. It anchors once and then adds
//! elapsed [`net::uptime_us`] — the "coarse but honest" 10 ms-granularity
//! LAPIC tick count `net.rs` already uses for network timeouts — with no
//! frequency discipline; `adjtimex` steps rather than slews, a divergence
//! pinned in `akuma-syscalls-time` for both kernels. That is a deliberate
//! scope cut: the requirement this exists for — TLS certificate date
//! validation — needs the answer right to within the *year*, arguably the
//! *day*, never the millisecond.
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
//! uptime/yield hooks `net.rs` already has. Where the result is *stored* is
//! shared — see above.

use akuma_net::socket::socket_const::SOCK_DGRAM;
use akuma_net::socket::SocketAddrV4;
use akuma_primitives::clock as wall;
use akuma_sntp::boot::{bootstrap_over_udp, BootstrapEffects};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::serial;

// ---------------------------------------------------------------------------
// The anchor is `akuma_primitives::clock`'s, not this file's (C3, 2026-09-12).
//
// It used to be a private `(ANCHOR_UNIX_US, ANCHOR_UPTIME_US)` pair right
// here, and that pair was the whole C3 problem in one object: this file wrote
// it, `fd.rs`'s and `boot.rs`'s `utc_time_us` hooks read it, and
// `akuma-syscalls-time` — which serves `clock_gettime`/`clock_settime`/
// `adjtimex` for the *other* kernel — read a different one. So folding the
// time syscalls in would have given ring 3 a clock nothing on this target
// ever set. One static now, in the crate both kernels already share, and
// there is nothing left here to disagree with.
// ---------------------------------------------------------------------------

/// Has the wall clock ever been set — by [`sync_via_sntp`] here, or by
/// `clock_settime`/`settimeofday`/`adjtimex` from ring 3?
#[must_use]
pub fn is_synced() -> bool {
    wall::is_utc_set()
}

/// Current wall-clock time, in microseconds since the Unix epoch. `0` if
/// never synced — matching `fs::no_clock`'s "0 is the honest answer, not a
/// guess dressed up as one" stance elsewhere on this target, rather than a
/// plausible-looking but wrong value.
#[must_use]
pub fn now_us() -> u64 {
    wall::utc_time_us(crate::net::uptime_us()).unwrap_or(0)
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

/// Set the wall clock, in microseconds since the Unix epoch.
///
/// The write half of [`now_us`], for the x86-only `settimeofday`/`time` ABI
/// spellings that have no asm-generic number and therefore stay in
/// `usermode.rs` as shims. `clock_settime` and `adjtimex` do not come through
/// here any more — they are glue's since C3 — but they write the same static,
/// so whichever path runs last wins and nothing has to know which.
///
/// `0` is still rejected. It is no longer a sentinel — the shared anchor has
/// its own, so 1970-01-01T00:00:00Z is representable — but the other half of
/// the reason stands on its own: a caller asking for the epoch is reporting a
/// failure as a time, and storing it would make [`is_synced`] answer true for
/// a machine that does not know what year it is.
pub fn set_unix_us(us: u64) {
    if us == 0 {
        return;
    }
    // Uptime first, then anchor to it: read the other way round, everything
    // between the two reads is silently added to the wall clock.
    let uptime = crate::net::uptime_us();
    wall::set_utc_time_us(us, uptime);
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

/// Shorter budget for the [`sync_tick`] retries, which run *inside the netpoll
/// daemon loop* — a 5 s stall there is 5 s of no `smoltcp_net::poll()`, which an
/// ssh session in the middle of a transfer would feel. The boot one-shot has no
/// such neighbour and keeps the longer budget.
const RETRY_TIMEOUT_US: u64 = 2_500_000;

/// How long [`sync_tick`] waits between attempts while the clock is still
/// unset. Long enough that a genuinely offline machine is not burning a UDP
/// socket and a DNS query every lap; short enough that a machine whose DHCP
/// lease just arrived gets a clock within a few tens of seconds.
const RETRY_INTERVAL_US: u64 = 15_000_000;

/// The result of one SNTP attempt, recorded in [`LAST_OUTCOME`] so it can be
/// read back — by the `netprobe` line, and by anyone asking the machine over
/// ssh why `date` still says 1970.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Not attempted yet.
    Untried = 0,
    /// Anchored — the clock is set.
    Ok = 1,
    /// [`NTP_HOST`] did not resolve (DNS server unreachable, or no route).
    DnsFailed = 2,
    /// No UDP socket free.
    NoSocket = 3,
    /// Resolved, but no valid SNTP reply came back inside the timeout.
    NoReply = 4,
}

static LAST_OUTCOME: AtomicU64 = AtomicU64::new(0);
static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
/// [`net::uptime_us`] before which [`sync_tick`] does nothing.
static NEXT_RETRY_US: AtomicU64 = AtomicU64::new(0);

/// The last SNTP attempt's outcome as `(outcome, attempt_count)` — for the
/// `netprobe` line and for a future "why no clock" syscall.
#[must_use]
pub fn sync_status() -> (SyncOutcome, u64) {
    let o = match LAST_OUTCOME.load(Ordering::Relaxed) {
        1 => SyncOutcome::Ok,
        2 => SyncOutcome::DnsFailed,
        3 => SyncOutcome::NoSocket,
        4 => SyncOutcome::NoReply,
        _ => SyncOutcome::Untried,
    };
    (o, ATTEMPTS.load(Ordering::Relaxed))
}

/// Try SNTP once if the clock is not already set. Cheap to call every netpoll
/// lap: it returns immediately when synced or when the retry interval has not
/// elapsed. This is what makes the clock **set itself** even when the boot-time
/// one-shot ([`sync_via_sntp`]) ran before DHCP finished or lost its datagram —
/// "synced once at boot or never" was the old contract and it left the HP box
/// at epoch 0 every time the lease was slow.
pub fn sync_tick() {
    if is_synced() {
        return;
    }
    if crate::net::uptime_us() < NEXT_RETRY_US.load(Ordering::Relaxed) {
        return;
    }
    // The interval is armed by `report_outcome`, i.e. *after* the attempt and
    // from whichever path made it — see its doc. Arming it here instead is
    // what let the daemon's first lap re-run a boot attempt that had just
    // failed.
    let outcome = attempt_sntp(RETRY_TIMEOUT_US);
    report_outcome(outcome, "retry");
}

/// Best-effort: resolve [`NTP_HOST`], fetch the time once, and record it if
/// it worked. Never fatal — this target booted with no clock at all for
/// every stage before this one, and a network that cannot reach an NTP pool
/// (no route, a captive/offline network, DNS blocked) is not a reason to
/// fail the boot over. Called once from `main.rs`, after DHCP; [`sync_tick`]
/// keeps trying afterwards if this did not land.
pub fn sync_via_sntp() {
    let outcome = attempt_sntp(NTP_TIMEOUT_US);
    report_outcome(outcome, "boot");
}

/// Store the outcome, **arm the next retry**, and print one line. `ctx` is
/// `"boot"` or `"retry"` so a console reader can see which path spoke.
///
/// # The rate limit belongs to the attempt, not to `sync_tick`
///
/// [`RETRY_INTERVAL_US`] used to be armed inside [`sync_tick`], so
/// [`sync_via_sntp`] — the boot one-shot — did not participate in it. On a
/// machine where the boot attempt *fails*, that left `NEXT_RETRY_US` at `0`,
/// and the netpoll daemon's **very first lap** re-attempted the thing that had
/// just failed microseconds earlier, blocking that lap for the whole
/// [`RETRY_TIMEOUT_US`] budget of 2.5 s.
///
/// That is longer than the 2 s `net::netpoll_spawn_selftest` gives the daemon
/// to reach 100 laps, so the check **could not pass on any machine whose boot
/// SNTP failed**: it reported `netpoll laps 0` and read as a starved daemon.
/// Measured on the OVMF/GRUB rig 2026-09-07, where the e1000 is undriven so
/// DNS cannot work: `entered 1, drains completed 1, ticks completed 0` — the
/// daemon had been scheduled, had polled the network once, and was sitting in
/// here. Bare metal showed the same failure as a coin flip, because there it
/// depends on whether the boot-time SNTP happened to land.
///
/// Arming here makes every attempt — boot or retry — set the clock for the
/// next one, which is what the interval always meant.
fn report_outcome(outcome: SyncOutcome, ctx: &str) {
    LAST_OUTCOME.store(outcome as u64, Ordering::Relaxed);
    ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    if outcome != SyncOutcome::Ok {
        NEXT_RETRY_US.store(
            crate::net::uptime_us().saturating_add(RETRY_INTERVAL_US),
            Ordering::Relaxed,
        );
    }
    match outcome {
        SyncOutcome::Ok => {
            serial::puts("  clock: synced via SNTP (");
            serial::puts(NTP_HOST);
            serial::puts(", ");
            serial::puts(ctx);
            serial::puts(")\n");
        }
        SyncOutcome::DnsFailed => {
            serial::puts("  clock: ");
            serial::puts(ctx);
            serial::puts(": could not resolve ");
            serial::puts(NTP_HOST);
            serial::puts(" via ");
            for (i, o) in akuma_net::smoltcp_net::static_ipv4().dns.iter().enumerate() {
                if i > 0 {
                    serial::puts(".");
                }
                serial::put_dec(u64::from(*o));
            }
            serial::puts("\n");
        }
        SyncOutcome::NoSocket => {
            serial::puts("  clock: ");
            serial::puts(ctx);
            serial::puts(": no socket free for SNTP\n");
        }
        SyncOutcome::NoReply => {
            serial::puts("  clock: ");
            serial::puts(ctx);
            serial::puts(": SNTP round trip got no reply\n");
        }
        SyncOutcome::Untried => {}
    }
}

/// One SNTP attempt. Resolves, opens a UDP socket, runs the shared
/// send/poll/receive/timeout loop, and anchors the clock on success. Prints
/// nothing — [`report_outcome`] owns the console line — so the boot and the
/// retry paths format their own context.
fn attempt_sntp(timeout_us: u64) -> SyncOutcome {
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
    let Some(ip) = crate::dns::resolve_a(NTP_HOST, timeout_us) else {
        return SyncOutcome::DnsFailed;
    };
    let server = SocketAddrV4::new(ip, akuma_sntp::sntp::NTP_PORT);

    let Some(idx) = akuma_net::socket::alloc_socket(SOCK_DGRAM) else {
        return SyncOutcome::NoSocket;
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

    let result = bootstrap_over_udp(&mut effects, timeout_us);
    akuma_net::socket::remove_socket(idx);

    match result {
        Ok(r) => {
            // `anchor_uptime_us` is the uptime SNTP sampled at *packet
            // receipt*, not now — anchoring at now would add the parse to the
            // clock. `.max(1)` is gone with the old `0`-means-unset sentinel:
            // the shared anchor has its own, so a reply really claiming the
            // epoch is now storable and `is_utc_set` still answers true.
            wall::set_utc_time_us(r.unix_epoch_us, r.anchor_uptime_us);
            SyncOutcome::Ok
        }
        Err(_) => SyncOutcome::NoReply,
    }
}
