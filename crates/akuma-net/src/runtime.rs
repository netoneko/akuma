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
