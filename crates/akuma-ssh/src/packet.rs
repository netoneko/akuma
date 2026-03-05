use alloc::vec::Vec;
use core::convert::TryInto;

use akuma_ssh_crypto::crypto::{HmacSha256, MAC_SIZE};
use ctr::cipher::StreamCipher;
use hmac::Mac;

use crate::session::SshSession;

pub fn process_encrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 4 {
        return None;
    }

    let cipher = session.crypto.decrypt_cipher.as_mut()?;

    let mut peek_cipher = cipher.clone();

    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&session.input_buffer[..4]);
    peek_cipher.apply_keystream(&mut len_buf);
    let packet_len = u32::from_be_bytes(len_buf) as usize;

    let total_needed = 4 + packet_len + MAC_SIZE;
    if session.input_buffer.len() < total_needed {
        return None;
    }

    let encrypted_data = &session.input_buffer[..4 + packet_len];
    let received_mac = &session.input_buffer[4 + packet_len..total_needed];

    let mut decrypted = encrypted_data.to_vec();
    cipher.apply_keystream(&mut decrypted);

    let seq = session.crypto.decrypt_seq;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&session.crypto.decrypt_mac_key).ok()?;
    mac.update(&seq.to_be_bytes());
    mac.update(&decrypted);

    if mac.verify_slice(received_mac).is_err() {
        log::warn!(
            "[SSH] MAC verification failed (seq={}, pkt_len={}, buf_len={})",
            seq,
            packet_len,
            session.input_buffer.len()
        );
        return None;
    }

    session.crypto.decrypt_seq = seq.wrapping_add(1);

    let padding_len = decrypted[4] as usize;
    let payload_len = packet_len - padding_len - 1;

    if 5 + payload_len > decrypted.len() {
        return None;
    }

    let msg_type = decrypted[5];
    let payload = decrypted[6..5 + payload_len].to_vec();

    session.input_buffer = session.input_buffer[total_needed..].to_vec();

    Some((msg_type, payload))
}

pub fn process_unencrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 5 {
        return None;
    }

    let packet_len = u32::from_be_bytes(session.input_buffer[..4].try_into().ok()?) as usize;
    let total_len = 4 + packet_len;

    if session.input_buffer.len() < total_len {
        return None;
    }

    let padding_len = session.input_buffer[4] as usize;
    let payload_len = packet_len - padding_len - 1;

    let msg_type = session.input_buffer[5];
    let payload = session.input_buffer[6..5 + payload_len].to_vec();

    session.crypto.decrypt_seq = session.crypto.decrypt_seq.wrapping_add(1);
    session.input_buffer = session.input_buffer[total_len..].to_vec();

    Some((msg_type, payload))
}
