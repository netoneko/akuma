//! SSH Authentication Primitives
//!
//! Pure signature-verification and response-building functions.
//! The async key-loading and session dispatch remain in the kernel.

use alloc::vec;
use alloc::vec::Vec;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::crypto::{read_string, write_string};

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
    /// For publickey queries without signature — key is acceptable
    PublicKeyOk(Vec<u8>),
}

// ============================================================================
// Public Key Verification
// ============================================================================

/// Parse an SSH public key blob to extract the ed25519 key.
#[must_use] 
pub fn parse_key_blob(blob: &[u8]) -> Option<VerifyingKey> {
    let mut offset = 0;
    let key_type = read_string(blob, &mut offset)?;
    if key_type != b"ssh-ed25519" {
        return None;
    }

    let key_bytes = read_string(blob, &mut offset)?;
    if key_bytes.len() != 32 {
        return None;
    }

    // Reject the identity point (all zeros) and other small-order points.
    // These are cryptographically degenerate and could allow signature forgery
    // depending on the Ed25519 backend's verification strictness.
    if is_low_order_point(key_bytes) {
        return None;
    }

    let key_array: [u8; 32] = key_bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&key_array).ok()
}

/// Known small-order points on the Ed25519 curve (compressed y-coordinates).
/// Any public key matching one of these can trivially forge signatures.
pub const LOW_ORDER_POINTS: [[u8; 32]; 5] = [
    [0; 32], // identity (0, 1)
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
     0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], // (0, 1) alt encoding
    [0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f], // order-2 point
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80], // (0, -1)
    [0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f,
     0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67, 0x0f,
     0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6,
     0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac, 0x03, 0x7a], // order-4 point
];

fn is_low_order_point(key_bytes: &[u8]) -> bool {
    LOW_ORDER_POINTS.iter().any(|p| p == key_bytes)
}

/// Parse an SSH signature blob to extract the ed25519 signature.
#[must_use] 
pub fn parse_signature_blob(blob: &[u8]) -> Option<Signature> {
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

/// Build the data that the client signed for publickey auth.
#[must_use] 
pub fn build_signed_data(
    session_id: &[u8; 32],
    username: &[u8],
    service: &[u8],
    algorithm: &[u8],
    key_blob: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();

    write_string(&mut data, session_id);
    data.push(50); // SSH_MSG_USERAUTH_REQUEST
    write_string(&mut data, username);
    write_string(&mut data, service);
    write_string(&mut data, b"publickey");
    data.push(1); // TRUE
    write_string(&mut data, algorithm);
    write_string(&mut data, key_blob);

    data
}

/// Verify an Ed25519 signature against the signed data.
#[must_use] 
pub fn verify_signature(
    key: &VerifyingKey,
    signed_data: &[u8],
    signature: &Signature,
) -> bool {
    key.verify(signed_data, signature).is_ok()
}

// ============================================================================
// Response Builders
// ============================================================================

#[must_use] 
pub fn build_success_response() -> Vec<u8> {
    vec![SSH_MSG_USERAUTH_SUCCESS]
}

#[must_use] 
pub fn build_failure_response() -> Vec<u8> {
    let mut response = vec![SSH_MSG_USERAUTH_FAILURE];
    write_string(&mut response, b"publickey");
    response.push(0);
    response
}

#[must_use] 
pub fn build_pk_ok_response(algorithm: &[u8], key_blob: &[u8]) -> Vec<u8> {
    let mut response = vec![SSH_MSG_USERAUTH_PK_OK];
    write_string(&mut response, algorithm);
    write_string(&mut response, key_blob);
    response
}

/// Handle the publickey auth flow (synchronous — caller provides authorized keys).
///
/// Returns `(AuthResult, response_bytes)`.
pub fn handle_publickey_auth(
    payload: &[u8],
    offset: &mut usize,
    session_id: &[u8; 32],
    username: &[u8],
    service: &[u8],
    authorized_keys: &[VerifyingKey],
) -> (AuthResult, Vec<u8>) {
    if *offset >= payload.len() {
        return (AuthResult::Failure, build_failure_response());
    }

    let has_signature = payload[*offset] != 0;
    *offset += 1;

    let Some(algorithm) = read_string(payload, offset) else {
        return (AuthResult::Failure, build_failure_response());
    };

    let Some(key_blob) = read_string(payload, offset) else {
        return (AuthResult::Failure, build_failure_response());
    };

    if algorithm != b"ssh-ed25519" {
        return (AuthResult::Failure, build_failure_response());
    }

    let Some(client_key) = parse_key_blob(key_blob) else {
        return (AuthResult::Failure, build_failure_response());
    };

    let is_authorized = authorized_keys
        .iter()
        .any(|k| k.as_bytes() == client_key.as_bytes());

    if !is_authorized {
        return (AuthResult::Failure, build_failure_response());
    }

    if !has_signature {
        return (
            AuthResult::PublicKeyOk(key_blob.to_vec()),
            build_pk_ok_response(algorithm, key_blob),
        );
    }

    let Some(signature_blob) = read_string(payload, offset) else {
        return (AuthResult::Failure, build_failure_response());
    };

    let Some(signature) = parse_signature_blob(signature_blob) else {
        return (AuthResult::Failure, build_failure_response());
    };

    let signed_data = build_signed_data(session_id, username, service, algorithm, key_blob);

    if verify_signature(&client_key, &signed_data, &signature) {
        (AuthResult::Success, build_success_response())
    } else {
        (AuthResult::Failure, build_failure_response())
    }
}
