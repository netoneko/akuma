#![no_std]
#![forbid(unsafe_code)]
//! Ed25519 verification for the kernel, and nothing else: the kernel verifies, it never signs and holds no secret.
//!
//! One function, [`verify`], over `ed25519-dalek`'s `verify_strict`. **Strict** matters for a signature that gates execution: it
//! rejects a small-order public key and a non-canonical `R`/`S`, so one signed file has one valid signature (the lax check accepts
//! malleated variants). The message is the bytes themselves (pure Ed25519, RFC 8032 §5.1), so a caller with the whole file in memory
//! needs no separate hash step and no prehash variant.
//!
//! The crate exists so the kernel's use of the primitive is one reviewable door, with the RFC's own vectors pinned beside it.

use ed25519_dalek::{Signature, VerifyingKey};

/// A public key, 32 bytes (RFC 8032 §5.1.5).
pub const PUBLIC_KEY_LEN: usize = 32;
/// A signature, 64 bytes: `R || S`.
pub const SIGNATURE_LEN: usize = 64;

/// Whether `signature` is a valid Ed25519 signature of `message` under `public_key`. Any malformed key or signature is `false`.
#[must_use]
pub fn verify(public_key: &[u8; PUBLIC_KEY_LEN], message: &[u8], signature: &[u8; SIGNATURE_LEN]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    key.verify_strict(message, &Signature::from_bytes(signature)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        let mut out = [0u8; N];
        assert_eq!(s.len(), N * 2);
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    // RFC 8032 §7.1, TEST 1 (empty message) and TEST 2 (one byte).
    const PK1: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const SIG1: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";
    const PK2: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
    const SIG2: &str = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00";

    #[test]
    fn the_rfc_vectors_verify() {
        assert!(verify(&hex(PK1), b"", &hex(SIG1)));
        assert!(verify(&hex(PK2), &[0x72], &hex(SIG2)));
    }

    #[test]
    fn a_changed_message_key_or_signature_does_not() {
        let (pk1, sig1, pk2, sig2) = (hex(PK1), hex(SIG1), hex(PK2), hex(SIG2));
        assert!(!verify(&pk1, b"x", &sig1), "message");
        assert!(!verify(&pk2, b"", &sig1), "another key");
        assert!(!verify(&pk1, b"", &sig2), "another signature");
        for i in [0, 31, 32, 63] {
            let mut s = sig1;
            s[i] ^= 1;
            assert!(!verify(&pk1, b"", &s), "one flipped bit at byte {i}");
        }
    }

    #[test]
    fn a_small_order_key_is_refused() {
        // The identity point (y = 1) is a valid encoding of a small-order key; `verify_strict` refuses it.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(!verify(&identity, b"", &[0u8; 64]));
        // A key that is not a point at all.
        assert!(!verify(&[0xff; 32], b"", &hex(SIG1)));
    }
}
