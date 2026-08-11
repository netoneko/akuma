//! SSH Authentication (dispatcher only).
//!
//! The actual cryptographic verification of `publickey` requests — blob
//! parsing, low-order-point rejection, signature verification, response
//! building — lives in `akuma_ssh_crypto::auth`, the host-testable crate
//! shared with the (now-removed) in-kernel SSH server via `crates/akuma-ssh`.
//! This module's only sshd-specific responsibilities are:
//!
//! - reading the `SSH_MSG_USERAUTH_REQUEST` envelope (user / service / method),
//! - honoring the `disable_key_verification` config flag,
//! - asynchronously loading `/etc/sshd/authorized_keys` (which the sync crypto
//!   helper takes as a parameter).
//!
//! Previously this file carried local copies of `parse_key_blob`,
//! `parse_signature_blob`, `build_signed_data`, and the response builders —
//! near-verbatim duplicates of the crypto crate's versions, and the local
//! `parse_key_blob` lacked the low-order-point / identity-point rejection
//! that protects against signature forgery on degenerate Ed25519 keys
//! (`akuma-ssh-crypto/src/auth.rs:56`). See
//! `docs/archive/TRIM_FAT_HAND_ROLLED_PARSERS.md`.

use alloc::vec::Vec;

// Re-export so callers (`protocol.rs`) match on the same enum the crypto
// crate returns from `handle_publickey_auth`.
pub use akuma_ssh_crypto::auth::AuthResult;

use akuma_ssh_crypto::auth::{
    build_failure_response, build_success_response, handle_publickey_auth,
};

use super::config::SshdConfig;
use super::crypto::read_string;
use super::keys::load_authorized_keys;
use libakuma::*;

/// Handle a userauth request.
///
/// Returns `(AuthResult, reply_bytes)`. The reply is what to send to the
/// client; the `AuthResult` discriminates `Success` so the caller can flip
/// the session state.
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

    println(&alloc::format!(
        "[SSH Auth] Auth request: user={:?}, service={:?}, method={:?}",
        core::str::from_utf8(username),
        core::str::from_utf8(service),
        core::str::from_utf8(method)
    ));

    // If key verification is disabled, accept any auth
    if config.disable_key_verification {
        println("[SSH Auth] Key verification disabled, accepting auth");
        return (AuthResult::Success, build_success_response());
    }

    match method {
        b"none" => {
            // Client is querying available methods
            (AuthResult::Failure, build_failure_response())
        }
        b"publickey" => {
            // The async half the sync crypto helper can't do: load authorized
            // keys from the filesystem, then delegate blob parsing, signature
            // verification, and response building to the shared crate.
            let authorized_keys = load_authorized_keys().await;
            let (result, reply) = handle_publickey_auth(
                payload,
                &mut offset,
                session_id,
                username,
                service,
                &authorized_keys,
            );
            match &result {
                AuthResult::Success => println("[SSH Auth] Signature verified successfully"),
                AuthResult::Failure => println("[SSH Auth] Publickey auth failed"),
                AuthResult::PublicKeyOk(_) => {
                    println("[SSH Auth] Key query - key is acceptable")
                }
            }
            (result, reply)
        }
        b"password" => {
            // We don't support password auth when key verification is enabled
            println("[SSH Auth] Password auth not supported");
            (AuthResult::Failure, build_failure_response())
        }
        _ => {
            println(&alloc::format!(
                "[SSH Auth] Unknown auth method: {:?}",
                core::str::from_utf8(method)
            ));
            (AuthResult::Failure, build_failure_response())
        }
    }
}
