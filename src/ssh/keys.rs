//! SSH Host Key Management
//!
//! Handles loading, generating, and persisting SSH host keys.
//! Keys are stored in /etc/sshd/id_ed25519 (private) and /etc/sshd/id_ed25519.pub (public).

use alloc::string::String;
use alloc::vec::Vec;
use ed25519_dalek::{SigningKey, VerifyingKey, SECRET_KEY_LENGTH};
use spinning_top::Spinlock;

use super::crypto::SimpleRng;
use crate::async_fs;
use crate::console;

// ============================================================================
// Constants
// ============================================================================

const SSHD_DIR: &str = "/etc/sshd";
const HOST_KEY_PATH: &str = "/etc/sshd/id_ed25519";
const HOST_KEY_PUB_PATH: &str = "/etc/sshd/id_ed25519.pub";
const AUTHORIZED_KEYS_PATH: &str = "/etc/sshd/authorized_keys";

// ============================================================================
// Base64 Encoding (simple implementation for no_std)
// ============================================================================

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes to base64
pub fn base64_encode(data: &[u8]) -> String {
    let mut result = String::new();
    let mut i = 0;

    while i < data.len() {
        let b0 = data[i] as u32;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as u32
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
pub fn base64_decode(data: &str) -> Option<Vec<u8>> {
    let data = data.trim();
    if data.is_empty() {
        return Some(Vec::new());
    }

    let mut result = Vec::new();
    let bytes: Vec<u8> = data.bytes().filter(|&b| b != b'\n' && b != b'\r').collect();

    if bytes.len() % 4 != 0 {
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

        let triple = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | (vals[3] as u32);

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
// Global Host Key
// ============================================================================

static HOST_KEY: Spinlock<Option<SigningKey>> = Spinlock::new(None);

/// Set the host key (used during initialization)
pub fn set_host_key(key: SigningKey) {
    let mut guard = HOST_KEY.lock();
    *guard = Some(key);
    log("[SSH Keys] Host key set\n");
}

/// Get a clone of the shared host key
pub fn get_host_key() -> Option<SigningKey> {
    HOST_KEY.lock().clone()
}

// ============================================================================
// Key Format Functions
// ============================================================================

/// Encode a public key in SSH wire format (for authorized_keys)
/// Format: "ssh-ed25519 BASE64_KEY"
pub fn encode_public_key_ssh(key: &VerifyingKey) -> String {
    use super::crypto::write_string;

    // Build the key blob: string "ssh-ed25519" + string key_bytes
    let mut blob = Vec::new();
    write_string(&mut blob, b"ssh-ed25519");
    write_string(&mut blob, key.as_bytes());

    // Encode as base64
    let encoded = base64_encode(&blob);

    alloc::format!("ssh-ed25519 {}", encoded)
}

/// Parse a public key from SSH authorized_keys format
/// Returns None if parsing fails
pub fn parse_public_key_ssh(line: &str) -> Option<VerifyingKey> {
    use super::crypto::read_string;

    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    // Format: key_type base64_key [comment]
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    if parts[0] != "ssh-ed25519" {
        return None;
    }

    // Decode base64
    let blob = base64_decode(parts[1])?;

    // Parse the blob
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
// Key Loading and Generation
// ============================================================================

/// Ensure the /etc/sshd directory exists
async fn ensure_sshd_directory() {
    if !async_fs::exists("/etc").await {
        if let Err(e) = async_fs::create_dir("/etc").await {
            log(&alloc::format!("[SSH Keys] Failed to create /etc: {}\n", e));
        }
    }
    if !async_fs::exists(SSHD_DIR).await {
        if let Err(e) = async_fs::create_dir(SSHD_DIR).await {
            log(&alloc::format!(
                "[SSH Keys] Failed to create {}: {}\n",
                SSHD_DIR, e
            ));
        }
    }
}

/// Generate a new Ed25519 keypair
fn generate_keypair() -> SigningKey {
    let mut rng = SimpleRng::new();
    let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
    rng.fill_bytes(&mut key_bytes);
    SigningKey::from_bytes(&key_bytes)
}

/// Load or generate the SSH host key
/// If a key exists at /etc/sshd/id_ed25519, load it.
/// Otherwise, generate a new keypair and save both private and public keys.
pub async fn load_or_generate_host_key() -> SigningKey {
    ensure_sshd_directory().await;

    // Try to load existing key
    if async_fs::exists(HOST_KEY_PATH).await {
        match async_fs::read_file(HOST_KEY_PATH).await {
            Ok(data) => {
                if data.len() == SECRET_KEY_LENGTH {
                    let key_bytes: [u8; SECRET_KEY_LENGTH] = data.try_into().unwrap();
                    let key = SigningKey::from_bytes(&key_bytes);
                    log("[SSH Keys] Loaded host key from filesystem\n");
                    set_host_key(key.clone());
                    return key;
                } else {
                    log(&alloc::format!(
                        "[SSH Keys] Invalid key length: {}, expected {}\n",
                        data.len(),
                        SECRET_KEY_LENGTH
                    ));
                }
            }
            Err(e) => {
                log(&alloc::format!(
                    "[SSH Keys] Failed to read host key: {}\n",
                    e
                ));
            }
        }
    }

    // Generate new keypair
    log("[SSH Keys] Generating new host key...\n");
    let key = generate_keypair();

    // Save private key (raw 32 bytes)
    if let Err(e) = async_fs::write_file(HOST_KEY_PATH, key.as_bytes()).await {
        log(&alloc::format!(
            "[SSH Keys] Failed to save private key: {}\n",
            e
        ));
    } else {
        log(&alloc::format!("[SSH Keys] Saved private key to {}\n", HOST_KEY_PATH));
    }

    // Save public key in SSH format
    let pub_key = key.verifying_key();
    let pub_key_str = encode_public_key_ssh(&pub_key);
    let pub_key_line = alloc::format!("{}\n", pub_key_str);
    if let Err(e) = async_fs::write_file(HOST_KEY_PUB_PATH, pub_key_line.as_bytes()).await {
        log(&alloc::format!(
            "[SSH Keys] Failed to save public key: {}\n",
            e
        ));
    } else {
        log(&alloc::format!("[SSH Keys] Saved public key to {}\n", HOST_KEY_PUB_PATH));
    }

    // Also add to authorized_keys if it doesn't exist
    if !async_fs::exists(AUTHORIZED_KEYS_PATH).await {
        let auth_keys_content = alloc::format!("# Authorized SSH Keys\n# Add public keys here, one per line\n{}\n", pub_key_str);
        if let Err(e) = async_fs::write_file(AUTHORIZED_KEYS_PATH, auth_keys_content.as_bytes()).await {
            log(&alloc::format!(
                "[SSH Keys] Failed to save authorized_keys: {}\n",
                e
            ));
        } else {
            log(&alloc::format!("[SSH Keys] Created {} with host public key\n", AUTHORIZED_KEYS_PATH));
        }
    }

    set_host_key(key.clone());
    key
}

/// Load authorized keys from the filesystem
pub async fn load_authorized_keys() -> Vec<VerifyingKey> {
    let mut keys = Vec::new();

    if !async_fs::exists(AUTHORIZED_KEYS_PATH).await {
        return keys;
    }

    match async_fs::read_to_string(AUTHORIZED_KEYS_PATH).await {
        Ok(content) => {
            for line in content.lines() {
                if let Some(key) = parse_public_key_ssh(line) {
                    keys.push(key);
                }
            }
            log(&alloc::format!(
                "[SSH Keys] Loaded {} authorized keys\n",
                keys.len()
            ));
        }
        Err(e) => {
            log(&alloc::format!(
                "[SSH Keys] Failed to read authorized_keys: {}\n",
                e
            ));
        }
    }

    keys
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

