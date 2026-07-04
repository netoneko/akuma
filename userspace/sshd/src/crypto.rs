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

/// A `SimpleRng` seeded from the kernel's hardware entropy (`getrandom`).
/// Falls back to a fixed non-zero seed if the syscall fails (`SimpleRng`
/// itself refuses an all-zero seed) — matches the previous behavior of never
/// leaving the PRNG in a wedged all-zero state.
pub fn new_seeded_rng() -> SimpleRng {
    let mut seed = [0u8; 8];
    let _ = libakuma::getrandom(&mut seed);
    SimpleRng::from_seed(seed)
}
