//! SSH Cryptography and Utility Functions
//!
//! This module contains:
//! - Simple RNG for key generation
//! - Crypto state management (AES-CTR, HMAC)
//! - SSH packet building and parsing
//! - Key derivation functions
//! - Byte utility functions

use alloc::vec::Vec;
use core::convert::TryInto;

use aes::Aes128;
use ctr::{Ctr128BE, cipher::StreamCipher};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::rng;
use crate::timer;

// ============================================================================
// Constants
// ============================================================================

pub const AES_KEY_SIZE: usize = 16;
pub const AES_IV_SIZE: usize = 16;
pub const MAC_KEY_SIZE: usize = 32;
pub const MAC_SIZE: usize = 32;

// ============================================================================
// Type Aliases
// ============================================================================

pub type Aes128Ctr = Ctr128BE<Aes128>;
pub type HmacSha256 = Hmac<Sha256>;

// ============================================================================
// Simple RNG using hardware entropy when available
// ============================================================================

pub struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    pub fn new() -> Self {
        let mut seed_bytes = [0u8; 8];
        if rng::fill_bytes(&mut seed_bytes).is_ok() {
            Self {
                state: u64::from_le_bytes(seed_bytes),
            }
        } else {
            // Fallback to timer if hardware RNG not available
            Self {
                state: timer::uptime_us() ^ 0xDEADBEEFCAFEBABE,
            }
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let val = self.next_u64();
            let bytes = val.to_le_bytes();
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = bytes[i];
            }
        }
    }
}

// ============================================================================
// Crypto State
// ============================================================================

pub struct CryptoState {
    pub decrypt_cipher: Option<Aes128Ctr>,
    pub decrypt_mac_key: [u8; MAC_KEY_SIZE],
    pub decrypt_seq: u32,
    pub encrypt_cipher: Option<Aes128Ctr>,
    pub encrypt_mac_key: [u8; MAC_KEY_SIZE],
    pub encrypt_seq: u32,
}

impl CryptoState {
    pub fn new() -> Self {
        Self {
            decrypt_cipher: None,
            decrypt_mac_key: [0u8; MAC_KEY_SIZE],
            decrypt_seq: 0,
            encrypt_cipher: None,
            encrypt_mac_key: [0u8; MAC_KEY_SIZE],
            encrypt_seq: 0,
        }
    }
}

// ============================================================================
// SSH Packet Helpers
// ============================================================================

/// Write a u32 in big-endian format
pub fn write_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Write a length-prefixed string
pub fn write_string(buf: &mut Vec<u8>, s: &[u8]) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s);
}

/// Write a name-list (comma-separated, length-prefixed)
pub fn write_namelist(buf: &mut Vec<u8>, names: &[&str]) {
    let joined = names.join(",");
    write_string(buf, joined.as_bytes());
}

/// Read a u32 from buffer at offset
pub fn read_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    if *offset + 4 > data.len() {
        return None;
    }
    let val = u32::from_be_bytes(data[*offset..*offset + 4].try_into().ok()?);
    *offset += 4;
    Some(val)
}

/// Read a length-prefixed string from buffer at offset
pub fn read_string<'a>(data: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    let len = read_u32(data, offset)? as usize;
    if *offset + len > data.len() {
        return None;
    }
    let s = &data[*offset..*offset + len];
    *offset += len;
    Some(s)
}

/// Build an unencrypted SSH packet
pub fn build_packet(payload: &[u8]) -> Vec<u8> {
    let padding_len = 8 - ((5 + payload.len()) % 8);
    let padding_len = if padding_len < 4 {
        padding_len + 8
    } else {
        padding_len
    };

    let packet_len = 1 + payload.len() + padding_len;
    let mut packet = Vec::with_capacity(4 + packet_len);

    write_u32(&mut packet, packet_len as u32);
    packet.push(padding_len as u8);
    packet.extend_from_slice(payload);
    packet.resize(packet.len() + padding_len, 0);

    packet
}

/// Build an encrypted SSH packet with MAC
pub fn build_encrypted_packet(
    payload: &[u8],
    cipher: &mut Aes128Ctr,
    mac_key: &[u8; MAC_KEY_SIZE],
    seq: u32,
) -> Vec<u8> {
    let padding_len = 16 - ((5 + payload.len()) % 16);
    let padding_len = if padding_len < 4 {
        padding_len + 16
    } else {
        padding_len
    };

    let packet_len = 1 + payload.len() + padding_len;
    let mut packet = Vec::with_capacity(4 + packet_len + MAC_SIZE);

    write_u32(&mut packet, packet_len as u32);
    packet.push(padding_len as u8);
    packet.extend_from_slice(payload);

    // Add random padding
    let mut rng = SimpleRng::new();
    let pad_start = packet.len();
    packet.resize(pad_start + padding_len, 0);
    rng.fill_bytes(&mut packet[pad_start..]);

    // Compute MAC before encryption: MAC(key, seq || unencrypted_packet)
    let mut mac = <HmacSha256 as Mac>::new_from_slice(mac_key).unwrap();
    mac.update(&seq.to_be_bytes());
    mac.update(&packet);
    let mac_result = mac.finalize().into_bytes();

    // Encrypt the packet
    cipher.apply_keystream(&mut packet);

    // Append MAC
    packet.extend_from_slice(&mac_result);

    packet
}

// ============================================================================
// Key Derivation
// ============================================================================

/// Derive a key using SSH key derivation function
/// K1 = HASH(K || H || letter || session_id)
pub fn derive_key(k: &[u8], h: &[u8], letter: u8, session_id: &[u8], size: usize) -> Vec<u8> {
    let mut hasher = Sha256::new();

    // K is encoded as mpint (with leading zero if high bit set)
    let mut k_mpint = Vec::new();
    if !k.is_empty() && k[0] & 0x80 != 0 {
        write_u32(&mut k_mpint, (k.len() + 1) as u32);
        k_mpint.push(0);
    } else {
        write_u32(&mut k_mpint, k.len() as u32);
    }
    k_mpint.extend_from_slice(k);

    hasher.update(&k_mpint);
    hasher.update(h);
    hasher.update(&[letter]);
    hasher.update(session_id);

    let mut result: Vec<u8> = hasher.finalize().to_vec();

    // If we need more bytes, continue hashing
    while result.len() < size {
        let mut hasher = Sha256::new();
        hasher.update(&k_mpint);
        hasher.update(h);
        hasher.update(&result);
        result.extend_from_slice(&hasher.finalize());
    }

    result.truncate(size);
    result
}

// ============================================================================
// Byte Utilities
// ============================================================================

/// Trim leading and trailing ASCII whitespace from bytes
pub fn trim_bytes(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|&b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &data[start..end]
}

/// Split at first whitespace, returning (first_word, rest_trimmed)
pub fn split_first_word(data: &[u8]) -> (&[u8], &[u8]) {
    if let Some(pos) = data.iter().position(|&b| b.is_ascii_whitespace()) {
        (&data[..pos], trim_bytes(&data[pos..]))
    } else {
        (data, &[])
    }
}
