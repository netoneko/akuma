//! SSH Authentication
//!
//! Handles SSH user authentication, including public key verification.

use alloc::vec;
use alloc::vec::Vec;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use super::config::SshdConfig;
use super::crypto::{read_string, write_string};
use super::keys::load_authorized_keys;
use crate::console;

// ============================================================================
// SSH Message Types
// ============================================================================

pub const SSH_MSG_USERAUTH_FAILURE: u8 = 51;
pub const SSH_MSG_USERAUTH_SUCCESS: u8 = 52;
pub const SSH_MSG_USERAUTH_PK_OK: u8 = 60;

// ============================================================================
// Authentication Result
// ============================================================================

/// Result of authentication attempt
#[derive(Debug)]
pub enum AuthResult {
    /// Authentication successful
    Success,
    /// Authentication failed, send failure message with available methods
    Failure,
    /// For publickey queries without signature - key is acceptable
    PublicKeyOk(Vec<u8>),
}

// ============================================================================
// Authentication Handler
// ============================================================================

/// Handle a userauth request
/// Returns the appropriate response to send to the client
pub async fn handle_userauth_request(
    payload: &[u8],
    session_id: &[u8; 32],
    config: &SshdConfig,
) -> (AuthResult, Vec<u8>) {
    let mut offset = 0;

    // Parse userauth request
    // Format: string user, string service, string method, ...
    let username = match read_string(payload, &mut offset) {
        Some(u) => u,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    let service = match read_string(payload, &mut offset) {
        Some(s) => s,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    let method = match read_string(payload, &mut offset) {
        Some(m) => m,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    log(&alloc::format!(
        "[SSH Auth] Auth request: user={:?}, service={:?}, method={:?}\n",
        core::str::from_utf8(username),
        core::str::from_utf8(service),
        core::str::from_utf8(method)
    ));

    let disable_key_verification = config.disable_key_verification;
    // let disable_key_verification = true;

    // If key verification is disabled, accept any auth
    if disable_key_verification {
        log("[SSH Auth] Key verification disabled, accepting auth\n");
        return (AuthResult::Success, build_success_response());
    }

    match method {
        b"none" => {
            // Client is querying available methods
            (AuthResult::Failure, build_failure_response())
        }
        b"publickey" => {
            handle_publickey_auth(payload, &mut offset, session_id, username, service).await
        }
        b"password" => {
            // We don't support password auth when key verification is enabled
            log("[SSH Auth] Password auth not supported\n");
            (AuthResult::Failure, build_failure_response())
        }
        _ => {
            log(&alloc::format!(
                "[SSH Auth] Unknown auth method: {:?}\n",
                core::str::from_utf8(method)
            ));
            (AuthResult::Failure, build_failure_response())
        }
    }
}

/// Handle publickey authentication
async fn handle_publickey_auth(
    payload: &[u8],
    offset: &mut usize,
    session_id: &[u8; 32],
    username: &[u8],
    service: &[u8],
) -> (AuthResult, Vec<u8>) {
    // Format: boolean has_signature, string algorithm, string key, [string signature]
    if *offset >= payload.len() {
        return (AuthResult::Failure, build_failure_response());
    }

    let has_signature = payload[*offset] != 0;
    *offset += 1;

    let algorithm = match read_string(payload, offset) {
        Some(a) => a,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    let key_blob = match read_string(payload, offset) {
        Some(k) => k,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    log(&alloc::format!(
        "[SSH Auth] Publickey: alg={:?}, has_sig={}\n",
        core::str::from_utf8(algorithm),
        has_signature
    ));

    // Only support ssh-ed25519
    if algorithm != b"ssh-ed25519" {
        log("[SSH Auth] Unsupported key algorithm\n");
        return (AuthResult::Failure, build_failure_response());
    }

    // Parse the public key from the blob
    let client_key = match parse_key_blob(key_blob) {
        Some(k) => k,
        None => {
            log("[SSH Auth] Failed to parse client public key\n");
            return (AuthResult::Failure, build_failure_response());
        }
    };

    // Load authorized keys and check if this key is authorized
    let authorized_keys = load_authorized_keys().await;
    let is_authorized = authorized_keys.iter().any(|k| k.as_bytes() == client_key.as_bytes());

    if !is_authorized {
        log("[SSH Auth] Key not in authorized_keys\n");
        return (AuthResult::Failure, build_failure_response());
    }

    if !has_signature {
        // Client is asking if this key is acceptable
        log("[SSH Auth] Key query - key is acceptable\n");
        return (
            AuthResult::PublicKeyOk(key_blob.to_vec()),
            build_pk_ok_response(algorithm, key_blob),
        );
    }

    // Verify the signature
    let signature_blob = match read_string(payload, offset) {
        Some(s) => s,
        None => return (AuthResult::Failure, build_failure_response()),
    };

    // Parse the signature
    let signature = match parse_signature_blob(signature_blob) {
        Some(s) => s,
        None => {
            log("[SSH Auth] Failed to parse signature\n");
            return (AuthResult::Failure, build_failure_response());
        }
    };

    // Build the data that was signed
    // Format: string session_id, byte SSH_MSG_USERAUTH_REQUEST, string user, 
    //         string service, string "publickey", boolean TRUE, string algorithm, string key
    let signed_data = build_signed_data(session_id, username, service, algorithm, key_blob);

    // Verify the signature
    match client_key.verify(&signed_data, &signature) {
        Ok(()) => {
            log("[SSH Auth] Signature verified successfully\n");
            (AuthResult::Success, build_success_response())
        }
        Err(e) => {
            log(&alloc::format!("[SSH Auth] Signature verification failed: {:?}\n", e));
            (AuthResult::Failure, build_failure_response())
        }
    }
}

/// Parse an SSH public key blob to extract the ed25519 key
fn parse_key_blob(blob: &[u8]) -> Option<VerifyingKey> {
    let mut offset = 0;
    let key_type = read_string(blob, &mut offset)?;
    if key_type != b"ssh-ed25519" {
        return None;
    }

    let key_bytes = read_string(blob, &mut offset)?;
    if key_bytes.len() != 32 {
        return None;
    }

    let key_array: [u8; 32] = key_bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&key_array).ok()
}

/// Parse an SSH signature blob to extract the ed25519 signature
fn parse_signature_blob(blob: &[u8]) -> Option<Signature> {
    let mut offset = 0;
    let sig_type = read_string(blob, &mut offset)?;
    if sig_type != b"ssh-ed25519" {
        return None;
    }

    let sig_bytes = read_string(blob, &mut offset)?;
    if sig_bytes.len() != 64 {
        return None;
    }

    let sig_array: [u8; 64] = sig_bytes.try_into().ok()?;
    Some(Signature::from_bytes(&sig_array))
}

/// Build the data that the client signed for publickey auth
fn build_signed_data(
    session_id: &[u8; 32],
    username: &[u8],
    service: &[u8],
    algorithm: &[u8],
    key_blob: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();

    // string session_id
    write_string(&mut data, session_id);

    // byte SSH_MSG_USERAUTH_REQUEST (50)
    data.push(50);

    // string user
    write_string(&mut data, username);

    // string service
    write_string(&mut data, service);

    // string "publickey"
    write_string(&mut data, b"publickey");

    // boolean TRUE
    data.push(1);

    // string algorithm
    write_string(&mut data, algorithm);

    // string key
    write_string(&mut data, key_blob);

    data
}

// ============================================================================
// Response Builders
// ============================================================================

fn build_success_response() -> Vec<u8> {
    vec![SSH_MSG_USERAUTH_SUCCESS]
}

fn build_failure_response() -> Vec<u8> {
    let mut response = vec![SSH_MSG_USERAUTH_FAILURE];
    // name-list of available methods
    write_string(&mut response, b"publickey");
    // partial success
    response.push(0);
    response
}

fn build_pk_ok_response(algorithm: &[u8], key_blob: &[u8]) -> Vec<u8> {
    let mut response = vec![SSH_MSG_USERAUTH_PK_OK];
    write_string(&mut response, algorithm);
    write_string(&mut response, key_blob);
    response
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

