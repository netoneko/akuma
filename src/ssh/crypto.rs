//! SSH Cryptography — thin kernel wrapper over `akuma_ssh_crypto::crypto`.
//!
//! Re-exports the crate's pure functions and adds a kernel-specific
//! `SimpleRng::new()` that seeds from hardware entropy.

use alloc::vec::Vec;

pub use akuma_ssh_crypto::crypto::{
    Aes128Ctr, MAC_KEY_SIZE,
    read_string, read_u32, split_first_word, trim_bytes,
    write_u32,
};

use crate::rng;
use crate::timer;

/// Kernel-side `SimpleRng` that seeds from hardware entropy via VirtIO RNG,
/// falling back to the timer if unavailable.
pub struct SimpleRng(akuma_ssh_crypto::crypto::SimpleRng);

impl SimpleRng {
    pub fn new() -> Self {
        Self(create_seeded_rng())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }
}

/// Create a hardware-seeded `SimpleRng` suitable for the `akuma-ssh` crate.
pub fn create_seeded_rng() -> akuma_ssh_crypto::crypto::SimpleRng {
    let mut seed_bytes = [0u8; 8];
    if rng::fill_bytes(&mut seed_bytes).is_err() {
        seed_bytes = (timer::uptime_us() ^ 0xDEAD_BEEF_CAFE_BABE).to_le_bytes();
    }
    akuma_ssh_crypto::crypto::SimpleRng::from_seed(seed_bytes)
}

/// Kernel wrapper that auto-creates an RNG from hardware entropy.
pub fn build_encrypted_packet(
    payload: &[u8],
    cipher: &mut Aes128Ctr,
    mac_key: &[u8; MAC_KEY_SIZE],
    seq: u32,
) -> Vec<u8> {
    let mut rng = SimpleRng::new();
    akuma_ssh_crypto::crypto::build_encrypted_packet(payload, cipher, mac_key, seq, &mut rng.0)
}
