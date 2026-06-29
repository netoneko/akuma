//! Per-core syscall-forwarding bounce region — the bulk data path for cross-core
//! syscall forwarding (docs/MULTIKERNEL.md §8.1 "how the bytes cross cores").
//!
//! Routing a forwarded syscall is the easy half; moving its **data** is the hard half.
//! A forwarding core's user buffer lives in its own partition and must never be touched
//! by the owner. So the bytes meet in this shared region instead: the forwarding core
//! `copyin`s an inbound buffer here before sending the request, and `copyout`s an
//! outbound result from here after the reply. Each core only ever dereferences its own
//! process memory — this bounce slot is the sole shared byte buffer.
//!
//! No internal synchronization: access is serialized by the request/reply handshake on
//! [`crate::Ring`] (the `ready` Release/Acquire publishes these bytes), so exactly one
//! side touches a given slot at a time. Bytes are [`AtomicU8`] purely so the shared
//! buffer is written/read without forming an `&mut` alias across cores (same discipline
//! as [`crate::ConsoleRing`]); the loads/stores are `Relaxed` because the ring is the
//! ordering edge.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Bytes per core in the forwarding bounce region. Sized to hold a path or a single
/// forwarded `read`/`write` chunk; larger transfers loop (§8.1 "Bulk").
pub const FWD_BOUNCE_CAP: usize = 256;

/// Number of scalar syscall arguments a forwarded call carries (matches the AArch64
/// syscall ABI: x0–x5).
pub const FWD_CALL_ARGS: usize = 6;

/// One core's forwarded-syscall request frame in the shared descriptor — the control half
/// of generic syscall forwarding (§8.1/§10.1).
///
/// Holds the syscall number + its scalar args; any pointer argument's bytes travel
/// separately in the core's [`FwdBounce`] slot. Like the bounce, it carries no internal
/// synchronization — the [`crate::Ring`] handshake (`ready` Release/Acquire) is the
/// publish edge, so the loads/stores are `Relaxed`.
#[repr(C)]
pub struct ForwardCall {
    nr: AtomicU64,
    args: [AtomicU64; FWD_CALL_ARGS],
}

impl Default for ForwardCall {
    fn default() -> Self {
        Self::new()
    }
}

impl ForwardCall {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nr: AtomicU64::new(0),
            args: [const { AtomicU64::new(0) }; FWD_CALL_ARGS],
        }
    }

    /// Publish a syscall number + args (the forwarding core, before pushing the request).
    pub fn set(&self, nr: u64, args: &[u64; FWD_CALL_ARGS]) {
        self.nr.store(nr, Ordering::Relaxed);
        for (slot, &a) in self.args.iter().zip(args.iter()) {
            slot.store(a, Ordering::Relaxed);
        }
    }

    /// Read back the syscall number + args (the owner core, on servicing the request).
    pub fn get(&self) -> (u64, [u64; FWD_CALL_ARGS]) {
        let mut args = [0u64; FWD_CALL_ARGS];
        for (out, slot) in args.iter_mut().zip(self.args.iter()) {
            *out = slot.load(Ordering::Relaxed);
        }
        (self.nr.load(Ordering::Relaxed), args)
    }
}

/// One core's bounce slot in the shared descriptor.
#[repr(C)]
pub struct FwdBounce {
    bytes: [AtomicU8; FWD_BOUNCE_CAP],
}

impl Default for FwdBounce {
    fn default() -> Self {
        Self::new()
    }
}

impl FwdBounce {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [const { AtomicU8::new(0) }; FWD_BOUNCE_CAP],
        }
    }

    /// Copy `src` (clamped to `FWD_BOUNCE_CAP`) into the slot; returns the count written.
    pub fn write(&self, src: &[u8]) -> usize {
        let n = src.len().min(FWD_BOUNCE_CAP);
        for (i, &b) in src.iter().take(n).enumerate() {
            self.bytes[i].store(b, Ordering::Relaxed);
        }
        n
    }

    /// Copy up to `out.len()` (clamped to `FWD_BOUNCE_CAP`) bytes into `out`; returns
    /// the count read.
    pub fn read(&self, out: &mut [u8]) -> usize {
        let n = out.len().min(FWD_BOUNCE_CAP);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.bytes[i].load(Ordering::Relaxed);
        }
        n
    }

    /// Apply `f` to the first `len` bytes in place (clamped). Used by the owner core to
    /// produce a forwarded syscall's result from the request payload.
    pub fn map_in_place(&self, len: usize, mut f: impl FnMut(u8) -> u8) {
        for i in 0..len.min(FWD_BOUNCE_CAP) {
            let b = self.bytes[i].load(Ordering::Relaxed);
            self.bytes[i].store(f(b), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_round_trip() {
        let b = FwdBounce::new();
        let src = [1u8, 2, 3, 4, 5];
        assert_eq!(b.write(&src), 5);
        let mut out = [0u8; 5];
        assert_eq!(b.read(&mut out), 5);
        assert_eq!(out, src);
    }

    #[test]
    fn write_clamps_to_cap() {
        let b = FwdBounce::new();
        let big = [0xABu8; FWD_BOUNCE_CAP + 64];
        assert_eq!(b.write(&big), FWD_BOUNCE_CAP);
    }

    #[test]
    fn map_in_place_transforms_prefix_only() {
        let b = FwdBounce::new();
        b.write(&[10, 20, 30, 40]);
        b.map_in_place(2, |x| x + 1);
        let mut out = [0u8; 4];
        b.read(&mut out);
        assert_eq!(out, [11, 21, 30, 40]); // only first 2 transformed
    }
}
