//! SSH Key Format Utilities
//!
//! Base64 encoding/decoding and SSH public key wire format parsing.
//! Host key persistence (async file I/O) remains in the kernel.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::crypto::{read_string, read_u32, write_string};

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
            } else {
                BASE64_ALPHABET.iter().position(|&c| c == b)? as u8
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

// ============================================================================
// OpenSSH private key container (`-----BEGIN OPENSSH PRIVATE KEY-----`)
// ============================================================================

/// First line of an OpenSSH private-key file, as `ssh-keygen` (7.8+) writes it.
pub const OPENSSH_PEM_BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const OPENSSH_PEM_END: &str = "-----END OPENSSH PRIVATE KEY-----";
/// `openssh-key-v1\0`: the magic that opens the decoded body.
const OPENSSH_MAGIC: &[u8] = b"openssh-key-v1\0";

/// Why an OpenSSH private-key file could not be turned into an Ed25519 seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpensshKeyError {
    /// Not an OpenSSH PEM container at all (no BEGIN/END lines, or no magic).
    NotOpenssh,
    /// The container is damaged: bad base64, truncated fields, mismatched check
    /// integers, a public key that is not the seed's, or wrong padding.
    Malformed,
    /// Protected by a passphrase (a cipher / KDF other than `none`). Not supported.
    Encrypted,
    /// A valid container, but the key is not a single `ssh-ed25519` key.
    NotEd25519,
    /// A container holding more than one key. `ssh-keygen` never writes one.
    MultipleKeys,
}

impl OpensshKeyError {
    /// A sentence for an operator, not a code.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::NotOpenssh => "not an OpenSSH private key",
            Self::Malformed => "damaged or truncated OpenSSH private key",
            Self::Encrypted => "OpenSSH private key is passphrase-protected (passphrases are not supported; \
                                make one without with: ssh-keygen -t ed25519 -N '')",
            Self::NotEd25519 => "OpenSSH private key is not an ssh-ed25519 key (only Ed25519 is supported)",
            Self::MultipleKeys => "OpenSSH private key file holds more than one key",
        }
    }
}

/// True when `text` opens like an OpenSSH private-key file (cheap pre-check for callers that
/// also accept the raw 32-byte format).
#[must_use]
pub fn looks_like_openssh_private_key(text: &str) -> bool {
    text.trim_start().starts_with(OPENSSH_PEM_BEGIN)
}

/// Extract the 32-byte Ed25519 seed from an unencrypted OpenSSH private-key file (the
/// `openssh-key-v1` container, PROTOCOL.key in the OpenSSH source):
///
/// ```text
/// "openssh-key-v1\0"  string cipher  string kdf  string kdfoptions  uint32 nkeys
/// string publickey     string private-section
/// private-section = uint32 check1  uint32 check2  string "ssh-ed25519"  string pub[32]
///                   string (seed[32] || pub[32])  string comment  padding 1,2,3,...
/// ```
///
/// Everything an attacker-chosen file could get wrong is checked rather than trusted: both check
/// integers must match, the public key in the outer blob, the private section and the one derived
/// from the seed must all agree, and the padding must be exactly 1,2,3,... A file that fails is
/// rejected as a whole; nothing partial is returned.
///
/// # Errors
/// See [`OpensshKeyError`].
pub fn parse_openssh_private_key_ed25519(text: &str) -> Result<[u8; 32], OpensshKeyError> {
    use OpensshKeyError::{Encrypted, Malformed, MultipleKeys, NotEd25519, NotOpenssh};

    let text = text.trim();
    let body = text.strip_prefix(OPENSSH_PEM_BEGIN).ok_or(NotOpenssh)?;
    let body = body.strip_suffix(OPENSSH_PEM_END).ok_or(NotOpenssh)?;
    let b64: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = base64_decode(&b64).ok_or(Malformed)?;

    let rest = raw.strip_prefix(OPENSSH_MAGIC).ok_or(NotOpenssh)?;
    let mut off = 0usize;
    let cipher = read_string(rest, &mut off).ok_or(Malformed)?;
    let kdf = read_string(rest, &mut off).ok_or(Malformed)?;
    let _kdf_options = read_string(rest, &mut off).ok_or(Malformed)?;
    if cipher != b"none" || kdf != b"none" {
        return Err(Encrypted);
    }
    match read_u32(rest, &mut off).ok_or(Malformed)? {
        1 => {}
        0 => return Err(Malformed),
        _ => return Err(MultipleKeys),
    }
    let outer_pub = read_string(rest, &mut off).ok_or(Malformed)?;
    let private = read_string(rest, &mut off).ok_or(Malformed)?;
    if off != rest.len() {
        return Err(Malformed);
    }

    // The outer public blob: string "ssh-ed25519", string pub[32].
    let mut o = 0usize;
    if read_string(outer_pub, &mut o).ok_or(Malformed)? != b"ssh-ed25519" {
        return Err(NotEd25519);
    }
    let outer_key = read_string(outer_pub, &mut o).ok_or(Malformed)?;
    if outer_key.len() != 32 || o != outer_pub.len() {
        return Err(Malformed);
    }

    let mut p = 0usize;
    let check1 = read_u32(private, &mut p).ok_or(Malformed)?;
    let check2 = read_u32(private, &mut p).ok_or(Malformed)?;
    if check1 != check2 {
        return Err(Malformed);
    }
    if read_string(private, &mut p).ok_or(Malformed)? != b"ssh-ed25519" {
        return Err(NotEd25519);
    }
    let inner_pub = read_string(private, &mut p).ok_or(Malformed)?;
    let secret = read_string(private, &mut p).ok_or(Malformed)?;
    let _comment = read_string(private, &mut p).ok_or(Malformed)?;
    if inner_pub.len() != 32 || secret.len() != 64 || inner_pub != outer_key {
        return Err(Malformed);
    }
    // Padding up to the cipher block size (8 for "none"): 1, 2, 3, ...
    for (i, b) in private[p..].iter().enumerate() {
        if usize::from(*b) != i + 1 {
            return Err(Malformed);
        }
    }

    let seed: [u8; 32] = secret[..32].try_into().map_err(|_| Malformed)?;
    if &secret[32..] != inner_pub {
        return Err(Malformed);
    }
    // The seed must really produce the public key the file claims.
    if SigningKey::from_bytes(&seed).verifying_key().as_bytes() != inner_pub {
        return Err(Malformed);
    }
    Ok(seed)
}
