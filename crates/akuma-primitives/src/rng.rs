//! The kernel's entropy source, as a boot-registered hook.
//!
//! Same shape as [`crate::clock`], and here for the same reason: a leaf crate
//! cannot name a driver, and the driver differs per machine.
//!
//! # Why this exists
//!
//! `akuma-syscalls-glue`'s `getrandom(2)` called `akuma_virtio::rng::fill_bytes`
//! directly. That is the right answer on every machine this kernel had run on
//! until 2026-09-05 — a VMM announces a virtio-rng device and the guest reads
//! it — and it is the wrong answer on the amd64 bare-metal box, which has no
//! virtio at all and takes its entropy from `RDRAND`. So `getrandom` was the
//! one syscall in `akuma_syscalls::fast_path`'s leaf tier that could not be
//! folded into glue for this target: folding it would have handed every ring-3
//! caller `EIO`, including `sshd`'s key exchange.
//!
//! Naming the source rather than the device is also what stops the next
//! machine from being a third copy: a board with an SoC TRNG registers here and
//! nothing above it changes.
//!
//! # The fallback is virtio, and stays virtio
//!
//! [`fill_bytes`] answers `None` when nothing is registered, and glue's
//! `getrandom` then does what it always did. The AArch64 kernel registers
//! nothing and its behaviour is unchanged; this is additive on that side by
//! construction, which is the property that let it land without touching
//! `src/`.
//!
//! # Not a CSPRNG, and not the place to make one
//!
//! This is a plumbing seam. Whether the registered source is cryptographically
//! sound is the registrant's problem and is stated at the registration site —
//! `amd64/src/net.rs`'s `rng_fill` degrades to a counter-based `weak_fill` on a
//! CPU without `RDRAND` and says so.

use crate::OnceCopy;

/// Fill `buf` with random bytes; `false` means the source could not.
///
/// A boolean rather than a `Result` because the only thing a caller can do
/// with the distinction is return `EIO`, and an error enum here would have to
/// be a union of every future source's failure modes.
static RNG_HOOK: OnceCopy<fn(&mut [u8]) -> bool> = OnceCopy::new();

/// Install the entropy source. Idempotent by `OnceCopy`'s contract: a second
/// call is ignored, so a target that registers early cannot be displaced by a
/// later probe.
pub fn set_rng_hook(f: fn(&mut [u8]) -> bool) {
    RNG_HOOK.set(f);
}

/// Whether a source has been registered.
#[must_use]
pub fn is_registered() -> bool {
    RNG_HOOK.is_set()
}

/// Fill `buf` from the registered source.
///
/// * `None` — nothing is registered; the caller keeps its own default.
/// * `Some(false)` — a source is registered and it failed.
///
/// The two are deliberately different: "no source here" is a machine that
/// wants the virtio device, and "the source failed" is an error to report.
/// Collapsing them would make a broken hardware RNG silently fall through to a
/// device that is not present, and `getrandom` would return whatever was
/// already in the caller's buffer.
#[must_use]
pub fn fill_bytes(buf: &mut [u8]) -> Option<bool> {
    RNG_HOOK.get().map(|f| f(buf))
}

#[cfg(test)]
mod tests {
    /// The degradation contract glue's `getrandom` depends on: unregistered is
    /// `None`, not `Some(false)`, so the virtio path is still reached.
    #[test]
    fn unregistered_source_is_none_not_failure() {
        let mut buf = [0u8; 8];
        assert_eq!(super::fill_bytes(&mut buf), None);
        assert!(!super::is_registered());
        assert_eq!(buf, [0u8; 8], "an unregistered source must not touch the buffer");
    }
}
