//! SSH Host Key Management — kernel wrapper.
//!
//! Re-exports pure key-format functions from `akuma_ssh_crypto::keys` and
//! provides async host-key persistence (filesystem I/O).

use alloc::vec::Vec;
use ed25519_dalek::{SigningKey, VerifyingKey};
use spinning_top::Spinlock;

pub use akuma_ssh_crypto::keys::parse_public_key_ssh;

use crate::async_fs;

// ============================================================================
// Constants
// ============================================================================

const AUTHORIZED_KEYS_PATH: &str = "/etc/sshd/authorized_keys";

// ============================================================================
// Global Host Key
// ============================================================================

static HOST_KEY: Spinlock<Option<SigningKey>> = Spinlock::new(None);

pub fn set_host_key(key: SigningKey) {
    let mut guard = HOST_KEY.lock();
    *guard = Some(key);
    log("[SSH Keys] Host key set\n");
}

pub fn get_host_key() -> Option<SigningKey> {
    HOST_KEY.lock().clone()
}

// ============================================================================
// Key Loading
// ============================================================================

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
            safe_print!(256, "[SSH Keys] Loaded {} authorized keys\n", keys.len());
        }
        Err(e) => {
            safe_print!(256, "[SSH Keys] Failed to read authorized_keys: {}\n", e);
        }
    }

    keys
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
