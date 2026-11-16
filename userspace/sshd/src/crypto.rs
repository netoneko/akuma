//! SSH cryptography and wire-format glue for the userspace sshd.
//!
//! The actual primitives (packet framing, key derivation, `SimpleRng`, byte
//! utilities) live in `akuma-ssh-crypto` — a host-testable `no_std` crate
//! shared with the in-kernel SSH server (`src/ssh/`, via `crates/akuma-ssh`) —
//! rather than a duplicated, untested copy. This module only adds the one
//! thing that's genuinely sshd-specific: seeding that crate's RNG from the
//! kernel's hardware entropy syscall.

pub use akuma_ssh_crypto::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, HmacSha256, MAC_KEY_SIZE, MAC_SIZE,
    SimpleRng, build_encrypted_packet, build_packet, derive_key, read_string, read_u32,
    write_namelist, write_string, write_u32,
};

/// Fill `buf` from the kernel's entropy source (`getrandom`), or report that it
/// could not be filled.
///
/// **Every secret this server makes comes from here and nowhere else**: the
/// X25519 ephemeral secret, the Ed25519 host key, and the KEXINIT cookie (which
/// is public, but is drawn here too so no value on the wire is ever an output of
/// the generator below). `false` means the caller must refuse to go on; there is
/// no fallback, because a fallback is a key an attacker can compute.
#[must_use]
pub fn secure_random(buf: &mut [u8]) -> bool {
    // libakuma's `getrandom` serves at most 256 bytes a call and may be short.
    let mut filled = 0;
    while filled < buf.len() {
        match libakuma::getrandom(&mut buf[filled..]) {
            Ok(n) if n > 0 => filled += n.min(buf.len() - filled),
            _ => return false,
        }
    }
    true
}

/// A `SimpleRng` for **encrypted-packet padding only**.
///
/// `SimpleRng` is xorshift64: 64 bits of state, and each output *is* the state.
/// It must never produce a key, a secret or anything sent in clear. Until
/// 2026-11-16 it produced the KEXINIT cookie and then the X25519 secret, so the
/// cookie handed an eavesdropper the secret
/// (`docs/archive/SECURITY_AUDIT_2026_11_16.md` C1). Padding bytes are encrypted
/// and carry no secret, so a failed seed read here is harmless.
pub fn new_padding_rng() -> SimpleRng {
    let mut seed = [0u8; 8];
    let _ = libakuma::getrandom(&mut seed);
    SimpleRng::from_seed(seed)
}
