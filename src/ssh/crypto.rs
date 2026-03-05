//! SSH Cryptography — thin kernel wrapper over `akuma_ssh_crypto::crypto`.
//!
//! Re-exports the crate's pure functions and adds a kernel-specific
//! `SimpleRng::new()` that seeds from hardware entropy.

use alloc::vec::Vec;

pub use akuma_ssh_crypto::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, HmacSha256, MAC_KEY_SIZE, MAC_SIZE,
    build_packet, derive_key, read_string, read_u32, split_first_word, trim_bytes, write_namelist,
    write_string, write_u32,
};

use crate::rng;
use crate::timer;

/// Kernel-side `SimpleRng` that seeds from hardware entropy via VirtIO RNG,
/// falling back to the timer if unavailable.
pub struct SimpleRng(akuma_ssh_crypto::crypto::SimpleRng);

impl SimpleRng {
    pub fn new() -> Self {
        let mut seed_bytes = [0u8; 8];
        if rng::fill_bytes(&mut seed_bytes).is_err() {
            seed_bytes = (timer::uptime_us() ^ 0xDEAD_BEEF_CAFE_BABE).to_le_bytes();
        }
        Self(akuma_ssh_crypto::crypto::SimpleRng::from_seed(seed_bytes))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }
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
