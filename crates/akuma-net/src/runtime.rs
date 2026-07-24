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
    /// Cooperative wait for a blocking socket loop (`wait_until`) that polls for
    /// data while holding the Big Kernel Lock. Under shared-kernel SMP this MUST
    /// drop the BKL across the wait (kernel wires it to `threading::blocking_relax`,
    /// which `yield_now`s then `idle_halt`s), so a peer core can enter the kernel
    /// and drive the network stack that delivers the data this loop waits on.
    /// Off `smp-shared` the kernel wires it to a plain `yield_now`. Holding the BKL
    /// across the wait freezes every peer core (see docs/runbooks/debug-smp.md).
    pub blocking_relax: fn(),
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
///
/// **`no-bkl-network` addition — local IRQs are masked for the hold too.** With
/// the Big Kernel Lock dropped around net syscalls, this core can be inside a
/// `NETWORK`/`SOCKET_TABLE` critical section *without* owning the BKL. A nested
/// IRQ then runs `enter_kernel()` (exceptions.rs), which hard-spins (IRQs masked)
/// until the BKL frees — while THIS core still holds the inner spinlock. If the
/// current BKL owner is meanwhile spinning on that same inner lock (the async-main
/// poller does exactly that on `NETWORK`, near-constantly), the two cores deadlock
/// AB-BA and every other core piles into the BKL wait: the SMP=4 hard wedge
/// (`[BKL] stuck`, owner frozen nonzero, guest timer starved). Masking IRQs for
/// the (short) hold makes the window nest-free, so a core can never be caught
/// "holding an inner lock, waiting for the BKL". Plain `smp-shared` builds don't
/// need it — there EL1 always holds the BKL, so the nested `enter_kernel` is the
/// idempotent owner fast path.
#[must_use]
pub struct PreemptGuard {
    /// Whether `new()` actually disabled preemption (runtime was registered).
    /// Present only under `smp-shared`; other builds carry no state.
    #[cfg(feature = "smp-shared")]
    active: bool,
    /// Saved DAIF to restore on drop (no-bkl-network builds only).
    #[cfg(all(feature = "smp-shared", feature = "no-bkl-network"))]
    saved_daif: u64,
}

/// Mask local IRQs and return the prior DAIF.
///
/// Bare-metal AArch64 only; host tests get a no-op shim. Mirrors
/// `akuma_exec::sync::irq_save_mask`.
#[cfg(all(feature = "smp-shared", feature = "no-bkl-network", target_arch = "aarch64", target_os = "none"))]
#[inline]
fn irq_save_mask() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF and setting the IRQ mask bit have no memory effects.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
        core::arch::asm!("msr daifset, #0x2", options(nomem, nostack));
    }
    daif
}

/// Restore DAIF saved by [`irq_save_mask`].
#[cfg(all(feature = "smp-shared", feature = "no-bkl-network", target_arch = "aarch64", target_os = "none"))]
#[inline]
fn irq_restore(daif: u64) {
    // SAFETY: restoring the previously-saved DAIF; no memory effects.
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
}

#[cfg(all(feature = "smp-shared", feature = "no-bkl-network", not(all(target_arch = "aarch64", target_os = "none"))))]
#[inline]
fn irq_save_mask() -> u64 {
    0
}

#[cfg(all(feature = "smp-shared", feature = "no-bkl-network", not(all(target_arch = "aarch64", target_os = "none"))))]
#[inline]
fn irq_restore(_daif: u64) {}

impl PreemptGuard {
    /// Disable preemption (under `smp-shared`) until the returned guard drops.
    #[inline]
    pub fn new() -> Self {
        #[cfg(feature = "smp-shared")]
        {
            // Best-effort: the runtime is always registered before any net path
            // runs, but stay panic-free during early boot / host tests.
            let active = if let Some(rt) = try_runtime() {
                (rt.disable_preemption)();
                true
            } else {
                false
            };
            // Mask IRQs AFTER disabling preemption so drop's reverse order
            // re-enables preemption only once IRQs are live again.
            #[cfg(feature = "no-bkl-network")]
            return Self { active, saved_daif: irq_save_mask() };
            #[cfg(not(feature = "no-bkl-network"))]
            return Self { active };
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
        #[cfg(all(feature = "smp-shared", feature = "no-bkl-network"))]
        irq_restore(self.saved_daif);
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
