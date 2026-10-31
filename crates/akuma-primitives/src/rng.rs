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

// ── the pure halves both x86 kernels used to copy ───────────────────────────
//
// `taoxichuan/src/net.rs` and `amd64/src/net.rs` each carried the same SplitMix64
// byte mixer and the same "fill eight bytes at a time from a hardware word
// source" loop. The hardware read (`rdrand` is `unsafe` asm) and the policy for
// a failed read stay with each kernel; the algebra is here, where it can be
// host-tested.

/// Fill `buf` from a SplitMix64 stream whose state starts at `state`.
///
/// **Not cryptographic.** Enough to keep TCP initial sequence numbers from
/// repeating every boot, nowhere near enough for key exchange. The caller
/// supplies the seed (a counter mixed with the TSC); the stream is a pure
/// function of it.
pub fn splitmix_fill(mut state: u64, buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let n = chunk.len();
        chunk.copy_from_slice(&z.to_le_bytes()[..n]);
    }
}

/// Fill `buf` eight bytes at a time from `next`, a hardware word source.
///
/// `Ok(())` when every byte came from `next`. `Err(filled)` when the source
/// failed (`None`) after `filled` bytes: `buf[..filled]` is real, the rest is
/// untouched - what to do about that tail (fall back, or fail the caller) is the
/// kernel's policy, not this crate's.
pub fn fill_from_words(buf: &mut [u8], mut next: impl FnMut() -> Option<u64>) -> Result<(), usize> {
    let mut i = 0;
    while i < buf.len() {
        let Some(w) = next() else {
            return Err(i);
        };
        let n = (buf.len() - i).min(8);
        buf[i..i + n].copy_from_slice(&w.to_le_bytes()[..n]);
        i += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The mixer is a pure function of the seed (the reason it can be tested),
    /// and different seeds give different streams.
    #[test]
    fn splitmix_is_deterministic_and_seed_sensitive() {
        let (mut a, mut b, mut c) = ([0u8; 21], [0u8; 21], [0u8; 21]);
        super::splitmix_fill(7, &mut a);
        super::splitmix_fill(7, &mut b);
        super::splitmix_fill(8, &mut c);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.iter().any(|&x| x != 0));
    }

    /// The first output word of SplitMix64 from state 0 (the published
    /// reference value), pinning the constants.
    #[test]
    fn splitmix_matches_the_reference_first_word() {
        let mut buf = [0u8; 8];
        super::splitmix_fill(0, &mut buf);
        assert_eq!(u64::from_le_bytes(buf), 0xE220_A839_7B1D_CDAF);
    }

    /// A short tail takes the low bytes of one more word; a failing source
    /// reports how many bytes were real.
    #[test]
    fn word_source_fills_tail_and_reports_failure() {
        let mut ok = [0u8; 11];
        let mut n = 0u64;
        let r = super::fill_from_words(&mut ok, || {
            n += 1;
            Some(0x0807_0605_0403_0201 * n)
        });
        assert_eq!(r, Ok(()));
        assert_eq!(&ok[..8], &0x0807_0605_0403_0201u64.to_le_bytes());
        assert_eq!(&ok[8..], &(0x0807_0605_0403_0201u64 * 2).to_le_bytes()[..3]);

        let mut bad = [0xAAu8; 20];
        let mut calls = 0;
        let r = super::fill_from_words(&mut bad, || {
            calls += 1;
            (calls <= 2).then_some(1)
        });
        assert_eq!(r, Err(16));
        assert_eq!(&bad[16..], &[0xAA; 4]);
    }

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
