#![allow(clippy::missing_safety_doc)]

use crate::Registered;

/// Kernel-provided callbacks for the networking crate.
///
/// Registered once during `init()`. All function pointers must remain valid
/// for the lifetime of the kernel (they are plain `fn` pointers, not closures).
#[derive(Clone, Copy)]
pub struct NetRuntime {
    // `virt_to_phys`/`phys_to_virt` used to live here, dispatching this crate's
    // `Hal` impl to the kernel's translators so akuma-net would not need to
    // depend on akuma-exec. That decoupling was already spent — akuma-exec is an
    // unconditional dependency now, for the `PreemptGuard` re-export — and the
    // indirection cost a spinlocked struct read on the per-packet DMA path to
    // reach two identity functions. The `Hal` moved to `akuma-virtio` and calls
    // `akuma_exec::mmu` directly. See that crate's `hal.rs`.
    pub uptime_us: fn() -> u64,
    pub utc_seconds: fn() -> Option<u64>,
    pub yield_now: fn(),
    /// Cooperative wait for a blocking socket loop (`wait_until`) that polls for
    /// data while holding the Big Kernel Lock. Under shared-kernel SMP this MUST
    /// drop the BKL across the wait (kernel wires it to `threading::blocking_relax`,
    /// which `yield_now`s then `idle_halt`s), so a peer core can enter the kernel
    /// and drive the network stack that delivers the data this loop waits on.
    /// Off `smp-shared` the kernel wires it to a plain `yield_now`. Holding the BKL
    /// across the wait freezes every peer core (see docs/runbooks/debug-smp.md).
    pub blocking_relax: fn(),
    /// Park the current thread until `wake_time_us`, or until something calls
    /// its waker — the kernel wires this to `threading::schedule_blocking`.
    ///
    /// The difference from [`blocking_relax`](Self::blocking_relax) is the whole
    /// point: `blocking_relax` leaves the thread READY and merely halts the core,
    /// so nothing can target it and it re-polls when *any* interrupt lands.
    /// `park_until` marks it WAITING, which is what makes a directed wake
    /// possible (`ThreadWaker::wake` CASes WAITING -> READY and IPIs the
    /// thread's last core). Every other blocking path in the kernel — pipes,
    /// fs, msgqueue, epoll — already parks this way; sockets were the outlier.
    ///
    /// The deadline is a BACKSTOP, not the mechanism. A wake that is somehow
    /// missed must cost latency, never a hang.
    pub park_until: fn(u64),
    /// A `Waker` for the current thread, for a waiter registering itself on a
    /// socket before it parks. Wired to `threading::get_waker_for_thread`, whose
    /// handle is generation-tagged so a stale registration cannot wake whatever
    /// later occupies the same thread slot.
    pub current_waker: fn() -> core::task::Waker,
    /// This core's id (`MPIDR` aff0). Used to record which cores have a socket
    /// waiter parked, so the NIC interrupt can wake exactly those instead of
    /// broadcasting to all of them. Always 0 off `smp-shared`.
    pub current_core_id: fn() -> u32,
    pub current_box_id: fn() -> u64,
    pub is_current_interrupted: fn() -> bool,
    pub rng_fill: fn(&mut [u8]),
    /// Returns the current kernel thread id. Used for NETWORK lock holder
    /// tracking (see `smoltcp_net::network_holder_snapshot`). Plain `u32`
    /// because the holder slot is an `AtomicU32` and stays IRQ-friendly.
    pub current_thread_id: fn() -> u32,
    /// End every parked core's `wfi`/`blocking_relax` halt immediately,
    /// instead of leaving it to the next timer tick.
    ///
    /// The kernel wires this to the same cross-core doorbell the virtio-net
    /// RX interrupt rings for external traffic (`src/main.rs`
    /// `ring_netpoll_doorbell`, called by `nic_irq_handler`). Called from
    /// `smoltcp_net::LoopbackRing::push` after a loopback frame is queued —
    /// loopback traffic never touches virtio, so unlike a real packet it has
    /// no interrupt of its own to end a waiter's halt with; without this it
    /// rides the periodic tick, the exact cost the NIC interrupt fix removed
    /// for external traffic (`docs/archive/AKUMA_NET_ISSUES.md` §3.1). See
    /// `docs/archive/LOOPBACK_RING_CONVERSION.md`.
    pub wake_netpoll: fn(),
}

/// RAII guard that disables scheduler preemption (and, under the BKL-drop
/// features, masks local IRQs) for the lifetime of a kernel spinlock critical
/// section.
///
/// **Lifted** to `akuma_primitives::preempt::PreemptGuard` so the VFS BKL-drop path
/// (`no-bkl-vfs`) can share the same primitive without duplication. akuma-net
/// re-exports it here for source compatibility — existing `use
/// crate::runtime::PreemptGuard` sites (`smoltcp_net.rs`, `socket.rs`) keep
/// working unchanged. See `akuma_primitives::preempt::PreemptGuard` for the full
/// rationale (the SMP=4 ABBA hard-wedge this guard prevents, the lift history,
/// and the `no-bkl-network`/`no-bkl-vfs` cfg union that gates the IRQ-masking
/// arm).
pub use crate::PreemptGuard;

/// Was `Spinlock<Option<NetRuntime>>` until 2026-08-13 — a lock taken on
/// **every** read of the callback table, on a crate whose own `NetRuntime` doc
/// comment (above) records moving two fields out because the indirection "cost
/// a spinlocked struct read on the per-packet DMA path". The other two crates
/// with this exact shape (`akuma-exec`, `akuma-ext2`) were already lock-free;
/// `Registered` is that mechanism, shared. Beyond the cost, a spinlock is
/// unsafe here in principle: reading this table from an IRQ handler that
/// interrupted the lock holder self-deadlocks on a single core.
///
/// Registration is single-shot now rather than last-writer-wins. That is not a
/// behaviour change: the two `runtime::register` call sites in `lib.rs` are
/// `#[cfg(feature = "smoltcp")]` and `#[cfg(not(...))]`, so exactly one is
/// compiled, and it is called once from the kernel's boot path.
static RUNTIME: Registered<NetRuntime> =
    Registered::new("akuma-net: NetRuntime not registered — call akuma_net::init() first");

/// Register the kernel runtime callbacks. Must be called before `init()`.
pub fn register(rt: NetRuntime) {
    RUNTIME.register(rt);
}

/// Access the registered runtime. Panics if not yet registered.
#[must_use]
pub fn runtime() -> NetRuntime {
    RUNTIME.require()
}

/// Best-effort runtime accessor that returns `None` if not yet registered.
/// Used by the NETWORK lock holder instrumentation, which may run during
/// boot test code before `register()` has been called.
#[must_use]
pub fn try_runtime() -> Option<NetRuntime> {
    RUNTIME.get()
}
