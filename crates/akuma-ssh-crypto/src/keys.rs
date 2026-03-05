//! SSH Key Format Utilities
//!
//! Base64 encoding/decoding and SSH public key wire format parsing.
//! Host key persistence (async file I/O) remains in the kernel.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use ed25519_dalek::VerifyingKey;

use crate::crypto::{read_string, write_string};

// ============================================================================
// Base64 Encoding (simple implementation for `no_std`)
// ============================================================================

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes to base64
#[must_use] 
pub fn base64_encode(data: &[u8]) -> String {
    let mut result = String::new();
    let mut i = 0;

    while i < data.len() {
        let b0 = u32::from(data[i]);
        let b1 = if i + 1 < data.len() {
            u32::from(data[i + 1])
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            u32::from(data[i + 2])
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < data.len() {
            result.push(BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if i + 2 < data.len() {
            result.push(BASE64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        i += 3;
    }

    result
}

/// Decode base64 to bytes
#[must_use] 
pub fn base64_decode(data: &str) -> Option<Vec<u8>> {
    let data = data.trim();
    if data.is_empty() {
        return Some(Vec::new());
    }

    let mut result = Vec::new();
    let bytes: Vec<u8> = data.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();

    if !bytes.len().is_multiple_of(4) {
        return None;
    }

    for chunk in bytes.chunks(4) {
        let mut vals = [0u8; 4];
        for (i, &b) in chunk.iter().enumerate() {
            vals[i] = if b == b'=' {
                0
            } else if let Some(pos) = BASE64_ALPHABET.iter().position(|&c| c == b) {
                pos as u8
            } else {
                return None;
            };
        }

        let triple = (u32::from(vals[0]) << 18)
            | (u32::from(vals[1]) << 12)
            | (u32::from(vals[2]) << 6)
            | u32::from(vals[3]);

        result.push((triple >> 16) as u8);
        if chunk[2] != b'=' {
            result.push((triple >> 8) as u8);
        }
        if chunk[3] != b'=' {
            result.push(triple as u8);
        }
    }

    Some(result)
}

// ============================================================================
// Key Format Functions
// ============================================================================

/// Encode a public key in SSH wire format.
///
/// Returns `"ssh-ed25519 BASE64_KEY"`.
#[must_use] 
pub fn encode_public_key_ssh(key: &VerifyingKey) -> String {
    let mut blob = Vec::new();
    write_string(&mut blob, b"ssh-ed25519");
    write_string(&mut blob, key.as_bytes());

    let encoded = base64_encode(&blob);
    format!("ssh-ed25519 {encoded}")
}

/// Parse a public key from SSH `authorized_keys` format.
///
/// Accepts lines like `ssh-ed25519 BASE64 comment`.
#[must_use] 
pub fn parse_public_key_ssh(line: &str) -> Option<VerifyingKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    if parts[0] != "ssh-ed25519" {
        return None;
    }

    let blob = base64_decode(parts[1])?;

    let mut offset = 0;
    let key_type = read_string(&blob, &mut offset)?;
    if key_type != b"ssh-ed25519" {
        return None;
    }

    let key_bytes = read_string(&blob, &mut offset)?;
    if key_bytes.len() != 32 {
        return None;
    }

    let key_array: [u8; 32] = key_bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&key_array).ok()
}
