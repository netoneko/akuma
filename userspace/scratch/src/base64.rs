//! Base64 encoding for HTTP Basic authentication
//!
//! Minimal implementation for encoding credentials.

use alloc::string::String;
use alloc::vec::Vec;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes to base64 string
pub fn encode(data: &[u8]) -> String {
    let mut result = Vec::with_capacity((data.len() + 2) / 3 * 4);
    
    let mut i = 0;
    while i + 3 <= data.len() {
        let b0 = data[i] as usize;
        let b1 = data[i + 1] as usize;
        let b2 = data[i + 2] as usize;
        
        result.push(ALPHABET[b0 >> 2]);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)]);
        result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)]);
        result.push(ALPHABET[b2 & 0x3f]);
        
        i += 3;
    }
    
    // Handle remaining bytes
    let remaining = data.len() - i;
    if remaining == 1 {
        let b0 = data[i] as usize;
        result.push(ALPHABET[b0 >> 2]);
        result.push(ALPHABET[(b0 & 0x03) << 4]);
        result.push(b'=');
        result.push(b'=');
    } else if remaining == 2 {
        let b0 = data[i] as usize;
        let b1 = data[i + 1] as usize;
        result.push(ALPHABET[b0 >> 2]);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)]);
        result.push(ALPHABET[(b1 & 0x0f) << 2]);
        result.push(b'=');
    }
    
    String::from_utf8(result).unwrap_or_default()
}

/// Create HTTP Basic auth header value
///
/// Format: "Basic <base64(username:password)>"
pub fn basic_auth(username: &str, password: &str) -> String {
    use alloc::format;
    let credentials = format!("{}:{}", username, password);
    let encoded = encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_encode() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }
}
