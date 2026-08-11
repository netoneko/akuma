//! SSH Host Key Management.
//!
//! Handles loading, generating, and persisting SSH host keys, plus loading
//! `/etc/sshd/authorized_keys`. Keys are stored in `/etc/sshd/id_ed25519`
//! (private) and `/etc/sshd/id_ed25519.pub` (public).
//!
//! The base64 codec and SSH wire-format parsing (`encode_public_key_ssh`,
//! `parse_public_key_ssh`) are re-exported from `akuma_ssh_crypto::keys` —
//! the host-testable crate shared with the in-kernel SSH server — rather
//! than duplicated here. See `docs/archive/TRIM_FAT_HAND_ROLLED_PARSERS.md`.

use alloc::vec::Vec;
use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey, VerifyingKey};
use spinning_top::Spinlock;

use super::crypto::new_seeded_rng;
use libakuma::*;

// Re-export the host-testable SSH wire-format helpers instead of keeping a
// local copy. `parse_public_key_ssh` is consumed by `load_authorized_keys`
// below; `encode_public_key_ssh` by `load_or_generate_host_key`. Both are
// also exercised by `userspace/akuma-ssh-crypto`'s own test suite, which is
// the point of sharing them.
pub use akuma_ssh_crypto::keys::{encode_public_key_ssh, parse_public_key_ssh};

// ============================================================================
// Constants
// ============================================================================

const SSHD_DIR: &str = "/etc/sshd";
const HOST_KEY_PATH: &str = "/etc/sshd/id_ed25519";
const HOST_KEY_PUB_PATH: &str = "/etc/sshd/id_ed25519.pub";
const AUTHORIZED_KEYS_PATH: &str = "/etc/sshd/authorized_keys";

// ============================================================================
// Global Host Key
// ============================================================================

static HOST_KEY: Spinlock<Option<SigningKey>> = Spinlock::new(None);

/// Set the host key (used during initialization)
pub fn set_host_key(key: SigningKey) {
    let mut guard = HOST_KEY.lock();
    *guard = Some(key);
}

/// Get a clone of the shared host key
pub fn get_host_key() -> Option<SigningKey> {
    HOST_KEY.lock().clone()
}

// ============================================================================
// Key Loading and Generation
// ============================================================================

/// Generate a new Ed25519 keypair
fn generate_keypair() -> SigningKey {
    let mut rng = new_seeded_rng();
    let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
    rng.fill_bytes(&mut key_bytes);
    SigningKey::from_bytes(&key_bytes)
}

/// Helper to read file to Vec<u8>
fn read_file_to_vec(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(fd);
    }

    let mut result = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut buf);
        if n < 0 {
            close(fd);
            return Err(n as i32);
        }
        if n == 0 {
            break;
        }
        result.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Ok(result)
}

/// Helper to write Vec<u8> to file
fn write_vec_to_file(path: &str, data: &[u8]) -> Result<(), i32> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(fd);
    }

    let n = write_fd(fd, data);
    close(fd);
    if n < 0 {
        Err(n as i32)
    } else {
        Ok(())
    }
}

/// Load or generate the SSH host key (Async version for protocol.rs)
pub async fn load_or_generate_host_key() -> SigningKey {
    let _ = mkdir_p(SSHD_DIR);

    // Try to load existing key
    if let Ok(data) = read_file_to_vec(HOST_KEY_PATH)
        && data.len() == SECRET_KEY_LENGTH
    {
        let key_bytes: [u8; SECRET_KEY_LENGTH] = data.try_into().unwrap();
        let key = SigningKey::from_bytes(&key_bytes);
        println("[SSH Keys] Loaded host key from filesystem");
        set_host_key(key.clone());
        return key;
    }

    // Generate new keypair
    println("[SSH Keys] Generating new host key...");
    let key = generate_keypair();

    // Save private key
    let _ = write_vec_to_file(HOST_KEY_PATH, key.as_bytes());

    // Save public key in SSH format
    let pub_key = key.verifying_key();
    let pub_key_str = encode_public_key_ssh(&pub_key);
    let pub_key_line = alloc::format!("{}\n", pub_key_str);
    let _ = write_vec_to_file(HOST_KEY_PUB_PATH, pub_key_line.as_bytes());

    set_host_key(key.clone());
    key
}

/// Load authorized keys from the filesystem
pub async fn load_authorized_keys() -> Vec<VerifyingKey> {
    let mut keys = Vec::new();

    if let Ok(data) = read_file_to_vec(AUTHORIZED_KEYS_PATH)
        && let Ok(content) = core::str::from_utf8(&data)
    {
        for line in content.lines() {
            if let Some(key) = parse_public_key_ssh(line) {
                keys.push(key);
            }
        }
    }

    keys
}
