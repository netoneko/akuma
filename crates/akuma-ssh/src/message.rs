use alloc::vec;
use alloc::vec::Vec;

use akuma_ssh_crypto::auth::AuthResult;
use akuma_ssh_crypto::crypto::{read_string, read_u32, write_string, write_u32};
use embedded_io_async::Write;

use crate::config::SshdConfig;
use crate::constants::{
    SSH_MSG_CHANNEL_CLOSE, SSH_MSG_CHANNEL_DATA, SSH_MSG_CHANNEL_EOF,
    SSH_MSG_CHANNEL_FAILURE, SSH_MSG_CHANNEL_OPEN, SSH_MSG_CHANNEL_OPEN_CONFIRMATION,
    SSH_MSG_CHANNEL_REQUEST, SSH_MSG_CHANNEL_SUCCESS, SSH_MSG_DEBUG, SSH_MSG_DISCONNECT,
    SSH_MSG_GLOBAL_REQUEST, SSH_MSG_IGNORE, SSH_MSG_KEX_ECDH_INIT, SSH_MSG_KEXINIT,
    SSH_MSG_NEWKEYS, SSH_MSG_REQUEST_FAILURE, SSH_MSG_SERVICE_ACCEPT, SSH_MSG_SERVICE_REQUEST,
    SSH_MSG_UNIMPLEMENTED, SSH_MSG_USERAUTH_REQUEST,
};
use crate::kex;
use crate::session::{SshSession, SshState};
use crate::transport::{send_packet, send_unencrypted_packet};

/// Pluggable authentication provider. The kernel implements this trait
/// with filesystem-backed authorized key loading.
pub trait AuthProvider {
    fn authenticate(
        &self,
        payload: &[u8],
        session_id: &[u8; 32],
        config: &SshdConfig,
    ) -> impl core::future::Future<Output = (AuthResult, Vec<u8>)>;
}

/// Result of handling a single SSH protocol message.
pub enum MessageResult {
    Continue,
    StartShell,
    ExecCommand(Vec<u8>),
    Disconnect,
}

#[allow(clippy::too_many_lines, clippy::future_not_send)]
pub async fn handle_message<T, A>(
    stream: &mut T,
    msg_type: u8,
    payload: &[u8],
    session: &mut SshSession,
    auth_provider: &A,
) -> Result<MessageResult, T::Error>
where
    T: Write,
    A: AuthProvider,
{
    log::debug!("[SSH] Received message type {msg_type}");

    match msg_type {
        SSH_MSG_KEXINIT => {
            let mut full = vec![SSH_MSG_KEXINIT];
            full.extend_from_slice(payload);
            session.client_kexinit = full;

            let kexinit = kex::build_kexinit(&mut session.rng);
            session.server_kexinit.clone_from(&kexinit);

            send_unencrypted_packet(stream, &kexinit, session).await?;
            session.state = SshState::AwaitingKexEcdhInit;
        }

        SSH_MSG_KEX_ECDH_INIT => {
            let mut offset = 0;
            if let Some(client_pubkey) = read_string(payload, &mut offset) {
                if let Some(reply) = kex::handle_kex_ecdh_init(session, client_pubkey) {
                    send_unencrypted_packet(stream, &reply, session).await?;

                    let newkeys = vec![SSH_MSG_NEWKEYS];
                    send_unencrypted_packet(stream, &newkeys, session).await?;
                    session.state = SshState::AwaitingNewKeys;
                } else {
                    log::warn!("[SSH] KEX failed");
                }
            }
        }

        SSH_MSG_NEWKEYS => {
            log::info!("[SSH] Encryption activated");
            session.state = SshState::AwaitingServiceRequest;
        }

        SSH_MSG_SERVICE_REQUEST => {
            let mut offset = 0;
            if let Some(service) = read_string(payload, &mut offset) {
                log::debug!(
                    "[SSH] Service request: {:?}",
                    core::str::from_utf8(service)
                );

                let mut reply = vec![SSH_MSG_SERVICE_ACCEPT];
                write_string(&mut reply, service);
                send_packet(stream, &reply, session).await?;
                session.state = SshState::AwaitingUserAuth;
            }
        }

        SSH_MSG_USERAUTH_REQUEST => {
            let (result, reply) =
                auth_provider.authenticate(payload, &session.session_id, &session.config).await;

            send_packet(stream, &reply, session).await?;

            match result {
                AuthResult::Success => {
                    session.state = SshState::Authenticated;
                    log::info!("[SSH] User authenticated");
                }
                AuthResult::PublicKeyOk(_) => {
                    log::info!("[SSH] Public key accepted, waiting for signature");
                }
                AuthResult::Failure => {
                    log::info!("[SSH] Authentication failed, waiting for retry");
                }
            }
        }

        SSH_MSG_CHANNEL_OPEN => {
            let mut offset = 0;
            let channel_type = read_string(payload, &mut offset);
            let sender_channel = read_u32(payload, &mut offset);
            let initial_window = read_u32(payload, &mut offset);
            let max_packet = read_u32(payload, &mut offset);

            if let (Some(_), Some(sender), Some(_), Some(_)) =
                (channel_type, sender_channel, initial_window, max_packet)
            {
                session.client_channel = sender;
                session.channel_open = true;

                let mut reply = vec![SSH_MSG_CHANNEL_OPEN_CONFIRMATION];
                write_u32(&mut reply, sender);
                write_u32(&mut reply, 0);
                write_u32(&mut reply, 0x10_0000);
                write_u32(&mut reply, 0x4000);
                send_packet(stream, &reply, session).await?;

                log::info!("[SSH] Channel opened");
            }
        }

        SSH_MSG_CHANNEL_REQUEST => {
            return handle_channel_request(stream, payload, session).await;
        }

        SSH_MSG_CHANNEL_DATA => {
            log::debug!("[SSH] Unexpected channel data outside shell mode");
        }

        SSH_MSG_CHANNEL_EOF | SSH_MSG_CHANNEL_CLOSE => {
            log::info!("[SSH] Channel close requested");
            let mut reply = vec![SSH_MSG_CHANNEL_CLOSE];
            write_u32(&mut reply, session.client_channel);
            send_packet(stream, &reply, session).await?;
            session.channel_open = false;
        }

        SSH_MSG_GLOBAL_REQUEST => {
            let reply = vec![SSH_MSG_REQUEST_FAILURE];
            send_packet(stream, &reply, session).await?;
        }

        SSH_MSG_DISCONNECT => {
            log::info!("[SSH] Client disconnected");
            session.state = SshState::Disconnected;
            return Ok(MessageResult::Disconnect);
        }

        SSH_MSG_IGNORE | SSH_MSG_DEBUG => {}

        _ => {
            log::debug!("[SSH] Unhandled message type {msg_type}");
            let mut reply = vec![SSH_MSG_UNIMPLEMENTED];
            write_u32(&mut reply, session.crypto.decrypt_seq.wrapping_sub(1));
            send_packet(stream, &reply, session).await?;
        }
    }

    Ok(MessageResult::Continue)
}

async fn handle_channel_request<T: Write>(
    stream: &mut T,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<MessageResult, T::Error> {
    let mut offset = 0;
    let _recipient = read_u32(payload, &mut offset);
    let request_type = read_string(payload, &mut offset);
    let want_reply = if offset < payload.len() {
        payload[offset] != 0
    } else {
        false
    };

    if let Some(req_type) = request_type {
        log::debug!(
            "[SSH] Channel request: {:?}",
            core::str::from_utf8(req_type)
        );

        let success = matches!(req_type, b"pty-req" | b"shell" | b"env" | b"exec");

        if req_type == b"pty-req" {
            offset += 1;
            let _term = read_string(payload, &mut offset);
            if let Some(width) = read_u32(payload, &mut offset)
                && let Some(height) = read_u32(payload, &mut offset)
            {
                session.term_width = width;
                session.term_height = height;
                log::info!("[SSH] Terminal size: {width}x{height}");
            }
        }

        if want_reply {
            let reply_type = if success {
                SSH_MSG_CHANNEL_SUCCESS
            } else {
                SSH_MSG_CHANNEL_FAILURE
            };
            let mut full_reply = vec![reply_type];
            write_u32(&mut full_reply, session.client_channel);
            send_packet(stream, &full_reply, session).await?;
        }

        if req_type == b"shell" {
            return Ok(MessageResult::StartShell);
        } else if req_type == b"exec" {
            log::info!("[SSH] Exec request");
            offset += 1;
            if let Some(cmd_bytes) = read_string(payload, &mut offset) {
                return Ok(MessageResult::ExecCommand(cmd_bytes.to_vec()));
            }
        }
    }

    Ok(MessageResult::Continue)
}
