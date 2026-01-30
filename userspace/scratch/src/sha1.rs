//! SHA-1 hashing utilities for Git objects
//!
//! Git uses SHA-1 to identify all objects. The hash is computed over:
//! "{type} {size}\0{content}"

use alloc::format;
use alloc::string::String;

/// SHA-1 hash (20 bytes)
pub type Sha1Hash = [u8; 20];

/// Compute SHA-1 hash of raw data
pub fn hash(data: &[u8]) -> Sha1Hash {
    let digest = sha1_smol::Sha1::from(data).digest();
    digest.bytes()
}

/// Compute SHA-1 hash of a Git object
///
/// Git hashes include a header: "{type} {size}\0"
pub fn hash_object(object_type: &str, data: &[u8]) -> Sha1Hash {
    let header = format!("{} {}\0", object_type, data.len());
    
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    hasher.digest().bytes()
}

/// Convert SHA-1 hash to hex string
pub fn to_hex(hash: &Sha1Hash) -> String {
    let mut hex = String::with_capacity(40);
    for byte in hash {
        hex.push(HEX_CHARS[(byte >> 4) as usize]);
        hex.push(HEX_CHARS[(byte & 0x0f) as usize]);
    }
    hex
}

/// Parse hex string to SHA-1 hash
pub fn from_hex(hex: &str) -> Option<Sha1Hash> {
    if hex.len() != 40 {
        return None;
    }
    
    let mut hash = [0u8; 20];
    let bytes = hex.as_bytes();
    
    for i in 0..20 {
        let high = hex_digit(bytes[i * 2])?;
        let low = hex_digit(bytes[i * 2 + 1])?;
        hash[i] = (high << 4) | low;
    }
    
    Some(hash)
}

const HEX_CHARS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7',
    '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        let hash = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67,
        ];
        let hex = to_hex(&hash);
        assert_eq!(hex, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(from_hex(&hex), Some(hash));
    }
}
