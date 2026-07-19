#![allow(clippy::missing_safety_doc)]

use spinning_top::Spinlock;

/// Kernel-provided callbacks for the networking crate.
///
/// Registered once during `init()`. All function pointers must remain valid
/// for the lifetime of the kernel (they are plain `fn` pointers, not closures).
#[derive(Clone, Copy)]
pub struct NetRuntime {
    pub virt_to_phys: fn(usize) -> usize,
    pub phys_to_virt: fn(usize) -> *mut u8,
    pub uptime_us: fn() -> u64,
    pub utc_seconds: fn() -> Option<u64>,
    pub yield_now: fn(),
    pub current_box_id: fn() -> u64,
    pub is_current_interrupted: fn() -> bool,
    pub rng_fill: fn(&mut [u8]),
    /// Returns the current kernel thread id. Used for NETWORK lock holder
    /// tracking (see `smoltcp_net::network_holder_snapshot`). Plain `u32`
    /// because the holder slot is an `AtomicU32` and stays IRQ-friendly.
    pub current_thread_id: fn() -> u32,
    /// Disable scheduler preemption for the current thread. Paired with
    /// [`Self::enable_preemption`]. Used by [`PreemptGuard`] to keep the base
    /// `NETWORK` / `SOCKET_TABLE` spinlocks from ever being held across a
    /// context switch (see `PreemptGuard` for why that matters under SMP).
    pub disable_preemption: fn(),
    /// Re-enable scheduler preemption. Must balance one `disable_preemption`.
    pub enable_preemption: fn(),
}

/// RAII guard that disables scheduler preemption for the lifetime of a kernel
/// spinlock critical section.
///
/// Under real shared-kernel SMP (`smp-shared`) the global `NETWORK` /
/// `SOCKET_TABLE` spinlocks must never be held across a context switch. The Big
/// Kernel Lock is released on an EL1→EL0 return, so a thread descheduled while
/// holding one of these inner spinlocks strands the lock — and any *other* core
/// that then spins on it does so **while holding the BKL**, wedging every core
/// (the BKL owner can never be rescheduled to release the inner lock). This is
/// exactly the httpd `socket()`/`bind()` deadlock seen booting the devbox-smoltcp
/// image under `SMP>=2`.
///
/// Disabling preemption for the hold keeps the holder on-core until it releases,
/// so under the BKL the inner lock is never cross-core contended (the BKL already
/// provides the mutual exclusion; this guard only prevents the strand). The
/// critical sections it wraps must never voluntarily yield/block — same rule the
/// native stack already followed on single-core.
///
/// A zero-cost no-op on every non-`smp-shared` build: `new`/`drop` compile to
/// nothing, so the hot path is byte-for-byte unchanged.
#[must_use]
pub struct PreemptGuard {
    /// Whether `new()` actually disabled preemption (runtime was registered).
    /// Present only under `smp-shared`; other builds carry no state.
    #[cfg(feature = "smp-shared")]
    active: bool,
}

impl PreemptGuard {
    /// Disable preemption (under `smp-shared`) until the returned guard drops.
    #[inline]
    pub fn new() -> Self {
        #[cfg(feature = "smp-shared")]
        {
            // Best-effort: the runtime is always registered before any net path
            // runs, but stay panic-free during early boot / host tests.
            if let Some(rt) = try_runtime() {
                (rt.disable_preemption)();
                return Self { active: true };
            }
            Self { active: false }
        }
        #[cfg(not(feature = "smp-shared"))]
        Self {}
    }
}

impl Default for PreemptGuard {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "smp-shared")]
        if self.active
            && let Some(rt) = try_runtime()
        {
            (rt.enable_preemption)();
        }
    }
}

static RUNTIME: Spinlock<Option<NetRuntime>> = Spinlock::new(None);

/// Register the kernel runtime callbacks. Must be called before `init()`.
pub fn register(rt: NetRuntime) {
    *RUNTIME.lock() = Some(rt);
}

/// Access the registered runtime. Panics if not yet registered.
#[must_use]
pub fn runtime() -> NetRuntime {
    RUNTIME
        .lock()
        .expect("akuma-net: NetRuntime not registered — call akuma_net::init() first")
}

/// Best-effort runtime accessor that returns `None` if not yet registered.
/// Used by the NETWORK lock holder instrumentation, which may run during
/// boot test code before `register()` has been called.
#[must_use]
pub fn try_runtime() -> Option<NetRuntime> {
    *RUNTIME.lock()
}
