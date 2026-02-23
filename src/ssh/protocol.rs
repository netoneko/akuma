//! SSH-2 Protocol Implementation (Async Multi-Session)
//!
//! Implements a minimal SSH-2 server that works with real SSH clients.
//! Supports:
//! - curve25519-sha256 key exchange
//! - ssh-ed25519 host key
//! - aes128-ctr encryption
//! - hmac-sha2-256 MAC
//! - Accepts any authentication
//! - Shell with basic commands (via ShellSession)
//! - Multiple concurrent SSH sessions

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use alloc::sync::Arc; // Added
use spinning_top::Spinlock;use core::convert::TryInto;
use core::task::Waker;

use ed25519_dalek::{SECRET_KEY_LENGTH, Signer, SigningKey};
use embedded_io_async::{ErrorType, Read, Write};
use hmac::Mac;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;

use super::auth::{self, AuthResult};
use super::config::SshdConfig;
use super::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, HmacSha256, MAC_KEY_SIZE, MAC_SIZE,
    SimpleRng, build_encrypted_packet, build_packet, derive_key, read_string, read_u32, trim_bytes,
    write_namelist, write_string, write_u32,
};
use super::keys;
use crate::smoltcp_net::{TcpError, TcpStream};
use crate::console;
use crate::shell::ShellContext;
use crate::shell::{self, commands::create_default_registry};
use crate::process::{self, Pid};
use crate::terminal::{self, mode_flags};
use crate::kernel_timer::Duration;

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
const SSH_MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const SSH_MSG_CHANNEL_DATA: u8 = 94;
const SSH_MSG_CHANNEL_EOF: u8 = 96;
const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
const SSH_MSG_CHANNEL_REQUEST: u8 = 98;

// ============================================================================
// SSH Timeouts
// ============================================================================

/// Timeout for initial handshake (version exchange, key exchange, auth)
const SSH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for idle connections (no data received)
/// Set to 5 minutes - clients should send keepalives
const SSH_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Timeout for shell input reads (shorter, to stay responsive)
const SSH_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Very short timeout for interactive polling reads
const SSH_INTERACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(10);
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

/// Initialize the shared host key (call once at startup)
/// This is a synchronous wrapper that generates a temporary key.
/// The full async key loading happens in init_host_key_async().
pub fn init_host_key() {
    // Generate a temporary key synchronously for backward compatibility
    // The proper key loading with filesystem persistence is done async
    let guard = keys::get_host_key();
    if guard.is_none() {
        let mut rng = SimpleRng::new();
        let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
        rng.fill_bytes(&mut key_bytes);
        let key = SigningKey::from_bytes(&key_bytes);
        keys::set_host_key(key);
        log("[SSH] Temporary host key initialized (will load from fs on first connection)\n");
    }
}

/// Initialize the shared host key asynchronously (loads from filesystem)
pub async fn init_host_key_async() {
    let _key = keys::load_or_generate_host_key().await;
    log("[SSH] Host key loaded/generated from filesystem\n");
}

/// Get a clone of the shared host key
fn get_host_key() -> Option<SigningKey> {
    keys::get_host_key()
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
    /// Buffer for incoming channel data (for SshChannelStream to read from)
    channel_data_buffer: Vec<u8>,
    /// Flag to indicate channel EOF was received
    channel_eof: bool,
    /// Terminal width in columns (from pty-req)
    term_width: u32,
    /// Terminal height in rows (from pty-req)
    term_height: u32,
    /// Flag to indicate terminal was resized (triggers re-render)
    resize_pending: bool,
    /// SSH server configuration
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
            host_key: get_host_key(),
            crypto: CryptoState::new(),
            input_buffer: Vec::new(),
            channel_open: false,
            client_channel: 0,
            channel_data_buffer: Vec::new(),
            channel_eof: false,
            term_width: 80,  // default
            term_height: 24, // default
            resize_pending: false,
            config,
        }
    }
}

// ============================================================================
// SSH Channel Stream (embedded_io_async adapter)
// ============================================================================

/// Error type for SSH channel stream operations
#[derive(Debug)]
pub struct SshStreamError;

impl embedded_io_async::Error for SshStreamError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

/// A stream adapter that provides embedded_io_async Read/Write over an SSH channel
pub struct SshChannelStream<'a> {
    stream: &'a mut TcpStream,
    session: &'a mut SshSession,
    pub current_process_pid: Option<Pid>, // Made public
    /// The process channel for the currently active foreground process, if any.
    /// Used to check raw mode status and push input directly to it.
    pub current_process_channel: Option<Arc<crate::process::ProcessChannel>>, // Made public
}

impl<'a> SshChannelStream<'a> {
    fn new(stream: &'a mut TcpStream, session: &'a mut SshSession) -> Self {
        Self {
            stream,
            session,
            current_process_pid: None,
            current_process_channel: None,
        }
    }

    pub fn terminal_state(&self) -> Option<Arc<Spinlock<terminal::TerminalState>>> {
        crate::process::get_terminal_state(crate::threading::current_thread_id())
    }

    /// Read and process SSH packets until we have channel data or an error
    async fn read_until_channel_data(&mut self) -> Result<(), TcpError> {
        let mut buf = [0u8; 512];

        loop {
            // Check if we already have channel data or EOF
            if !self.session.channel_data_buffer.is_empty() || self.session.channel_eof {
                return Ok(());
            }

            // Read more data from the network with timeout
            let read_result = crate::kernel_timer::with_timeout(
                SSH_READ_TIMEOUT,
                self.stream.read(&mut buf)
            ).await;
            
            match read_result {
                Err(_timeout) => {
                    // Timeout - treat as EOF
                    self.session.channel_eof = true;
                    return Ok(());
                }
                Ok(Ok(0)) => {
                    self.session.channel_eof = true;
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e),
                Ok(Ok(n)) => {
                    self.session.input_buffer.extend_from_slice(&buf[..n]);

                    // Process any complete packets
                    loop {
                        let packet = process_encrypted_packet(self.session);
                        match packet {
                            Some((msg_type, payload)) => {
                                match self.handle_channel_message(msg_type, &payload).await {
                                    Ok(true) => {
                                        // Got channel data or EOF, can return
                                        return Ok(());
                                    }
                                    Ok(false) => {
                                        // Keep processing
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            None => break, // No more complete packets
                        }
                    }
                }
            }
        }
    }

    /// Try to read channel data with a very short timeout (for interactive mode)
    /// Returns the number of bytes read, or 0 if no data is available
    async fn try_read_interactive(&mut self, buf: &mut [u8]) -> Result<usize, TcpError> {
        // First check if we have buffered channel data
        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        // Check for EOF
        if self.session.channel_eof {
            return Ok(0);
        }

        // Try a very short timeout read from the network
        let mut tcp_buf = [0u8; 512];
        let read_result = crate::kernel_timer::with_timeout(
            SSH_INTERACTIVE_READ_TIMEOUT,
            self.stream.read(&mut tcp_buf)
        ).await;

        match read_result {
            Err(_timeout) => {
                // Timeout - no data available, but not EOF
                Ok(0)
            }
            Ok(Ok(0)) => {
                self.session.channel_eof = true;
                Ok(0)
            }
            Ok(Err(e)) => Err(e),
            Ok(Ok(n)) => {
                self.session.input_buffer.extend_from_slice(&tcp_buf[..n]);

                // Process any complete packets
                loop {
                    let packet = process_encrypted_packet(self.session);
                    match packet {
                        Some((msg_type, payload)) => {
                            let _ = self.handle_channel_message(msg_type, &payload).await;
                        }
                        None => break,
                    }
                }

                // Return any buffered data we got
                if !self.session.channel_data_buffer.is_empty() {
                    let len = buf.len().min(self.session.channel_data_buffer.len());
                    buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
                    self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
                    return Ok(len);
                }

                Ok(0)
            }
        }
    }

    /// Handle a single SSH message, return true if we got channel data or EOF
    async fn handle_channel_message(
        &mut self,
        msg_type: u8,
        payload: &[u8],
    ) -> Result<bool, TcpError> {
        match msg_type {
            SSH_MSG_CHANNEL_DATA => {
                let mut offset = 0;
                let _recipient = read_u32(payload, &mut offset);
                if let Some(data) = read_string(payload, &mut offset) {
                    self.session.channel_data_buffer.extend_from_slice(data);
                    return Ok(true);
                }
            }
            SSH_MSG_CHANNEL_REQUEST => {
                // Handle window-change requests during shell session
                let mut offset = 0;
                let _recipient = read_u32(payload, &mut offset);
                if let Some(req_type) = read_string(payload, &mut offset) {
                    if req_type == b"window-change" {
                        let _want_reply = if offset < payload.len() {
                            payload[offset] != 0
                        } else {
                            false
                        };
                        offset += 1;
                        // window-change format: uint32 width_cols, uint32 height_rows, uint32 width_px, uint32 height_px
                        if let Some(width) = read_u32(payload, &mut offset) {
                            if let Some(height) = read_u32(payload, &mut offset) {
                                self.session.term_width = width;
                                self.session.term_height = height;
                                self.session.resize_pending = true;
                                log(&format!("[SSH] Terminal resized: {}x{}\n", width, height));
                                // Return true to break out of read loop and trigger re-render
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            SSH_MSG_CHANNEL_EOF | SSH_MSG_CHANNEL_CLOSE => {
                log("[SSH] Channel close/EOF received\n");
                self.session.channel_eof = true;
                return Ok(true);
            }
            SSH_MSG_GLOBAL_REQUEST => {
                // Respond to global requests (e.g. keepalive@openssh.com)
                // so the SSH client doesn't time out during long-running processes
                let mut offset = 0;
                let _req_name = read_string(payload, &mut offset);
                let want_reply = if offset < payload.len() { payload[offset] != 0 } else { false };
                if want_reply {
                    let reply = alloc::vec![SSH_MSG_REQUEST_FAILURE];
                    let _ = send_packet(self.stream, &reply, self.session).await;
                }
            }
            SSH_MSG_CHANNEL_WINDOW_ADJUST => {
                // Client is adjusting its receive window; we don't enforce
                // flow control, so just silently consume the message.
            }
            SSH_MSG_IGNORE | SSH_MSG_DEBUG => {}
            SSH_MSG_DISCONNECT => {
                log("[SSH] Client disconnected\n");
                self.session.state = SshState::Disconnected;
                self.session.channel_eof = true;
                return Ok(true);
            }
            _ => {
                log(&format!(
                    "[SSH] Ignoring message type {} during shell\n",
                    msg_type
                ));
            }
        }
        Ok(false)
    }
}

impl ErrorType for SshChannelStream<'_> {
    type Error = SshStreamError;
}

impl crate::shell::InteractiveRead for SshChannelStream<'_> {
    async fn try_read_interactive(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.try_read_interactive(buf).await.map_err(|_| SshStreamError)
    }
}

impl crate::editor::TermSizeProvider for SshChannelStream<'_> {
    fn get_term_size(&self) -> crate::editor::TermSize {
        crate::editor::TermSize::new(self.session.term_width, self.session.term_height)
    }
}

/// Special byte used to signal a terminal resize event
pub const RESIZE_SIGNAL_BYTE: u8 = 0x00;

impl Read for SshChannelStream<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // Check for resize signal first
        if self.session.resize_pending {
            self.session.resize_pending = false;
            if !buf.is_empty() {
                buf[0] = RESIZE_SIGNAL_BYTE;
                return Ok(1);
            }
        }

        // If we have buffered channel data, return it
        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        // Check for EOF
        if self.session.channel_eof {
            return Ok(0);
        }

        // Read until we have channel data (or resize occurs)
        self.read_until_channel_data()
            .await
            .map_err(|_| SshStreamError)?;

        // Check for resize after read loop returns
        if self.session.resize_pending {
            self.session.resize_pending = false;
            if !buf.is_empty() {
                buf[0] = RESIZE_SIGNAL_BYTE;
                return Ok(1);
            }
        }

        // Try again with the newly buffered data
        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        // EOF
        Ok(0)
    }
}

/// Maximum chunk size for SSH channel data (conservative to avoid packet issues)
const SSH_CHANNEL_MAX_CHUNK: usize = 4096;

impl Write for SshChannelStream<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if !self.session.channel_open {
            return Err(SshStreamError);
        }

        // Send data in chunks to avoid packet size issues
        let mut sent = 0;
        while sent < buf.len() {
            let chunk_size = (buf.len() - sent).min(SSH_CHANNEL_MAX_CHUNK);
            let chunk = &buf[sent..sent + chunk_size];
            send_channel_data(self.stream, self.session, chunk)
                .await
                .map_err(|_| SshStreamError)?;
            sent += chunk_size;
        }

        // Auto-flush to ensure immediate transmission for interactive sessions
        // Use a timeout (10ms) to prevent blocking if the network is backed up
        let _ = crate::kernel_timer::with_timeout(
            SSH_INTERACTIVE_READ_TIMEOUT,
            self.flush()
        ).await;
        
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // Flush the underlying TCP stream to push data to network driver
        self.stream.flush().await.map_err(|_| SshStreamError)?;
        // Yield to give network runner a chance to transmit
        crate::threading::yield_now();
        Ok(())
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
    if data.is_empty() { return Ok(()); }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let sn_len = data.len().min(32);
        let mut snippet = [0u8; 32];
        let n = sn_len.min(snippet.len());
        snippet[..n].copy_from_slice(&data[..n]);
        for byte in &mut snippet[..n] {
            if *byte < 32 || *byte > 126 { *byte = b'.'; }
        }
        let snippet_str = core::str::from_utf8(&snippet[..n]).unwrap_or("...");
        log(&alloc::format!("[SSH] Sending {} bytes channel data \"{}\"\n", data.len(), snippet_str));
    }

    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, session.client_channel);
    write_string(&mut payload, data);
    send_packet(stream, &payload, session).await
}

async fn bridge_process(
    stream: &mut TcpStream,
    session: &mut SshSession,
    pid: u32,
    process_channel: Arc<crate::process::ProcessChannel>,
) -> Result<(), TcpError> {
    log(&format!("[SSH] Starting I/O bridge for PID {}\n", pid));
    let mut buf = [0u8; 1024];
    
    loop {
        // 1. Check for process exit
        if let Some((_, _exit_code)) = crate::process::waitpid(pid) { 
            log(&format!("[SSH] Process PID {} exited, ending bridge\n", pid));
            break; 
        }
        
        // 2. Output from process to SSH
        // Read directly from the process channel
        loop {
            let n = process_channel.read(&mut buf);
            if n == 0 { break; }
            send_channel_data(stream, session, &buf[..n]).await?;
        }

        // 3. Input from SSH to process
        let mut ssh_buf = [0u8; 512];
        // Use timeout to keep loop responsive
        let read_res = crate::kernel_timer::with_timeout(
            crate::kernel_timer::Duration::from_millis(10),
            stream.read(&mut ssh_buf)
        ).await;

        match read_res {
            Ok(Ok(n)) if n > 0 => {
                session.input_buffer.extend_from_slice(&ssh_buf[..n]);
                while let Some((msg_type, payload)) = process_encrypted_packet(session) {
                    if msg_type == SSH_MSG_CHANNEL_DATA {
                        let mut offset = 0;
                        let _recipient = read_u32(&payload, &mut offset);
                        if let Some(data) = read_string(&payload, &mut offset) {
                            // Forward directly to process stdin
                            let _ = crate::process::write_to_process_stdin(pid, data);
                        }
                    } else if msg_type == SSH_MSG_CHANNEL_EOF || msg_type == SSH_MSG_CHANNEL_CLOSE {
                        log("[SSH] Channel closed, ending bridge\n");
                        return Ok(());
                    }
                }
            }
            _ => {}
        }
        
        // 4. Handle terminal resizing
        if session.resize_pending {
            session.resize_pending = false;
            // TIOCGWINSZ will pick up session.term_width/height next time it's called
        }

        crate::threading::yield_now();
    }
    Ok(())
}

/// Escape sequence state machine for parsing ANSI escape codes
#[derive(Clone, Copy, PartialEq)]
enum EscapeState {
    Normal,
    Escape,  // Got ESC (0x1B)
    Bracket, // Got ESC [
}

/// Generate the shell prompt with current working directory
fn generate_prompt(ctx: &ShellContext) -> alloc::string::String {
    // let _ = crate::threading::dump_stack_info();
    format!("akuma:{}> ", ctx.cwd())
}

/// Run an interactive shell session
async fn run_shell_session(
    stream: &mut TcpStream,
    session: &mut SshSession,
) -> Result<(), TcpError> {
    log("[SSH] Starting shell session\n");

    // 0. Get shell config before borrowing session mutably
    let shell_path_opt = session.config.shell.clone();

    // Create per-session shell context (starts at /)
    let mut ctx = ShellContext::new();

    // Create the SSH channel stream adapter
    let mut channel_stream = SshChannelStream::new(stream, session);

    // Create shared terminal state for this session
    let terminal_state = Arc::new(Spinlock::new(terminal::TerminalState::default()));
    log(&format!("[SSH] Created shared terminal state at {:p}\n", Arc::as_ptr(&terminal_state)));

    // Register the channel and terminal state for this system thread so syscalls can find them
    let tid = crate::threading::current_thread_id();
    let channel = Arc::new(crate::process::ProcessChannel::new());
    crate::process::register_system_thread_channel(tid, channel.clone());
    crate::process::register_terminal_state(tid, terminal_state.clone());

    // 1. If an external shell is configured, spawn it and bridge
    if let Some(shell_path) = shell_path_opt {
        log(&format!("[SSH] Spawning external shell: {}\n", shell_path));
        // Use kernel spawn function directly
        if let Ok((_tid, proc_channel, pid)) = crate::process::spawn_process_with_channel(&shell_path, None, None) {
            return bridge_process(stream, session, pid, proc_channel).await;
        }
        log(&format!("[SSH] Failed to spawn external shell {}, falling back to built-in\n", shell_path));
    }

    // Create command registry
    let registry = create_default_registry();

    // Send welcome message
    {
        let welcome = b"\r\n=================================\r\n  Welcome to Akuma SSH Server\r\n=================================\r\n\r\nType 'help' for available commands.\r\n\r\n";
        let _ = channel_stream.write(welcome).await;
        let prompt = generate_prompt(&ctx);
        let _ = channel_stream.write(prompt.as_bytes()).await;
    }

    // Line buffer for input with cursor position
    let mut line_buffer: Vec<u8> = Vec::new();
    let mut cursor_pos: usize = 0;
    let mut read_buf = [0u8; 64];
    let mut escape_state = EscapeState::Normal;

    // Command history
    let mut history: Vec<Vec<u8>> = Vec::new();
    let mut history_index: usize = 0;
    let mut saved_line: Vec<u8> = Vec::new(); // Save current line when navigating history

    
    loop {
        // Read input
        match channel_stream.read(&mut read_buf).await {
            Ok(0) => {
                log("[SSH] Shell session ended (EOF)\n");
                break;
            }
            Ok(n) => {
                // Determine if the current foreground process is in raw mode
                let is_raw_mode = if let Some(channel) = &channel_stream.current_process_channel {
                    (*channel).is_raw_mode()
                } else {
                    false // No foreground process, assume cooked mode
                };

                if is_raw_mode {
                    // Raw mode: Pass input directly to the process's stdin buffer
                    // using unified helper (UNIFIED I/O)
                    if let Some(pid) = channel_stream.current_process_pid {
                        let _ = process::write_to_process_stdin(pid, &read_buf[..n]);
                    }
                    // No echo, no line editing in raw mode
                } else {
                    // Cooked mode (shell itself or process not in raw mode):
                    // Existing line editing and echoing logic
                    for &byte in &read_buf[..n] {
                        match escape_state {
                            EscapeState::Normal => {
                                match byte {
                                    0x1B => {
                                        // ESC - start of escape sequence
                                        escape_state = EscapeState::Escape;
                                    }
                                    b'\r' | b'\n' => {
                                        // Echo newline
                                        let _ = channel_stream.write(b"\r\n").await;

                                        // Process command
                                        let trimmed = trim_bytes(&line_buffer);
                                        if !trimmed.is_empty() {
                                            // Add to history
                                            history.push(line_buffer.clone());
                                            if history.len() > 50 {
                                                history.remove(0);
                                            }
                                            history_index = history.len();                                        

                                            // Check for neko editor command (special case - not part of command chain)
                                            if trimmed == b"neko" || trimmed.starts_with(b"neko ") {
                                                let filepath = if trimmed.len() > 5 {
                                                    let path_bytes = trim_bytes(&trimmed[5..]);
                                                    if path_bytes.is_empty() {
                                                        None
                                                    } else {
                                                        Some(
                                                            core::str::from_utf8(path_bytes)
                                                                .unwrap_or(""),
                                                        )
                                                    }
                                                } else {
                                                    None
                                                };

                                                if let Err(e) =
                                                    crate::editor::run(&mut channel_stream, filepath)
                                                        .await
                                                {
                                                    let msg = format!("Editor error: {}\r\n", e);
                                                    let _ = channel_stream.write(msg.as_bytes()).await;
                                                }

                                                line_buffer.clear();
                                                cursor_pos = 0;
                                                let prompt = generate_prompt(&ctx);
                                                let _ = channel_stream.write(prompt.as_bytes()).await;
                                                continue;
                                            }

                                            // Try streaming execution for simple external binaries
                                            // This provides real-time output for long-running commands
                                            let result = if let Some(streaming_result) = 
                                                shell::execute_command_streaming(
                                                    trimmed, &registry, &mut ctx, &mut channel_stream, None,
                                                ).await 
                                            {
                                                streaming_result
                                            } else {
                                                // Fall back to buffered execution for complex commands
                                                // (pipelines, redirects, builtins, command chains)
                                                shell::execute_command_chain(
                                                    trimmed, &registry, &mut ctx,
                                                ).await
                                            };

                                            // Output the result (empty for streamed commands)
                                            if !result.output.is_empty() {
                                                let _ = channel_stream.write(&result.output).await;
                                            }

                                            // Check if we should exit
                                            if result.should_exit {
                                                let _ = channel_stream.write(b"Goodbye!\r\n").await;
                                                return Ok(());
                                            }
                                        }

                                        line_buffer.clear();
                                        cursor_pos = 0;
                                        let prompt = generate_prompt(&ctx);
                                        let _ = channel_stream.write(prompt.as_bytes()).await;
                                    }
                                    0x7F | 0x08 => {
                                        // Backspace - delete character before cursor
                                        if cursor_pos > 0 {
                                            cursor_pos -= 1;
                                            line_buffer.remove(cursor_pos);

                                            // Move cursor back, rewrite rest of line, clear extra char
                                            let _ = channel_stream.write(b"\x08").await;
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            let _ = channel_stream.write(b" ").await;
                                            // Move cursor back to position
                                            let moves = line_buffer.len() - cursor_pos + 1;
                                            for _ in 0..moves {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                        }
                                    }
                                    0x03 => {
                                        // Ctrl+C
                                        line_buffer.clear();
                                        cursor_pos = 0;
                                        let _ = channel_stream.write(b"^C\r\n").await;
                                        let prompt = generate_prompt(&ctx);
                                        let _ = channel_stream.write(prompt.as_bytes()).await;
                                    }
                                    0x04 => {
                                        // Ctrl+D
                                        if line_buffer.is_empty() {
                                            let _ = channel_stream.write(b"\r\nGoodbye!\r\n").await;
                                            return Ok(());
                                        }
                                    }
                                    0x01 => {
                                        // Ctrl+A - move to beginning of line
                                        while cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    0x05 => {
                                        // Ctrl+E - move to end of line
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            cursor_pos = line_buffer.len();
                                        }
                                    }
                                    0x0B => {
                                        // Ctrl+K - kill from cursor to end of line
                                        if cursor_pos < line_buffer.len() {
                                            let chars_to_clear = line_buffer.len() - cursor_pos;
                                            line_buffer.truncate(cursor_pos);
                                            // Clear characters visually
                                            for _ in 0..chars_to_clear {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..chars_to_clear {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                        }
                                    }
                                    0x15 => {
                                        // Ctrl+U - kill from beginning to cursor
                                        if cursor_pos > 0 {
                                            // Move to beginning
                                            for _ in 0..cursor_pos {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                            // Write rest of line
                                            let rest: Vec<u8> = line_buffer[cursor_pos..].to_vec();
                                            let _ = channel_stream.write(&rest).await;
                                            // Clear old chars
                                            for _ in 0..cursor_pos {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            // Move back
                                            for _ in 0..(cursor_pos + rest.len()) {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                            line_buffer = rest;
                                            cursor_pos = 0;
                                        }
                                    }
                                    _ if byte >= 0x20 && byte < 0x7F => {
                                        // Printable character - insert at cursor position
                                        line_buffer.insert(cursor_pos, byte);
                                        cursor_pos += 1;

                                        // Write character and rest of line
                                        let _ =
                                            channel_stream.write(&line_buffer[cursor_pos - 1..]).await;
                                        // Move cursor back to position
                                        let moves = line_buffer.len() - cursor_pos;
                                        for _ in 0..moves {
                                            let _ = channel_stream.write(b"\x08").await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            EscapeState::Escape => {
                                if byte == b'[' {
                                    escape_state = EscapeState::Bracket;
                                } else {
                                    // Not a CSI sequence, ignore
                                    escape_state = EscapeState::Normal;
                                }
                            }
                            EscapeState::Bracket => {
                                escape_state = EscapeState::Normal;
                                match byte {
                                    b'A' => {
                                        // Up arrow - previous history
                                        if !history.is_empty() && history_index > 0 {
                                            // Save current line if at the end
                                            if history_index == history.len() {
                                                saved_line = line_buffer.clone();
                                            }
                                            history_index -= 1;

                                            // Clear current line
                                            while cursor_pos > 0 {
                                                let _ = channel_stream.write(b"\x08 \x08").await;
                                                cursor_pos -= 1;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }

                                            // Load history entry
                                            line_buffer = history[history_index].clone();
                                            cursor_pos = line_buffer.len();
                                            let _ = channel_stream.write(&line_buffer).await;
                                        }
                                    }
                                    b'B' => {
                                        // Down arrow - next history
                                        if history_index < history.len() {
                                            history_index += 1;

                                            // Clear current line
                                            while cursor_pos > 0 {
                                                let _ = channel_stream.write(b"\x08 \x08").await;
                                                cursor_pos -= 1;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }

                                            // Load history entry or saved line
                                            if history_index < history.len() {
                                                line_buffer = history[history_index].clone();
                                            } else {
                                                line_buffer = saved_line.clone();
                                            }
                                            cursor_pos = line_buffer.len();
                                            let _ = channel_stream.write(&line_buffer).await;
                                        }
                                    }
                                    b'C' => {
                                        // Right arrow - move cursor right
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&[line_buffer[cursor_pos]]).await;
                                            cursor_pos += 1;
                                        }
                                    }
                                    b'D' => {
                                        // Left arrow - move cursor left
                                        if cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    b'H' => {
                                        // Home key
                                        while cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    b'F' => {
                                        // End key
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            cursor_pos = line_buffer.len();
                                        }
                                    }
                                    b'3' => {
                                        // Might be Delete key (ESC[3~) - need to handle tilde
                                        // For simplicity, we'll handle this as a special case
                                        // The next byte should be ~
                                    }
                                    _ => {
                                        // Unknown escape sequence, ignore
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => {
                log("[SSH] Shell session ended (read error)\n");
                break;
            }
        }
    }

    Ok(())
}

// ============================================================================
// Message Handlers
// ============================================================================

/// Result of handling an SSH message
enum MessageResult {
    /// Continue processing messages
    Continue,
    /// Start an interactive shell session
    StartShell,
    /// Disconnect the session
    Disconnect,
}

async fn handle_message(
    stream: &mut TcpStream,
    msg_type: u8,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<MessageResult, TcpError> {
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
            // Use the auth module for proper authentication
            let (result, reply) =
                auth::handle_userauth_request(payload, &session.session_id, &session.config).await;

            send_packet(stream, &reply, session).await?;

            match result {
                AuthResult::Success => {
                    session.state = SshState::Authenticated;
                    log("[SSH] User authenticated\n");
                }
                AuthResult::PublicKeyOk(_) => {
                    // Key query - stay in AwaitingUserAuth state
                    log("[SSH] Public key accepted, waiting for signature\n");
                }
                AuthResult::Failure => {
                    // Stay in AwaitingUserAuth state to allow retry
                    log("[SSH] Authentication failed, waiting for retry\n");
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

                // Parse pty-req to get terminal dimensions
                if req_type == b"pty-req" {
                    offset += 1; // skip want_reply byte
                    // pty-req format: string TERM, uint32 width_chars, uint32 height_rows, ...
                    let _term = read_string(payload, &mut offset);
                    if let Some(width) = read_u32(payload, &mut offset) {
                        if let Some(height) = read_u32(payload, &mut offset) {
                            session.term_width = width;
                            session.term_height = height;
                            log(&alloc::format!(
                                "[SSH] Terminal size: {}x{}\n",
                                width,
                                height
                            ));
                        }
                    }
                }

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
                    // Signal to start the interactive shell session
                    return Ok(MessageResult::StartShell);
                } else if req_type == b"exec" {
                    // Handle exec request - supports command chaining with ; and &&
                    crate::console::print("[SSH-EXEC] Got exec request!\n");
                    // Create per-session context (starts at /)
                    let mut exec_ctx = ShellContext::new();
                    offset += 1; // skip want_reply byte
                    if let Some(cmd_bytes) = read_string(payload, &mut offset) {
                        crate::safe_print!(64, 
                            "[SSH-EXEC] Command: {:?}\n",
                            core::str::from_utf8(cmd_bytes)
                        );

                        let registry = create_default_registry();
                        let trimmed = trim_bytes(cmd_bytes);

                        // Scope the channel_stream so borrows are released for send_packet below
                        {
                            // Create a channel stream for potential streaming output
                            let mut channel_stream = SshChannelStream::new(stream, session);

                            // Try streaming execution for simple external binaries
                            if let Some(_streaming_result) = 
                                shell::execute_command_streaming(
                                    trimmed, &registry, &mut exec_ctx, &mut channel_stream, None,
                                ).await 
                            {
                                // Output was already streamed
                            } else {
                                // Fall back to buffered execution for complex commands
                                let _ = channel_stream.write(b"[DEBUG] Using buffered path\r\n").await;
                                let result =
                                    shell::execute_command_chain(trimmed, &registry, &mut exec_ctx).await;

                                // Send collected output
                                if !result.output.is_empty() {
                                    let _ = channel_stream.write(&result.output).await;
                                }
                            }
                        }
                    }
                    // Send EOF after exec - client will send CLOSE, we respond to that
                    let mut eof = vec![SSH_MSG_CHANNEL_EOF];
                    write_u32(&mut eof, session.client_channel);
                    send_packet(stream, &eof, session).await?;
                }
            }
        }

        SSH_MSG_CHANNEL_DATA => {
            // Channel data during non-shell mode is ignored
            // (Shell mode handles data via SshChannelStream)
            log("[SSH] Unexpected channel data outside shell mode\n");
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
            return Ok(MessageResult::Disconnect);
        }

        SSH_MSG_IGNORE | SSH_MSG_DEBUG => {}

        _ => {
            log(&format!("[SSH] Unhandled message type {}\n", msg_type));
            let mut reply = vec![SSH_MSG_UNIMPLEMENTED];
            write_u32(&mut reply, session.crypto.decrypt_seq.wrapping_sub(1));
            send_packet(stream, &reply, session).await?;
        }
    }

    Ok(MessageResult::Continue)
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

    // Get cached configuration (loaded at server startup)
    let config = super::config::get_config();
    let mut session = SshSession::new(config);

    // Send our version
    if send_raw(&mut stream, SSH_VERSION).await.is_err() {
        log("[SSH] Failed to send version\n");
        return;
    }

    // Main receive loop with timeout
    let mut buf = [0u8; 512];
    loop {
        // Use appropriate timeout based on connection state
        // After authentication, allow longer idle timeout for shell sessions
        let timeout = if session.state == SshState::Authenticated {
            SSH_IDLE_TIMEOUT
        } else {
            SSH_HANDSHAKE_TIMEOUT
        };
        
        let read_result = crate::kernel_timer::with_timeout(timeout, stream.read(&mut buf)).await;
        
        match read_result {
            Err(_timeout) => {
                log("[SSH] Connection timed out\n");
                break;
            }
            Ok(Ok(0)) => {
                log("[SSH] Connection closed by peer\n");
                break;
            }
            Ok(Err(_e)) => {
                log("[SSH] Read error\n");
                break;
            }
            Ok(Ok(n)) => {
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
                                Ok(MessageResult::Continue) => {}
                                Ok(MessageResult::StartShell) => {
                                    // Run the interactive shell session
                                    if run_shell_session(&mut stream, &mut session).await.is_err() {
                                        log("[SSH] Shell session error\n");
                                    }
                                    // After shell exits, close the channel and disconnect
                                    if session.channel_open {
                                        let mut close = vec![SSH_MSG_CHANNEL_CLOSE];
                                        write_u32(&mut close, session.client_channel);
                                        let _ =
                                            send_packet(&mut stream, &close, &mut session).await;
                                        session.channel_open = false;
                                    }
                                    session.state = SshState::Disconnected;
                                    return;
                                }
                                Ok(MessageResult::Disconnect) => {
                                    return;
                                }
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
        }
    }

    log("[SSH] Connection ended\n");
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    safe_print!(512, "{}", msg);
}
