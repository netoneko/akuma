//! The socket layer's machine table and its pump, shared by both machines
//! (B21 A5.9; moved from the amd64 half's `net.rs`).
//!
//! `akuma-net` (smoltcp and the AF_INET socket table) over `akuma-net-nic` (the
//! virtio-net device) needs twelve function pointers ([`NetRuntime`]) and
//! somebody to pump the NIC. Neither machine takes the NIC's interrupt: frames
//! arrive only when [`netpoll`] runs, from the tick, the idle thread, and every
//! lap of a socket wait. A machine supplies its clock, one bounded idle moment
//! and its entropy ([`NetMachine`]).

use akuma_net::runtime::NetRuntime;
use akuma_primitives::safe_print;
use core::sync::atomic::{AtomicBool, Ordering};

/// What the socket layer asks of the machine.
pub trait NetMachine: 'static {
    /// Microseconds since boot: smoltcp's timers (retransmit, DHCP, ARP).
    fn uptime_us() -> u64;
    /// One bounded idle moment between re-checks.
    fn wait_lap();
    /// Fill `buf` with the best entropy the machine has.
    fn rng_fill(buf: &mut [u8]);
    /// Whether the caller is a task the scheduler can park: a user thread inside a syscall, not the boot
    /// task, the idle thread or an interrupt. A wait lap that can park does, so the thread that would
    /// satisfy the wait (the peer end of a loopback connection, a sleeper whose deadline passed) can run;
    /// a lap that cannot park halts. Defaults to `false`: the halting lap, as before.
    #[must_use]
    fn can_park() -> bool {
        false
    }
}

struct Wait<M>(core::marker::PhantomData<M>);

impl<M: NetMachine> akuma_net::wait_hooks::Machine for Wait<M> {
    fn now() -> u64 { M::uptime_us() }
    fn yield_thread() { M::wait_lap(); }
    fn park(deadline_us: u64) { akuma_threading::schedule_blocking(deadline_us); }
    fn halt() { M::wait_lap(); }
    fn deadline() -> akuma_net::wait_hooks::Deadline { akuma_net::wait_hooks::Deadline::Pump }
    fn pump() { netpoll::<M>(); }
    fn relax_laps() -> usize { 64 }
    fn can_park() -> bool { M::can_park() }
}

/// The stack is up; until then [`netpoll`] is a no-op (a tick can come first).
static NET_UP: AtomicBool = AtomicBool::new(false);
/// One lap at a time: the tick calls [`netpoll`] from interrupt context while a
/// task-context pump may be inside it.
static IN_LAP: AtomicBool = AtomicBool::new(false);

/// `net_init` brought the stack up on a NIC (with or without a DHCP answer).
#[must_use]
pub fn is_up() -> bool {
    NET_UP.load(Ordering::Acquire)
}

/// One lap of the stack: drain the NIC into smoltcp, run its timers.
pub fn netpoll<M: NetMachine>() {
    if !NET_UP.load(Ordering::Acquire) || IN_LAP.swap(true, Ordering::Acquire) {
        return;
    }
    #[cfg(feature = "trace-err")]
    let t0 = M::uptime_us();
    let _ = akuma_net::smoltcp_net::poll();
    #[cfg(feature = "trace-err")]
    lap_note::<M>(M::uptime_us().saturating_sub(t0));
    IN_LAP.store(false, Ordering::Release);
}

/// Trace builds: how long the stack's laps take. A lap runs with the kernel
/// lock held, so a slow one stalls every core that wants it
/// (`docs/archive/TAOXICHUAN_J4_BKL_CONVOY.md`). One line a second when
/// anything ran: `laps`, `slow` (over 2 ms), `max_us`, `sum_us`.
#[cfg(feature = "trace-err")]
fn lap_note<M: NetMachine>(dur_us: u64) {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LAPS: AtomicU64 = AtomicU64::new(0);
    static SLOW: AtomicU64 = AtomicU64::new(0);
    static MAX: AtomicU64 = AtomicU64::new(0);
    static SUM: AtomicU64 = AtomicU64::new(0);
    static NEXT: AtomicU64 = AtomicU64::new(0);
    LAPS.fetch_add(1, Relaxed);
    SUM.fetch_add(dur_us, Relaxed);
    if dur_us > 2_000 {
        SLOW.fetch_add(1, Relaxed);
    }
    MAX.fetch_max(dur_us, Relaxed);
    let now = M::uptime_us();
    let next = NEXT.load(Relaxed);
    if now >= next && NEXT.compare_exchange(next, now + 1_000_000, Relaxed, Relaxed).is_ok() {
        safe_print!(
            128,
            "[netlap] laps={} slow={} max_us={} sum_us={}\n",
            LAPS.swap(0, Relaxed),
            SLOW.swap(0, Relaxed),
            MAX.swap(0, Relaxed),
            SUM.swap(0, Relaxed)
        );
    }
}

/// A waker that does nothing: the wait loops poll, nothing is parked for one.
fn noop_waker() -> core::task::Waker {
    core::task::Waker::noop().clone()
}

/// The table, each hook collapsed to what a kernel with no NIC interrupt can
/// honestly answer.
#[must_use]
pub fn runtime<M: NetMachine>() -> NetRuntime {
    fn noop() {}
    NetRuntime {
        uptime_us: M::uptime_us,
        utc_seconds: || None, // nothing in akuma-net reads it
        yield_now: akuma_net::wait_hooks::yield_now::<Wait<M>>,
        blocking_relax: akuma_net::wait_hooks::blocking_relax::<Wait<M>>,
        park_until: akuma_net::wait_hooks::park_until::<Wait<M>>,
        current_waker: noop_waker,
        current_core_id: || 0,
        #[cfg(not(feature = "containers"))]
        current_box_id: || 0,
        #[cfg(feature = "containers")]
        current_box_id: || akuma_exec::process::current_process_shared().map_or(0, |p| p.box_id),
        is_current_interrupted: || false,
        rng_fill: M::rng_fill,
        current_thread_id: || 0,
        // No parked core to ring: loopback frames ride the next poll lap.
        wake_netpoll: noop,
    }
}

/// The socket layer with no NIC: `127.0.0.1` works, nothing else is reachable.
pub fn loopback_init<M: NetMachine>() {
    if akuma_net::init_loopback_only(runtime::<M>()).is_err() {
        safe_print!(64, "  net: loopback init failed\n");
        return;
    }
    safe_print!(64, "  net: loopback only\n");
}

/// The stack on the first virtio-net device, then a bounded DHCP wait.
///
/// The wait pumps the NIC, so the guest has its address before init asks for
/// a port. `false` when there is no device or DHCP went unanswered (the stack stays up,
/// unaddressed).
pub fn net_init<M: NetMachine>() -> bool {
    if akuma_net::init(runtime::<M>(), true).is_err() {
        safe_print!(96, "  net: stack init failed -- no virtio-net device\n");
        return false;
    }
    NET_UP.store(true, Ordering::Release);
    safe_print!(96, "  net: smoltcp stack up, DHCP requested\n");
    let deadline = M::uptime_us() + 10_000_000;
    while M::uptime_us() < deadline {
        netpoll::<M>();
        if akuma_net::smoltcp_net::is_dhcp_configured() {
            let ip = akuma_net::smoltcp_net::static_ipv4().addr;
            safe_print!(96, "  net: DHCP configured, {}.{}.{}.{}\n", ip[0], ip[1], ip[2], ip[3]);
            return true;
        }
        M::wait_lap();
    }
    safe_print!(96, "  net: DHCP not answered in 10 s -- continuing unaddressed\n");
    false
}
