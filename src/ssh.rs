//! SSH-2 Protocol Implementation (Async Multi-Session)
//!
//! Implements a minimal SSH-2 server that works with real SSH clients.
//! Supports:
//! - curve25519-sha256 key exchange
//! - ssh-ed25519 host key
//! - aes128-ctr encryption
//! - hmac-sha2-256 MAC
//! - Accepts any authentication
//! - Shell with basic commands
//! - Multiple concurrent SSH sessions

use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;
use spinning_top::Spinlock;

use ed25519_dalek::{Signer, SigningKey, SECRET_KEY_LENGTH};
use hmac::Mac;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;

use crate::async_net::{TcpError, TcpStream};
use crate::console;
use crate::shell;
use crate::ssh_crypto::{
    build_encrypted_packet, build_packet, derive_key, read_string, read_u32, write_namelist,
    write_string, write_u32, Aes128Ctr, CryptoState, HmacSha256, SimpleRng, AES_IV_SIZE,
    AES_KEY_SIZE, MAC_KEY_SIZE, MAC_SIZE,
};

// ============================================================================
// SSH Constants
// ============================================================================

const SSH_VERSION: &[u8] = b"SSH-2.0-Akuma_0.1\r\n";

// SSH Message Types
const SSH_MSG_DISCONNECT: u8 = 1;
const SSH_MSG_IGNORE: u8 = 2;
const SSH_MSG_UNIMPLEMENTED: u8 = 3;
const SSH_MSG_DEBUG: u8 = 4;
const SSH_MSG_SERVICE_REQUEST: u8 = 5;
const SSH_MSG_SERVICE_ACCEPT: u8 = 6;
const SSH_MSG_KEXINIT: u8 = 20;
const SSH_MSG_NEWKEYS: u8 = 21;
const SSH_MSG_KEX_ECDH_INIT: u8 = 30;
const SSH_MSG_KEX_ECDH_REPLY: u8 = 31;
const SSH_MSG_USERAUTH_REQUEST: u8 = 50;
const SSH_MSG_USERAUTH_SUCCESS: u8 = 52;
const SSH_MSG_GLOBAL_REQUEST: u8 = 80;
const SSH_MSG_REQUEST_FAILURE: u8 = 82;
const SSH_MSG_CHANNEL_OPEN: u8 = 90;
const SSH_MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const SSH_MSG_CHANNEL_DATA: u8 = 94;
const SSH_MSG_CHANNEL_EOF: u8 = 96;
const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
const SSH_MSG_CHANNEL_REQUEST: u8 = 98;
const SSH_MSG_CHANNEL_SUCCESS: u8 = 99;
const SSH_MSG_CHANNEL_FAILURE: u8 = 100;

// Algorithm names
const KEX_ALGO: &str = "curve25519-sha256";
const HOST_KEY_ALGO: &str = "ssh-ed25519";
const CIPHER_ALGO: &str = "aes128-ctr";
const MAC_ALGO: &str = "hmac-sha2-256";
const COMPRESS_ALGO: &str = "none";

// ============================================================================
// Shared Host Key (for all sessions)
// ============================================================================

static HOST_KEY: Spinlock<Option<SigningKey>> = Spinlock::new(None);

/// Initialize the shared host key (call once at startup)
pub fn init_host_key() {
    let mut guard = HOST_KEY.lock();
    if guard.is_none() {
        let mut rng = SimpleRng::new();
        let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
        rng.fill_bytes(&mut key_bytes);
        *guard = Some(SigningKey::from_bytes(&key_bytes));
        log("[SSH] Host key initialized\n");
    }
}

/// Get a clone of the shared host key
fn get_host_key() -> Option<SigningKey> {
    HOST_KEY.lock().clone()
}

// ============================================================================
// SSH State Machine
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshState {
    AwaitingVersion,
    AwaitingKexInit,
    AwaitingKexEcdhInit,
    AwaitingNewKeys,
    AwaitingServiceRequest,
    AwaitingUserAuth,
    Authenticated,
    Disconnected,
}

// ============================================================================
// SSH Session (per-connection, no global state)
// ============================================================================

struct SshSession {
    state: SshState,
    rng: SimpleRng,
    client_version: Vec<u8>,
    server_version: Vec<u8>,
    client_kexinit: Vec<u8>,
    server_kexinit: Vec<u8>,
    session_id: [u8; 32],
    host_key: Option<SigningKey>,
    crypto: CryptoState,
    input_buffer: Vec<u8>,
    channel_open: bool,
    client_channel: u32,
    line_buffer: Vec<u8>,
}

impl SshSession {
    fn new() -> Self {
        Self {
            state: SshState::AwaitingVersion,
            rng: SimpleRng::new(),
            client_version: Vec::new(),
            server_version: SSH_VERSION[..SSH_VERSION.len() - 2].to_vec(),
            client_kexinit: Vec::new(),
            server_kexinit: Vec::new(),
            session_id: [0u8; 32],
            host_key: get_host_key(),
            crypto: CryptoState::new(),
            input_buffer: Vec::new(),
            channel_open: false,
            client_channel: 0,
            line_buffer: Vec::new(),
        }
    }
}

// ============================================================================
// KEXINIT Message
// ============================================================================

fn build_kexinit(rng: &mut SimpleRng) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(SSH_MSG_KEXINIT);

    let mut cookie = [0u8; 16];
    rng.fill_bytes(&mut cookie);
    payload.extend_from_slice(&cookie);

    write_namelist(&mut payload, &[KEX_ALGO]);
    write_namelist(&mut payload, &[HOST_KEY_ALGO]);
    write_namelist(&mut payload, &[CIPHER_ALGO]);
    write_namelist(&mut payload, &[CIPHER_ALGO]);
    write_namelist(&mut payload, &[MAC_ALGO]);
    write_namelist(&mut payload, &[MAC_ALGO]);
    write_namelist(&mut payload, &[COMPRESS_ALGO]);
    write_namelist(&mut payload, &[COMPRESS_ALGO]);
    write_namelist(&mut payload, &[]);
    write_namelist(&mut payload, &[]);
    payload.push(0);
    write_u32(&mut payload, 0);

    payload
}

// ============================================================================
// Key Exchange
// ============================================================================

fn handle_kex_ecdh_init(session: &mut SshSession, client_pubkey: &[u8]) -> Option<Vec<u8>> {
    // Generate server ephemeral key pair using X25519
    let mut secret_bytes = [0u8; 32];
    session.rng.fill_bytes(&mut secret_bytes);

    let server_secret = x25519_dalek::StaticSecret::from(secret_bytes);
    let server_public = X25519PublicKey::from(&server_secret);
    let server_pubkey = server_public.as_bytes();

    // Parse client's X25519 public key
    let client_pubkey_bytes: [u8; 32] = client_pubkey.try_into().ok()?;
    let client_public = X25519PublicKey::from(client_pubkey_bytes);

    // Compute shared secret via ECDH
    let shared_secret_point = server_secret.diffie_hellman(&client_public);
    let shared_secret = shared_secret_point.as_bytes().to_vec();

    let host_key = session.host_key.as_ref()?;
    let host_pubkey = host_key.verifying_key().to_bytes();

    let mut host_key_blob = Vec::new();
    write_string(&mut host_key_blob, b"ssh-ed25519");
    write_string(&mut host_key_blob, &host_pubkey);

    let mut hash_data = Vec::new();
    write_string(&mut hash_data, &session.client_version);
    write_string(&mut hash_data, &session.server_version);
    write_string(&mut hash_data, &session.client_kexinit);
    write_string(&mut hash_data, &session.server_kexinit);
    write_string(&mut hash_data, &host_key_blob);
    write_string(&mut hash_data, client_pubkey);
    write_string(&mut hash_data, server_pubkey);

    // K as mpint
    if !shared_secret.is_empty() && shared_secret[0] & 0x80 != 0 {
        write_u32(&mut hash_data, (shared_secret.len() + 1) as u32);
        hash_data.push(0);
    } else {
        write_u32(&mut hash_data, shared_secret.len() as u32);
    }
    hash_data.extend_from_slice(&shared_secret);

    let mut hasher = Sha256::new();
    hasher.update(&hash_data);
    let exchange_hash: [u8; 32] = hasher.finalize().into();

    if session.session_id == [0u8; 32] {
        session.session_id = exchange_hash;
    }

    let signature = host_key.sign(&exchange_hash);
    let mut sig_blob = Vec::new();
    write_string(&mut sig_blob, b"ssh-ed25519");
    write_string(&mut sig_blob, signature.to_bytes().as_slice());

    // Derive encryption keys
    let iv_c2s = derive_key(
        &shared_secret,
        &exchange_hash,
        b'A',
        &session.session_id,
        AES_IV_SIZE,
    );
    let iv_s2c = derive_key(
        &shared_secret,
        &exchange_hash,
        b'B',
        &session.session_id,
        AES_IV_SIZE,
    );
    let key_c2s = derive_key(
        &shared_secret,
        &exchange_hash,
        b'C',
        &session.session_id,
        AES_KEY_SIZE,
    );
    let key_s2c = derive_key(
        &shared_secret,
        &exchange_hash,
        b'D',
        &session.session_id,
        AES_KEY_SIZE,
    );
    let mac_c2s = derive_key(
        &shared_secret,
        &exchange_hash,
        b'E',
        &session.session_id,
        MAC_KEY_SIZE,
    );
    let mac_s2c = derive_key(
        &shared_secret,
        &exchange_hash,
        b'F',
        &session.session_id,
        MAC_KEY_SIZE,
    );

    use ctr::cipher::KeyIvInit;
    session.crypto.decrypt_cipher = Some(Aes128Ctr::new(
        key_c2s[..AES_KEY_SIZE].try_into().unwrap(),
        iv_c2s[..AES_IV_SIZE].try_into().unwrap(),
    ));
    session
        .crypto
        .decrypt_mac_key
        .copy_from_slice(&mac_c2s[..MAC_KEY_SIZE]);

    session.crypto.encrypt_cipher = Some(Aes128Ctr::new(
        key_s2c[..AES_KEY_SIZE].try_into().unwrap(),
        iv_s2c[..AES_IV_SIZE].try_into().unwrap(),
    ));
    session
        .crypto
        .encrypt_mac_key
        .copy_from_slice(&mac_s2c[..MAC_KEY_SIZE]);

    // Build KEX_ECDH_REPLY
    let mut reply = Vec::new();
    reply.push(SSH_MSG_KEX_ECDH_REPLY);
    write_string(&mut reply, &host_key_blob);
    write_string(&mut reply, server_pubkey);
    write_string(&mut reply, &sig_blob);

    Some(reply)
}

// ============================================================================
// Async Packet Sending
// ============================================================================

async fn send_raw(stream: &mut TcpStream, data: &[u8]) -> Result<(), TcpError> {
    stream.write_all(data).await
}

async fn send_unencrypted_packet(
    stream: &mut TcpStream,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), TcpError> {
    let packet = build_packet(payload);
    session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
    send_raw(stream, &packet).await
}

async fn send_encrypted_packet(
    stream: &mut TcpStream,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), TcpError> {
    if let Some(cipher) = session.crypto.encrypt_cipher.as_mut() {
        let seq = session.crypto.encrypt_seq;
        session.crypto.encrypt_seq = seq.wrapping_add(1);
        let packet = build_encrypted_packet(payload, cipher, &session.crypto.encrypt_mac_key, seq);
        send_raw(stream, &packet).await
    } else {
        Ok(())
    }
}

async fn send_packet(
    stream: &mut TcpStream,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<(), TcpError> {
    if session.crypto.encrypt_cipher.is_some() && session.state != SshState::AwaitingNewKeys {
        send_encrypted_packet(stream, payload, session).await
    } else {
        send_unencrypted_packet(stream, payload, session).await
    }
}

// ============================================================================
// Shell Handling
// ============================================================================

async fn send_channel_data(
    stream: &mut TcpStream,
    session: &mut SshSession,
    data: &[u8],
) -> Result<(), TcpError> {
    if !session.channel_open {
        return Ok(());
    }
    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, session.client_channel);
    write_string(&mut payload, data);
    send_packet(stream, &payload, session).await
}

async fn handle_shell_input(
    stream: &mut TcpStream,
    session: &mut SshSession,
    data: &[u8],
) -> Result<bool, TcpError> {
    for &byte in data {
        match byte {
            b'\r' | b'\n' => {
                let line = session.line_buffer.clone();
                session.line_buffer.clear();

                send_channel_data(stream, session, b"\r\n").await?;

                if !line.is_empty() {
                    let response = shell::execute_command(&line);
                    if !response.is_empty() {
                        send_channel_data(stream, session, &response).await?;
                    }

                    if shell::is_quit_command(&line) {
                        let mut close = vec![SSH_MSG_CHANNEL_CLOSE];
                        write_u32(&mut close, session.client_channel);
                        send_packet(stream, &close, session).await?;
                        session.channel_open = false;
                        session.state = SshState::Disconnected;
                        return Ok(true); // Signal disconnect
                    }
                }

                send_channel_data(stream, session, b"akuma> ").await?;
            }
            0x7F | 0x08 => {
                if !session.line_buffer.is_empty() {
                    session.line_buffer.pop();
                    send_channel_data(stream, session, b"\x08 \x08").await?;
                }
            }
            0x03 => {
                session.line_buffer.clear();
                send_channel_data(stream, session, b"^C\r\n").await?;
                send_channel_data(stream, session, b"akuma> ").await?;
            }
            0x04 => {
                if session.line_buffer.is_empty() {
                    send_channel_data(stream, session, b"\r\nGoodbye!\r\n").await?;
                    let mut close = vec![SSH_MSG_CHANNEL_CLOSE];
                    write_u32(&mut close, session.client_channel);
                    send_packet(stream, &close, session).await?;
                    session.channel_open = false;
                    session.state = SshState::Disconnected;
                    return Ok(true); // Signal disconnect
                }
            }
            _ if byte >= 0x20 && byte < 0x7F => {
                session.line_buffer.push(byte);
                send_channel_data(stream, session, &[byte]).await?;
            }
            _ => {}
        }
    }
    Ok(false)
}

// ============================================================================
// Message Handlers
// ============================================================================

async fn handle_message(
    stream: &mut TcpStream,
    msg_type: u8,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<bool, TcpError> {
    log(&alloc::format!(
        "[SSH] Received message type {}\n",
        msg_type
    ));

    match msg_type {
        SSH_MSG_KEXINIT => {
            let mut full = vec![SSH_MSG_KEXINIT];
            full.extend_from_slice(payload);
            session.client_kexinit = full;

            let kexinit = build_kexinit(&mut session.rng);
            session.server_kexinit = kexinit.clone();

            send_unencrypted_packet(stream, &kexinit, session).await?;
            session.state = SshState::AwaitingKexEcdhInit;
        }

        SSH_MSG_KEX_ECDH_INIT => {
            let mut offset = 0;
            if let Some(client_pubkey) = read_string(payload, &mut offset) {
                if let Some(reply) = handle_kex_ecdh_init(session, client_pubkey) {
                    send_unencrypted_packet(stream, &reply, session).await?;

                    let newkeys = vec![SSH_MSG_NEWKEYS];
                    send_unencrypted_packet(stream, &newkeys, session).await?;
                    session.state = SshState::AwaitingNewKeys;
                } else {
                    log("[SSH] KEX failed\n");
                }
            }
        }

        SSH_MSG_NEWKEYS => {
            log("[SSH] Encryption activated\n");
            session.state = SshState::AwaitingServiceRequest;
        }

        SSH_MSG_SERVICE_REQUEST => {
            let mut offset = 0;
            if let Some(service) = read_string(payload, &mut offset) {
                log(&alloc::format!(
                    "[SSH] Service request: {:?}\n",
                    core::str::from_utf8(service)
                ));

                let mut reply = vec![SSH_MSG_SERVICE_ACCEPT];
                write_string(&mut reply, service);
                send_packet(stream, &reply, session).await?;
                session.state = SshState::AwaitingUserAuth;
            }
        }

        SSH_MSG_USERAUTH_REQUEST => {
            let reply = vec![SSH_MSG_USERAUTH_SUCCESS];
            send_packet(stream, &reply, session).await?;
            session.state = SshState::Authenticated;
            log("[SSH] User authenticated\n");
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
                write_u32(&mut reply, 0x100000);
                write_u32(&mut reply, 0x4000);
                send_packet(stream, &reply, session).await?;

                log("[SSH] Channel opened\n");
            }
        }

        SSH_MSG_CHANNEL_REQUEST => {
            let mut offset = 0;
            let _recipient = read_u32(payload, &mut offset);
            let request_type = read_string(payload, &mut offset);
            let want_reply = if offset < payload.len() {
                payload[offset] != 0
            } else {
                false
            };

            if let Some(req_type) = request_type {
                log(&alloc::format!(
                    "[SSH] Channel request: {:?}\n",
                    core::str::from_utf8(req_type)
                ));

                let success = matches!(req_type, b"pty-req" | b"shell" | b"env" | b"exec");

                if want_reply {
                    let msg_type = if success {
                        SSH_MSG_CHANNEL_SUCCESS
                    } else {
                        SSH_MSG_CHANNEL_FAILURE
                    };
                    let mut full_reply = vec![msg_type];
                    write_u32(&mut full_reply, session.client_channel);
                    send_packet(stream, &full_reply, session).await?;
                }

                if req_type == b"shell" {
                    send_channel_data(stream, session, b"\r\n=================================\r\n")
                        .await?;
                    send_channel_data(stream, session, b"  Welcome to Akuma SSH Server\r\n")
                        .await?;
                    send_channel_data(stream, session, b"=================================\r\n\r\n")
                        .await?;
                    send_channel_data(
                        stream,
                        session,
                        b"Type 'help' for available commands.\r\n\r\n",
                    )
                    .await?;
                    send_channel_data(stream, session, b"akuma> ").await?;
                } else if req_type == b"exec" {
                    // Handle exec request - extract and execute the command
                    offset += 1; // skip want_reply byte
                    if let Some(cmd_bytes) = read_string(payload, &mut offset) {
                        log(&alloc::format!(
                            "[SSH] Exec command: {:?}\n",
                            core::str::from_utf8(cmd_bytes)
                        ));
                        let response = shell::execute_command(cmd_bytes);
                        if !response.is_empty() {
                            send_channel_data(stream, session, &response).await?;
                        }
                        send_channel_data(stream, session, b"\n").await?;
                    }
                    // Send EOF after exec - client will send CLOSE, we respond to that
                    let mut eof = vec![SSH_MSG_CHANNEL_EOF];
                    write_u32(&mut eof, session.client_channel);
                    send_packet(stream, &eof, session).await?;
                }
            }
        }

        SSH_MSG_CHANNEL_DATA => {
            let mut offset = 0;
            let _recipient = read_u32(payload, &mut offset);
            if let Some(data) = read_string(payload, &mut offset) {
                if handle_shell_input(stream, session, data).await? {
                    return Ok(true); // Disconnect requested
                }
            }
        }

        SSH_MSG_CHANNEL_EOF | SSH_MSG_CHANNEL_CLOSE => {
            log("[SSH] Channel close requested\n");
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
            log("[SSH] Client disconnected\n");
            session.state = SshState::Disconnected;
            return Ok(true);
        }

        SSH_MSG_IGNORE | SSH_MSG_DEBUG => {}

        _ => {
            log(&alloc::format!(
                "[SSH] Unhandled message type {}\n",
                msg_type
            ));
            let mut reply = vec![SSH_MSG_UNIMPLEMENTED];
            write_u32(&mut reply, session.crypto.decrypt_seq.wrapping_sub(1));
            send_packet(stream, &reply, session).await?;
        }
    }

    Ok(false)
}

// ============================================================================
// Packet Processing
// ============================================================================

fn process_encrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    // Need at least 4 bytes for packet length
    if session.input_buffer.len() < 4 {
        return None;
    }

    let cipher = session.crypto.decrypt_cipher.as_mut()?;

    // Clone cipher to peek at packet length without advancing the real cipher
    use ctr::cipher::StreamCipher;
    let mut peek_cipher = cipher.clone();

    // Decrypt first 4 bytes to get packet length
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&session.input_buffer[..4]);
    peek_cipher.apply_keystream(&mut len_buf);
    let packet_len = u32::from_be_bytes(len_buf) as usize;

    // Total size needed: 4 (length) + packet_len + MAC_SIZE
    let total_needed = 4 + packet_len + MAC_SIZE;
    if session.input_buffer.len() < total_needed {
        return None;
    }

    // We have enough data - now decrypt for real
    let encrypted_data = &session.input_buffer[..4 + packet_len];
    let received_mac = &session.input_buffer[4 + packet_len..total_needed];

    // Decrypt the packet
    let mut decrypted = encrypted_data.to_vec();
    cipher.apply_keystream(&mut decrypted);

    // Verify MAC: MAC(key, sequence_number || unencrypted_packet)
    let seq = session.crypto.decrypt_seq;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&session.crypto.decrypt_mac_key).ok()?;
    mac.update(&seq.to_be_bytes());
    mac.update(&decrypted);

    if mac.verify_slice(received_mac).is_err() {
        log(&alloc::format!(
            "[SSH] MAC verification failed (seq={}, pkt_len={}, buf_len={})\n",
            seq,
            packet_len,
            session.input_buffer.len()
        ));
        return None;
    }

    session.crypto.decrypt_seq = seq.wrapping_add(1);

    // Parse packet
    let padding_len = decrypted[4] as usize;
    let payload_len = packet_len - padding_len - 1;

    if 5 + payload_len > decrypted.len() {
        return None;
    }

    let msg_type = decrypted[5];
    let payload = decrypted[6..5 + payload_len].to_vec();

    // Remove processed packet from buffer
    session.input_buffer = session.input_buffer[total_needed..].to_vec();

    Some((msg_type, payload))
}

fn process_unencrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
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

// ============================================================================
// Async Connection Handler (per-connection)
// ============================================================================

/// Handle a single SSH connection asynchronously
/// Each connection gets its own SshSession - no global state
pub async fn handle_connection(mut stream: TcpStream) {
    log("[SSH] New SSH connection\n");

    let mut session = SshSession::new();

    // Send our version
    if send_raw(&mut stream, SSH_VERSION).await.is_err() {
        log("[SSH] Failed to send version\n");
        return;
    }

    // Main receive loop
    let mut buf = [0u8; 512];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => {
                log("[SSH] Connection closed by peer\n");
                break;
            }
            Ok(n) => {
                session.input_buffer.extend_from_slice(&buf[..n]);

                // Handle version exchange
                if session.state == SshState::AwaitingVersion {
                    if let Some(pos) = session.input_buffer.iter().position(|&b| b == b'\n') {
                        let version_line = session.input_buffer[..pos].to_vec();
                        session.input_buffer = session.input_buffer[pos + 1..].to_vec();

                        let version = if version_line.ends_with(b"\r") {
                            version_line[..version_line.len() - 1].to_vec()
                        } else {
                            version_line
                        };

                        session.client_version = version;
                        session.state = SshState::AwaitingKexInit;
                        log("[SSH] Client version received\n");
                    }
                    continue;
                }

                // Process packets
                loop {
                    let use_encryption = !matches!(
                        session.state,
                        SshState::AwaitingNewKeys
                            | SshState::AwaitingKexInit
                            | SshState::AwaitingKexEcdhInit
                    );

                    let packet = if use_encryption {
                        process_encrypted_packet(&mut session)
                    } else {
                        process_unencrypted_packet(&mut session)
                    };

                    match packet {
                        Some((msg_type, payload)) => {
                            match handle_message(&mut stream, msg_type, &payload, &mut session)
                                .await
                            {
                                Ok(true) => {
                                    // Disconnect requested
                                    return;
                                }
                                Ok(false) => {}
                                Err(_) => {
                                    log("[SSH] Error handling message\n");
                                    return;
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
            Err(_) => {
                log("[SSH] Read error\n");
                break;
            }
        }
    }

    log("[SSH] Connection ended\n");
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
