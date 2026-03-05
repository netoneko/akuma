//! SSH Authentication — kernel wrapper.
//!
//! Re-exports types from `akuma_ssh_crypto::auth` and provides the async
//! `handle_userauth_request` that loads authorized keys from the filesystem.

use alloc::vec::Vec;

pub use akuma_ssh_crypto::auth::{
    AuthResult, SSH_MSG_USERAUTH_FAILURE, SSH_MSG_USERAUTH_PK_OK, SSH_MSG_USERAUTH_SUCCESS,
    build_failure_response, build_pk_ok_response, build_success_response,
};

use super::config::SshdConfig;
use super::crypto::read_string;
use super::keys::load_authorized_keys;
use crate::console;

/// Handle a userauth request (async — loads authorized keys from disk).
pub async fn handle_userauth_request(
    payload: &[u8],
    session_id: &[u8; 32],
    config: &SshdConfig,
) -> (AuthResult, Vec<u8>) {
    let mut offset = 0;

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

    safe_print!(
        256,
        "[SSH Auth] Auth request: user={:?}, service={:?}, method={:?}\n",
        core::str::from_utf8(username),
        core::str::from_utf8(service),
        core::str::from_utf8(method)
    );

    if config.disable_key_verification {
        log("[SSH Auth] Key verification disabled, accepting auth\n");
        return (AuthResult::Success, build_success_response());
    }

    match method {
        b"none" => (AuthResult::Failure, build_failure_response()),
        b"publickey" => {
            let authorized_keys = load_authorized_keys().await;
            akuma_ssh_crypto::auth::handle_publickey_auth(
                payload,
                &mut offset,
                session_id,
                username,
                service,
                &authorized_keys,
            )
        }
        b"password" => {
            log("[SSH Auth] Password auth not supported\n");
            (AuthResult::Failure, build_failure_response())
        }
        _ => {
            safe_print!(256, 
                "[SSH Auth] Unknown auth method: {:?}\n",
                core::str::from_utf8(method)
            );
            (AuthResult::Failure, build_failure_response())
        }
    }
}

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
