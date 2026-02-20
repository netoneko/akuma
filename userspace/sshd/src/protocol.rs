//! SSH-2 Protocol Implementation (Userspace)

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use alloc::string::String;
use core::convert::TryInto;

use ed25519_dalek::{SigningKey, Signer};
use embedded_io_async::{Read, Write};
use hmac::Mac;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;

use super::auth::{self, AuthResult};
use super::config::SshdConfig;
use super::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, HmacSha256, MAC_KEY_SIZE, MAC_SIZE,
    SimpleRng, build_encrypted_packet, build_packet, derive_key, read_string, read_u32,
    write_namelist, write_string, write_u32,
};
use super::keys;
use crate::SshStream;
use libakuma::*;
use libakuma::net::Error as NetError;

// Use our ported shell
use crate::shell::{self, CommandRegistry, ShellContext, create_default_registry};

// ============================================================================
// SSH Constants
// ============================================================================

const SSH_VERSION: &[u8] = b"SSH-2.0-Akuma_0.1_User\r\n";

const SSH_MSG_DISCONNECT: u8 = 1;
const SSH_MSG_SERVICE_REQUEST: u8 = 5;
const SSH_MSG_SERVICE_ACCEPT: u8 = 6;
const SSH_MSG_KEXINIT: u8 = 20;
const SSH_MSG_NEWKEYS: u8 = 21;
const SSH_MSG_KEX_ECDH_INIT: u8 = 30;
const SSH_MSG_KEX_ECDH_REPLY: u8 = 31;
const SSH_MSG_USERAUTH_REQUEST: u8 = 50;
const SSH_MSG_CHANNEL_OPEN: u8 = 90;
const SSH_MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const SSH_MSG_CHANNEL_DATA: u8 = 94;
const SSH_MSG_CHANNEL_EOF: u8 = 96;
const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
const SSH_MSG_CHANNEL_REQUEST: u8 = 98;
const SSH_MSG_CHANNEL_SUCCESS: u8 = 99;

const KEX_ALGO: &str = "curve25519-sha256";
const HOST_KEY_ALGO: &str = "ssh-ed25519";
const CIPHER_ALGO: &str = "aes128-ctr";
const MAC_ALGO: &str = "hmac-sha2-256";
const COMPRESS_ALGO: &str = "none";

// ============================================================================
// SSH Session
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
}

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
    config: SshdConfig,
}

impl SshSession {
    fn new(config: SshdConfig) -> Self {
        Self {
            state: SshState::AwaitingVersion,
            rng: SimpleRng::new(),
            client_version: Vec::new(),
            server_version: SSH_VERSION[..SSH_VERSION.len() - 2].to_vec(),
            client_kexinit: Vec::new(),
            server_kexinit: Vec::new(),
            session_id: [0u8; 32],
            host_key: keys::get_host_key(),
            crypto: CryptoState::new(),
            input_buffer: Vec::new(),
            channel_open: false,
            client_channel: 0,
            config,
        }
    }
}

// ============================================================================
// Shell Handling
// ============================================================================

async fn run_shell_session(
    stream: &mut SshStream,
    session: &mut SshSession,
) -> Result<(), NetError> {
    if let Some(ref shell_path) = session.config.shell {
        println(&format!("[SSH] Spawning shell: {}", shell_path));
        if let Some(res) = spawn(shell_path, None) {
            return bridge_process(stream, session, res.pid, res.stdout_fd).await;
        }
        println(&format!("[SSH] Failed to spawn {}, falling back to built-in", shell_path));
    }

    run_built_in_shell(stream, session).await
}

async fn bridge_process(
    stream: &mut SshStream,
    session: &mut SshSession,
    pid: u32,
    stdout_fd: u32,
) -> Result<(), NetError> {
    let mut buf = [0u8; 1024];
    let stdin_path = format!("/proc/{}/fd/0", pid);
    
    loop {
        // 1. Check for process exit
        if let Some((_, _exit_code)) = waitpid(pid) { break; }
        
        // 2. Output from process to SSH
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 { send_channel_data(stream, session, &buf[..n as usize]).await?; }

        // 3. Input from SSH to process
        let mut ssh_buf = [0u8; 512];
        match stream.read(&mut ssh_buf).await {
            Ok(0) => break,
            Ok(n) => {
                session.input_buffer.extend_from_slice(&ssh_buf[..n]);
                while let Some((msg_type, payload)) = process_encrypted_packet(session) {
                    if msg_type == SSH_MSG_CHANNEL_DATA {
                        let mut offset = 0;
                        let _recipient = read_u32(&payload, &mut offset);
                        if let Some(data) = read_string(&payload, &mut offset) {
                            // Forward to process stdin via procfs
                            let fd = open(&stdin_path, open_flags::O_WRONLY);
                            if fd >= 0 {
                                write_fd(fd, data);
                                close(fd);
                            }
                        }
                    } else if msg_type == SSH_MSG_CHANNEL_EOF || msg_type == SSH_MSG_CHANNEL_CLOSE {
                        return Ok(());
                    }
                }
            }
            Err(_) => {}
        }
        sleep_ms(10);
    }
    Ok(())
}

async fn run_built_in_shell(
    stream: &mut SshStream,
    session: &mut SshSession,
) -> Result<(), NetError> {
    let welcome = b"\r\nWelcome to Akuma SSH Built-in Shell\r\nType 'help' for commands.\r\n";
    send_channel_data(stream, session, welcome).await?;
    
    let mut line = Vec::new();
    let mut shell_ctx = ShellContext::new();
    let registry = create_default_registry();

    loop {
        let prompt = format!("akuma:{}> ", shell_ctx.cwd());
        send_channel_data(stream, session, prompt.as_bytes()).await?;
        
        line.clear();
        'read_loop: loop {
            let mut b = [0u8; 1];
            if stream.read(&mut b).await? == 0 { return Ok(()); }
            
            session.input_buffer.extend_from_slice(&b);
            while let Some((msg_type, payload)) = process_encrypted_packet(session) {
                if msg_type == SSH_MSG_CHANNEL_DATA {
                    let mut offset = 0;
                    let _recipient = read_u32(&payload, &mut offset);
                    if let Some(data) = read_string(&payload, &mut offset) {
                        for &byte in data {
                            if byte == b'\r' || byte == b'\n' {
                                send_channel_data(stream, session, b"\r\n").await?;
                                break 'read_loop;
                            } else if byte == 8 || byte == 127 {
                                if !line.is_empty() {
                                    line.pop();
                                    send_channel_data(stream, session, b"\x08 \x08").await?;
                                }
                            } else if byte >= 32 {
                                line.push(byte);
                                send_channel_data(stream, session, &[byte]).await?;
                            }
                        }
                    }
                } else if msg_type == SSH_MSG_DISCONNECT {
                    return Ok(());
                }
            }
        }

        if line.is_empty() { continue; }

        let res = shell::execute_command_chain(&line, &registry, &mut shell_ctx).await;
        if !res.output.is_empty() {
            send_channel_data(stream, session, &res.output).await?;
        }
        if res.should_exit {
            send_channel_data(stream, session, b"Goodbye!\r\n").await?;
            break;
        }
    }
    Ok(())
}

// ============================================================================
// Message Handlers
// ============================================================================

enum MessageResult { Continue, StartShell, Disconnect }

async fn handle_message(
    stream: &mut SshStream,
    msg_type: u8,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<MessageResult, NetError> {
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
                }
            }
        }
        SSH_MSG_NEWKEYS => { session.state = SshState::AwaitingServiceRequest; }
        SSH_MSG_SERVICE_REQUEST => {
            let mut offset = 0;
            if let Some(service) = read_string(payload, &mut offset) {
                let mut reply = vec![SSH_MSG_SERVICE_ACCEPT];
                write_string(&mut reply, service);
                send_packet(stream, &reply, session).await?;
                session.state = SshState::AwaitingUserAuth;
            }
        }
        SSH_MSG_USERAUTH_REQUEST => {
            let (result, reply) = auth::handle_userauth_request(payload, &session.session_id, &session.config).await;
            send_packet(stream, &reply, session).await?;
            if let AuthResult::Success = result { session.state = SshState::Authenticated; }
        }
        SSH_MSG_CHANNEL_OPEN => {
            let mut offset = 0;
            let _type = read_string(payload, &mut offset);
            let sender = read_u32(payload, &mut offset).unwrap_or(0);
            session.client_channel = sender;
            session.channel_open = true;
            let mut reply = vec![SSH_MSG_CHANNEL_OPEN_CONFIRMATION];
            write_u32(&mut reply, sender);
            write_u32(&mut reply, 0);
            write_u32(&mut reply, 0x100000);
            write_u32(&mut reply, 0x4000);
            send_packet(stream, &reply, session).await?;
        }
        SSH_MSG_CHANNEL_REQUEST => {
            let mut offset = 0;
            let _recipient = read_u32(payload, &mut offset);
            let req_type = read_string(payload, &mut offset).unwrap_or(b"");
            let want_reply = if offset < payload.len() { payload[offset] != 0 } else { false };
            if want_reply {
                let mut full_reply = vec![SSH_MSG_CHANNEL_SUCCESS];
                write_u32(&mut full_reply, session.client_channel);
                send_packet(stream, &full_reply, session).await?;
            }
            if req_type == b"shell" { return Ok(MessageResult::StartShell); }
        }
        SSH_MSG_DISCONNECT => return Ok(MessageResult::Disconnect),
        _ => {}
    }
    Ok(MessageResult::Continue)
}

// ============================================================================
// Packet Helpers
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

fn handle_kex_ecdh_init(session: &mut SshSession, client_pubkey: &[u8]) -> Option<Vec<u8>> {
    let mut secret_bytes = [0u8; 32];
    session.rng.fill_bytes(&mut secret_bytes);
    let server_secret = x25519_dalek::StaticSecret::from(secret_bytes);
    let server_public = X25519PublicKey::from(&server_secret);
    let server_pubkey = server_public.as_bytes();
    let client_pubkey_bytes: [u8; 32] = client_pubkey.try_into().ok()?;
    let client_public = X25519PublicKey::from(client_pubkey_bytes);
    let shared_secret = server_secret.diffie_hellman(&client_public).as_bytes().to_vec();
    let host_key = session.host_key.as_ref()?;
    let mut host_key_blob = Vec::new();
    write_string(&mut host_key_blob, b"ssh-ed25519");
    write_string(&mut host_key_blob, host_key.verifying_key().as_bytes());
    let mut hash_data = Vec::new();
    write_string(&mut hash_data, &session.client_version);
    write_string(&mut hash_data, &session.server_version);
    write_string(&mut hash_data, &session.client_kexinit);
    write_string(&mut hash_data, &session.server_kexinit);
    write_string(&mut hash_data, &host_key_blob);
    write_string(&mut hash_data, client_pubkey);
    write_string(&mut hash_data, server_pubkey);
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
    if session.session_id == [0u8; 32] { session.session_id = exchange_hash; }
    let signature = host_key.sign(&exchange_hash);
    let mut sig_blob = Vec::new();
    write_string(&mut sig_blob, b"ssh-ed25519");
    write_string(&mut sig_blob, signature.to_bytes().as_slice());
    let iv_c2s = derive_key(&shared_secret, &exchange_hash, b'A', &session.session_id, AES_IV_SIZE);
    let iv_s2c = derive_key(&shared_secret, &exchange_hash, b'B', &session.session_id, AES_IV_SIZE);
    let key_c2s = derive_key(&shared_secret, &exchange_hash, b'C', &session.session_id, AES_KEY_SIZE);
    let key_s2c = derive_key(&shared_secret, &exchange_hash, b'D', &session.session_id, AES_KEY_SIZE);
    let mac_c2s = derive_key(&shared_secret, &exchange_hash, b'E', &session.session_id, MAC_KEY_SIZE);
    let mac_s2c = derive_key(&shared_secret, &exchange_hash, b'F', &session.session_id, MAC_KEY_SIZE);
    use ctr::cipher::KeyIvInit;
    session.crypto.decrypt_cipher = Some(Aes128Ctr::new(key_c2s[..AES_KEY_SIZE].try_into().unwrap(), iv_c2s[..AES_IV_SIZE].try_into().unwrap()));
    session.crypto.decrypt_mac_key.copy_from_slice(&mac_c2s[..MAC_KEY_SIZE]);
    session.crypto.encrypt_cipher = Some(Aes128Ctr::new(key_s2c[..AES_KEY_SIZE].try_into().unwrap(), iv_s2c[..AES_IV_SIZE].try_into().unwrap()));
    session.crypto.encrypt_mac_key.copy_from_slice(&mac_s2c[..MAC_KEY_SIZE]);
    let mut reply = Vec::new();
    reply.push(SSH_MSG_KEX_ECDH_REPLY);
    write_string(&mut reply, &host_key_blob);
    write_string(&mut reply, server_pubkey);
    write_string(&mut reply, &sig_blob);
    Some(reply)
}

async fn send_packet(stream: &mut SshStream, payload: &[u8], session: &mut SshSession) -> Result<(), NetError> {
    if session.crypto.encrypt_cipher.is_some() && session.state != SshState::AwaitingNewKeys {
        let seq = session.crypto.encrypt_seq;
        session.crypto.encrypt_seq = seq.wrapping_add(1);
        let packet = build_encrypted_packet(payload, session.crypto.encrypt_cipher.as_mut().unwrap(), &session.crypto.encrypt_mac_key, seq);
        stream.write_all(&packet).await
    } else {
        let packet = build_packet(payload);
        session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
        stream.write_all(&packet).await
    }
}

async fn send_unencrypted_packet(stream: &mut SshStream, payload: &[u8], session: &mut SshSession) -> Result<(), NetError> {
    let packet = build_packet(payload);
    session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
    stream.write_all(&packet).await
}

async fn send_channel_data(stream: &mut SshStream, session: &mut SshSession, data: &[u8]) -> Result<(), NetError> {
    if !session.channel_open { return Ok(()); }
    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, session.client_channel);
    write_string(&mut payload, data);
    send_packet(stream, &payload, session).await
}

fn process_encrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 4 { return None; }
    let cipher = session.crypto.decrypt_cipher.as_mut()?;
    use ctr::cipher::StreamCipher;
    let mut peek_cipher = cipher.clone();
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&session.input_buffer[..4]);
    peek_cipher.apply_keystream(&mut len_buf);
    let packet_len = u32::from_be_bytes(len_buf) as usize;
    let total_needed = 4 + packet_len + MAC_SIZE;
    if session.input_buffer.len() < total_needed { return None; }
    let encrypted_data = &session.input_buffer[..4 + packet_len];
    let received_mac = &session.input_buffer[4 + packet_len..total_needed];
    let mut decrypted = encrypted_data.to_vec();
    cipher.apply_keystream(&mut decrypted);
    let seq = session.crypto.decrypt_seq;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&session.crypto.decrypt_mac_key).ok()?;
    mac.update(&seq.to_be_bytes());
    mac.update(&decrypted);
    if mac.verify_slice(received_mac).is_err() { return None; }
    session.crypto.decrypt_seq = seq.wrapping_add(1);
    let padding_len = decrypted[4] as usize;
    let payload_len = packet_len - padding_len - 1;
    let msg_type = decrypted[5];
    let payload = decrypted[6..5 + payload_len].to_vec();
    session.input_buffer = session.input_buffer[total_needed..].to_vec();
    Some((msg_type, payload))
}

fn process_unencrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 5 { return None; }
    let packet_len = u32::from_be_bytes(session.input_buffer[..4].try_into().ok()?) as usize;
    let total_len = 4 + packet_len;
    if session.input_buffer.len() < total_len { return None; }
    let padding_len = session.input_buffer[4] as usize;
    let payload_len = packet_len - padding_len - 1;
    let msg_type = session.input_buffer[5];
    let payload = session.input_buffer[6..5 + payload_len].to_vec();
    session.crypto.decrypt_seq = session.crypto.decrypt_seq.wrapping_add(1);
    session.input_buffer = session.input_buffer[total_len..].to_vec();
    Some((msg_type, payload))
}

pub async fn handle_connection(mut stream: SshStream, config: SshdConfig) {
    let mut session = SshSession::new(config);
    let _ = stream.write_all(SSH_VERSION).await;
    
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                session.input_buffer.extend_from_slice(&buf[..n]);
                if session.state == SshState::AwaitingVersion {
                    if let Some(pos) = session.input_buffer.iter().position(|&b| b == b'\n') {
                        let line = session.input_buffer[..pos].to_vec();
                        session.input_buffer = session.input_buffer[pos+1..].to_vec();
                        session.client_version = if line.ends_with(b"\r") { line[..line.len()-1].to_vec() } else { line };
                        session.state = SshState::AwaitingKexInit;
                    }
                    continue;
                }
                
                while let Some((msg_type, payload)) = if !matches!(session.state, SshState::AwaitingNewKeys | SshState::AwaitingKexInit | SshState::AwaitingKexEcdhInit) {
                    process_encrypted_packet(&mut session)
                } else {
                    process_unencrypted_packet(&mut session)
                } {
                    match handle_message(&mut stream, msg_type, &payload, &mut session).await {
                        Ok(MessageResult::Continue) => {}
                        Ok(MessageResult::StartShell) => {
                            let _ = run_shell_session(&mut stream, &mut session).await;
                            return;
                        }
                        Ok(MessageResult::Disconnect) => return,
                        Err(_) => return,
                    }
                }
            }
            Err(_) => break,
        }
    }
}
