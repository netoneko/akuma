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
//! # The pool between the source and every reader
//!
//! [`fill_bytes`] does not hand out the source's bytes. It hands out the output
//! of [`Pool`], a ChaCha20 generator whose key is seeded from the source on
//! first use, mixed with a fresh read of the source on every call, and ratcheted
//! after every call. So:
//!
//! * a source that is weak, biased or lying (the virtio device is untrusted
//!   input) cannot choose what `getrandom` returns once the pool holds 256 bits
//!   from anywhere;
//! * a source that fails *after* seeding does not fail the caller;
//! * a source that fails *before* seeding does: the answer is `Some(false)` and
//!   `getrandom` is `EIO`. There is no weak fallback, because a fallback is a
//!   key an attacker can compute (`docs/archive/SECURITY_AUDIT_2026_11_16.md` C3).
//!
//! A kernel with a second source (`RNDR`, `RDSEED`, a boot-loader seed) feeds it
//! with [`seed`]; anything merely hard to guess (timer samples) goes to
//! [`absorb`], which mixes without counting as a seed.
//!
//! The pool takes no lock: the key is eight atomics updated only by `fetch_xor`,
//! and every call draws a unique stream id from a counter, so it is safe from an
//! interrupt handler and cannot deadlock a core against itself.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::OnceCopy;

// ── ChaCha20 (the original 64-bit counter, 64-bit nonce layout) ─────────────

const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

#[inline(always)]
const fn quarter_round(state: &mut [u32; 16], ia: usize, ib: usize, ic: usize, id: usize) {
    state[ia] = state[ia].wrapping_add(state[ib]);
    state[id] = (state[id] ^ state[ia]).rotate_left(16);
    state[ic] = state[ic].wrapping_add(state[id]);
    state[ib] = (state[ib] ^ state[ic]).rotate_left(12);
    state[ia] = state[ia].wrapping_add(state[ib]);
    state[id] = (state[id] ^ state[ia]).rotate_left(8);
    state[ic] = state[ic].wrapping_add(state[id]);
    state[ib] = (state[ib] ^ state[ic]).rotate_left(7);
}

/// One ChaCha20 block: 16 output words for `(key, counter, nonce)`.
#[must_use]
pub const fn chacha20_block(key: &[u32; 8], counter: u64, nonce: u64) -> [u32; 16] {
    let init = [
        SIGMA[0], SIGMA[1], SIGMA[2], SIGMA[3],
        key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
        counter as u32, (counter >> 32) as u32, nonce as u32, (nonce >> 32) as u32,
    ];
    let mut x = init;
    let mut round = 0;
    while round < 10 {
        quarter_round(&mut x, 0, 4, 8, 12);
        quarter_round(&mut x, 1, 5, 9, 13);
        quarter_round(&mut x, 2, 6, 10, 14);
        quarter_round(&mut x, 3, 7, 11, 15);
        quarter_round(&mut x, 0, 5, 10, 15);
        quarter_round(&mut x, 1, 6, 11, 12);
        quarter_round(&mut x, 2, 7, 8, 13);
        quarter_round(&mut x, 3, 4, 9, 14);
        round += 1;
    }
    let mut i = 0;
    while i < 16 {
        x[i] = x[i].wrapping_add(init[i]);
        i += 1;
    }
    x
}

/// Stream id reserved for [`Pool::absorb`], so a mixing block can never equal
/// an output block whatever the key.
const ABSORB_NONCE: u64 = u64::MAX;

/// The generator behind [`fill_bytes`]. A type rather than loose statics so the
/// host tests can run it without touching the kernel's one instance.
pub struct Pool {
    key: [AtomicU32; 8],
    /// Unique id per `generate` call; the ChaCha20 nonce of that call's stream.
    calls: AtomicU64,
    /// Unique id per absorbed 32-byte chunk.
    absorbs: AtomicU64,
    seeded: AtomicBool,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl Pool {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            key: [const { AtomicU32::new(0) }; 8],
            calls: AtomicU64::new(0),
            absorbs: AtomicU64::new(0),
            seeded: AtomicBool::new(false),
        }
    }

    fn load_key(&self) -> [u32; 8] {
        let mut k = [0u32; 8];
        for (dst, src) in k.iter_mut().zip(&self.key) {
            *dst = src.load(Ordering::Relaxed);
        }
        k
    }

    /// Mix `material` into the key. Does **not** mark the pool seeded: this is
    /// for input that helps but that nobody will vouch for.
    ///
    /// Each 32-byte chunk `m` updates `key ^= ChaCha20(key ^ m)[..8]`, so the
    /// new key depends on every bit of the old key and of `m`, and absorbing
    /// known or hostile material cannot remove entropy already there.
    pub fn absorb(&self, material: &[u8]) {
        for chunk in material.chunks(32) {
            let mut k = self.load_key();
            for (i, bytes) in chunk.chunks(4).enumerate() {
                let mut w = [0u8; 4];
                w[..bytes.len()].copy_from_slice(bytes);
                k[i] ^= u32::from_le_bytes(w);
            }
            let id = self.absorbs.fetch_add(1, Ordering::Relaxed);
            let block = chacha20_block(&k, id, ABSORB_NONCE);
            for (slot, word) in self.key.iter().zip(&block[..8]) {
                slot.fetch_xor(*word, Ordering::Relaxed);
            }
        }
    }

    /// Mix in 32 bytes the caller vouches for as real entropy, and mark the
    /// pool seeded.
    pub fn seed(&self, material: &[u8; 32]) {
        self.absorb(material);
        self.seeded.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_seeded(&self) -> bool {
        self.seeded.load(Ordering::Acquire)
    }

    /// Fill `buf`, mixing in `fresh` (a new read of the hardware source, when
    /// the caller has one). `false`, with `buf` untouched, if the pool was never
    /// seeded.
    ///
    /// One block keyed by the pool gives this call its own key (words 8..16)
    /// and ratchets the pool (words 0..8 are XORed back and never leave), so
    /// the key that produced this output is gone when the call returns.
    #[must_use]
    pub fn generate(&self, fresh: Option<&[u8; 32]>, buf: &mut [u8]) -> bool {
        if !self.is_seeded() {
            return false;
        }
        let mut k = self.load_key();
        if let Some(fresh) = fresh {
            for (word, bytes) in k.iter_mut().zip(fresh.as_chunks::<4>().0) {
                *word ^= u32::from_le_bytes(*bytes);
            }
        }
        let id = self.calls.fetch_add(1, Ordering::Relaxed);
        // `ABSORB_NONCE` is `u64::MAX`; a call counter cannot reach it.
        let first = chacha20_block(&k, 0, id);
        for (slot, word) in self.key.iter().zip(&first[..8]) {
            slot.fetch_xor(*word, Ordering::Relaxed);
        }
        let mut call_key = [0u32; 8];
        call_key.copy_from_slice(&first[8..]);
        for (n, chunk) in buf.chunks_mut(64).enumerate() {
            let block = chacha20_block(&call_key, n as u64, id);
            for (bytes, word) in chunk.chunks_mut(4).zip(&block) {
                let n = bytes.len();
                bytes.copy_from_slice(&word.to_le_bytes()[..n]);
            }
        }
        true
    }
}

/// The kernel's pool.
static POOL: Pool = Pool::new();

/// Mix `material` into the kernel's pool without counting it as a seed.
pub fn absorb(material: &[u8]) {
    POOL.absorb(material);
}

/// Seed the kernel's pool with 32 bytes of real entropy from a source other
/// than the registered hook (`RNDR`, `RDSEED`, a boot-loader seed).
pub fn seed(material: &[u8; 32]) {
    POOL.seed(material);
}

/// Whether [`fill_bytes`] can answer without the hook.
#[must_use]
pub fn is_seeded() -> bool {
    POOL.is_seeded()
}

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

/// Fill `buf` with random bytes from the kernel's pool.
///
/// * `None` — the pool was never seeded and nothing is registered; the caller
///   keeps its own default. `buf` is untouched.
/// * `Some(false)` — the pool is unseeded and the registered source failed.
///   `buf` is untouched; report the error.
/// * `Some(true)` — `buf` is filled.
///
/// The first two are deliberately different: "no source here" is a machine that
/// wants the virtio device, and "the source failed" is an error to report.
/// Collapsing them would make a broken hardware RNG silently fall through to a
/// device that is not present, and `getrandom` would return whatever was
/// already in the caller's buffer.
#[must_use]
pub fn fill_bytes(buf: &mut [u8]) -> Option<bool> {
    let source = RNG_HOOK.get();
    if !POOL.is_seeded() {
        let source = source?;
        let mut seed = [0u8; 32];
        if !source(&mut seed) {
            return Some(false);
        }
        POOL.seed(&seed);
        // Not entropy anyone vouches for, but free, and different every boot.
        POOL.absorb(&crate::clock::uptime_us().to_le_bytes());
    }
    // A fresh read on every call. Its failure is not the caller's: the pool
    // already holds a seed.
    let mut fresh = [0u8; 32];
    let fresh = source.is_some_and(|f| f(&mut fresh)).then_some(&fresh);
    Some(POOL.generate(fresh, buf))
}

// ── the pure halves both x86 kernels used to copy ───────────────────────────
//
// `crates/akuma-tokio-kernel-amd64/src/net.rs` and `crates/akuma-kernel-amd64/src/net.rs` each carried the same SplitMix64
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
    use super::{Pool, chacha20_block};

    /// RFC 8439 §2.3.2. Its 32-bit counter and 96-bit nonce are this layout's
    /// words 12 and 13..16, so the same block is `counter = 1 | 0x0900_0000 << 32`,
    /// `nonce = 0x4a00_0000`.
    #[test]
    fn chacha20_block_matches_rfc_8439() {
        let mut key = [0u32; 8];
        for (i, w) in key.iter_mut().enumerate() {
            let b = (4 * i) as u8;
            *w = u32::from_le_bytes([b, b + 1, b + 2, b + 3]);
        }
        let out = chacha20_block(&key, 1 | (0x0900_0000 << 32), 0x4a00_0000);
        assert_eq!(
            out,
            [
                0xe4e7_f110, 0x1559_3bd1, 0x1fdd_0f50, 0xc471_20a3, 0xc7f4_d1c7, 0x0368_c033,
                0x9aaa_2204, 0x4e6c_d4c3, 0x4664_82d2, 0x09aa_9f07, 0x05d7_c214, 0xa202_8bd9,
                0xd19c_12b5, 0xb94e_16de, 0xe883_d0cb, 0x4e3c_50a2,
            ]
        );
    }

    /// An unseeded pool answers nothing and writes nothing, whatever was absorbed.
    #[test]
    fn pool_refuses_until_seeded() {
        let pool = Pool::new();
        pool.absorb(b"timer samples are not a seed");
        let mut buf = [0x55u8; 40];
        assert!(!pool.generate(None, &mut buf));
        assert_eq!(buf, [0x55u8; 40]);
        pool.seed(&[7; 32]);
        assert!(pool.generate(None, &mut buf));
        assert_ne!(buf, [0x55u8; 40]);
    }

    /// Two calls never return the same bytes, and the pool key moves on each
    /// call (the ratchet), including across a block boundary.
    #[test]
    fn pool_output_never_repeats_and_ratchets() {
        let pool = Pool::new();
        pool.seed(&[1; 32]);
        let key0 = pool.load_key();
        let (mut a, mut b) = ([0u8; 100], [0u8; 100]);
        assert!(pool.generate(None, &mut a));
        let key1 = pool.load_key();
        assert!(pool.generate(None, &mut b));
        assert_ne!(a, b);
        assert_ne!(key0, key1);
        assert_ne!(pool.load_key(), key1);
        assert_ne!(a[..36], a[64..], "blocks of one call differ");
    }

    /// The same seed gives the same stream (it is a function of its inputs),
    /// and one flipped bit of seed, absorbed material or fresh input changes it.
    #[test]
    fn pool_depends_on_every_input() {
        let run = |seed: u8, extra: &[u8], fresh: Option<&[u8; 32]>| {
            let pool = Pool::new();
            pool.seed(&[seed; 32]);
            pool.absorb(extra);
            let mut out = [0u8; 32];
            assert!(pool.generate(fresh, &mut out));
            out
        };
        let base = run(3, b"x", None);
        assert_eq!(base, run(3, b"x", None));
        assert_ne!(base, run(2, b"x", None));
        assert_ne!(base, run(3, b"y", None));
        assert_ne!(base, run(3, b"x", Some(&[0; 31].iter().chain(&[1]).copied().collect::<Vec<_>>().try_into().unwrap())));
    }

    /// Absorbing material an attacker chose cannot return the key to a known
    /// value: the update is keyed by the secret it is updating.
    #[test]
    fn hostile_absorb_does_not_cancel_the_seed() {
        let (p, q) = (Pool::new(), Pool::new());
        p.seed(&[9; 32]);
        q.seed(&[8; 32]);
        for pool in [&p, &q] {
            pool.absorb(&[0; 32]);
            pool.absorb(&[0xFF; 64]);
        }
        assert_ne!(p.load_key(), q.load_key());
        assert_ne!(p.load_key(), [0; 8]);
    }

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
