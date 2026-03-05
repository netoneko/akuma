//! SSH Host Key Management — kernel wrapper.
//!
//! Re-exports pure key-format functions from `akuma_ssh_crypto::keys` and
//! provides async host-key persistence (filesystem I/O).

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey, VerifyingKey};
use spinning_top::Spinlock;

pub use akuma_ssh_crypto::keys::{
    base64_decode, base64_encode, encode_public_key_ssh, parse_public_key_ssh,
};

use super::crypto::SimpleRng;
use crate::async_fs;

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

pub fn set_host_key(key: SigningKey) {
    let mut guard = HOST_KEY.lock();
    *guard = Some(key);
    log("[SSH Keys] Host key set\n");
}

pub fn get_host_key() -> Option<SigningKey> {
    HOST_KEY.lock().clone()
}

// ============================================================================
// Key Loading and Generation
// ============================================================================

async fn ensure_sshd_directory() {
    if !async_fs::exists("/etc").await {
        if let Err(e) = async_fs::create_dir("/etc").await {
            safe_print!(256, "[SSH Keys] Failed to create /etc: {}\n", e);
        }
    }
    if !async_fs::exists(SSHD_DIR).await {
        if let Err(e) = async_fs::create_dir(SSHD_DIR).await {
            safe_print!(256, "[SSH Keys] Failed to create {}: {}\n", SSHD_DIR, e);
        }
    }
}

fn generate_keypair() -> SigningKey {
    let mut rng = SimpleRng::new();
    let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
    rng.fill_bytes(&mut key_bytes);
    SigningKey::from_bytes(&key_bytes)
}

pub async fn load_or_generate_host_key() -> SigningKey {
    ensure_sshd_directory().await;

    if async_fs::exists(HOST_KEY_PATH).await {
        match async_fs::read_file(HOST_KEY_PATH).await {
            Ok(data) => {
                if data.len() == SECRET_KEY_LENGTH {
                    let key_bytes: [u8; SECRET_KEY_LENGTH] = data.try_into().unwrap();
                    let key = SigningKey::from_bytes(&key_bytes);
                    log("[SSH Keys] Loaded host key from filesystem\n");
                    set_host_key(key.clone());
                    return key;
                }
                safe_print!(
                    256,
                    "[SSH Keys] Invalid key length: {}, expected {}\n",
                    data.len(),
                    SECRET_KEY_LENGTH
                );
            }
            Err(e) => {
                safe_print!(256, "[SSH Keys] Failed to read host key: {}\n", e);
            }
        }
    }

    log("[SSH Keys] Generating new host key...\n");
    let key = generate_keypair();

    if let Err(e) = async_fs::write_file(HOST_KEY_PATH, key.as_bytes()).await {
        safe_print!(256, "[SSH Keys] Failed to save private key: {}\n", e);
    } else {
        safe_print!(256, "[SSH Keys] Saved private key to {}\n", HOST_KEY_PATH);
    }

    let pub_key = key.verifying_key();
    let pub_key_str = encode_public_key_ssh(&pub_key);
    let pub_key_line = format!("{}\n", pub_key_str);
    if let Err(e) = async_fs::write_file(HOST_KEY_PUB_PATH, pub_key_line.as_bytes()).await {
        safe_print!(256, "[SSH Keys] Failed to save public key: {}\n", e);
    } else {
        safe_print!(256, "[SSH Keys] Saved public key to {}\n", HOST_KEY_PUB_PATH);
    }

    if !async_fs::exists(AUTHORIZED_KEYS_PATH).await {
        let auth_keys_content = format!(
            "# Authorized SSH Keys\n# Add public keys here, one per line\n{}\n",
            pub_key_str
        );
        if let Err(e) =
            async_fs::write_file(AUTHORIZED_KEYS_PATH, auth_keys_content.as_bytes()).await
        {
            safe_print!(256, "[SSH Keys] Failed to save authorized_keys: {}\n", e);
        } else {
            safe_print!(
                256,
                "[SSH Keys] Created {} with host public key\n",
                AUTHORIZED_KEYS_PATH
            );
        }
    }

    set_host_key(key.clone());
    key
}

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
