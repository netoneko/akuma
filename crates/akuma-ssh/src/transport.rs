use alloc::vec;
use alloc::vec::Vec;

use akuma_ssh_crypto::crypto::{build_packet, write_string, write_u32};
use embedded_io_async::Write;

use crate::constants::SSH_MSG_CHANNEL_DATA;
use crate::session::{SshSession, SshState};

pub async fn send_raw<T: Write>(stream: &mut T, data: &[u8]) -> Result<(), T::Error> {
    stream.write_all(data).await
}

pub async fn send_unencrypted_packet<T: Write>(
    stream: &mut T,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), T::Error> {
    let packet = build_packet(payload);
    session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
    send_raw(stream, &packet).await
}

pub async fn send_encrypted_packet<T: Write>(
    stream: &mut T,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), T::Error> {
    if let Some(cipher) = session.crypto.encrypt_cipher.as_mut() {
        let seq = session.crypto.encrypt_seq;
        session.crypto.encrypt_seq = seq.wrapping_add(1);
        let packet = akuma_ssh_crypto::crypto::build_encrypted_packet(
            payload,
            cipher,
            &session.crypto.encrypt_mac_key,
            seq,
            &mut session.rng,
        );
        send_raw(stream, &packet).await
    } else {
        Ok(())
    }
}

pub async fn send_packet<T: Write>(
    stream: &mut T,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), T::Error> {
    if session.crypto.encrypt_cipher.is_some() && session.state != SshState::AwaitingNewKeys {
        send_encrypted_packet(stream, payload, session).await
    } else {
        send_unencrypted_packet(stream, payload, session).await
    }
}

pub async fn send_channel_data<T: Write>(
    stream: &mut T,
    session: &mut SshSession,
    data: &[u8],
) -> Result<(), T::Error> {
    if !session.channel_open || data.is_empty() {
        return Ok(());
    }

    log::trace!("[SSH] Sending {} bytes channel data", data.len());

    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, session.client_channel);
    write_string(&mut payload, data);
    send_packet(stream, &payload, session).await
}

/// Build a raw SSH channel data packet without sending it.
/// Useful for callers that need to control when data is written.
#[must_use]
pub fn build_channel_data_payload(client_channel: u32, data: &[u8]) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, client_channel);
    write_string(&mut payload, data);
    payload
}
