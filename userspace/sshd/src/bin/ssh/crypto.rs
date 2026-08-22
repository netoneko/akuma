//! SSH cryptography glue for the `ssh` client binary.
//!
//! Mirrors `crate::crypto` in the `sshd` binary (`../../crypto.rs`), but lives
//! under `bin/ssh/` because `sshd`'s binary is `src/main.rs` and `ssh`'s is
//! `src/bin/ssh/main.rs` — separate crate roots, so they can't share
//! modules directly with each other (both can, and do, reach into the
//! package's *lib* target instead — see `sshd::client_wire`). Rather than
//! restructure `lib.rs` to share this six-line seeding helper too, it's kept
//! as a small duplicate; the actual primitives it re-exports still come from
//! the one shared crate (`akuma_ssh_crypto`).

pub use akuma_ssh_crypto::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, MAC_KEY_SIZE, SimpleRng,
    build_encrypted_packet, build_packet, derive_key, read_string, read_u32, write_string,
    write_u32,
};

/// A `SimpleRng` seeded from the kernel's hardware entropy (`getrandom`).
/// Falls back to a fixed non-zero seed if the syscall fails (`SimpleRng`
/// itself refuses an all-zero seed).
pub fn new_seeded_rng() -> SimpleRng {
    let mut seed = [0u8; 8];
    let _ = libakuma::getrandom(&mut seed);
    SimpleRng::from_seed(seed)
}
