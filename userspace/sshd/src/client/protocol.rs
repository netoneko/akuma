//! Client-side SSH-2: version/KEX/auth handshake plus the interactive
//! channel pump. Mirrors the algorithm suite `userspace/sshd` speaks
//! (`curve25519-sha256` / `ssh-ed25519` / `aes128-ctr` / `hmac-sha2-256`) but
//! is a fresh implementation — `src/client/main.rs` (package binary `ssh`)
//! and `src/main.rs` (`sshd`) are separate crate roots that can't share
//! modules directly with each other, and the two sides parse opposite
//! halves of the same wire format anyway.
//!
//! Scope, deliberately: no rekeying (one KEX per connection), no port/agent/
//! X11 forwarding, no SFTP/SCP subsystem, no cipher/KEX negotiation beyond
//! this one suite — this is a terminal client, not a general SSH client. It
//! still implements real flow control (channel window + max-packet) and a
//! TOFU `known_hosts`, because those are needed to interoperate with a real
//! third-party server (e.g. `ssh late.sh`), not just this repo's own `sshd`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ed25519_dalek::{Signer, SigningKey, Verifier};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use akuma_ssh_crypto::auth::{build_signed_data, parse_key_blob, parse_signature_blob};

use libakuma::net::{Error as NetError, ErrorKind as NetErrorKind, TcpStream};
use libakuma::*;

use sshd::client_wire;

use super::crypto::*;
use super::keys;

// ============================================================================
// Wire constants
// ============================================================================

const CLIENT_VERSION: &[u8] = b"SSH-2.0-Akuma-ssh_0.1\r\n";

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
const SSH_MSG_GLOBAL_REQUEST: u8 = 80;
const SSH_MSG_REQUEST_FAILURE: u8 = 82;
const SSH_MSG_CHANNEL_OPEN: u8 = 90;
const SSH_MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const SSH_MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
const SSH_MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const SSH_MSG_CHANNEL_DATA: u8 = 94;
const SSH_MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
const SSH_MSG_CHANNEL_EOF: u8 = 96;
const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
const SSH_MSG_CHANNEL_REQUEST: u8 = 98;
const SSH_MSG_CHANNEL_SUCCESS: u8 = 99;
const SSH_MSG_CHANNEL_FAILURE: u8 = 100;

use akuma_ssh_crypto::auth::{SSH_MSG_USERAUTH_FAILURE, SSH_MSG_USERAUTH_SUCCESS};

const LOCAL_CHANNEL: u32 = 0;
const INITIAL_WINDOW: u32 = 0x0010_0000; // 1 MiB — matches sshd's own confirmation
const MAX_PACKET: u32 = 0x4000; // 16 KiB
const WINDOW_ADJUST_THRESHOLD: u32 = 64 * 1024;
/// Hard ceiling on how much unconsumed data `Connection::input_buffer` may
/// hold. Without this, a peer that declares a packet_len far larger than it
/// ever sends (or than we'd ever legitimately need) makes us buffer
/// indefinitely while waiting for a "packet" that never completes — a
/// memory-exhaustion DoS against this client, not the peer. 1 MiB is
/// generous headroom over the 16 KiB `MAX_PACKET` this client ever asks a
/// peer to keep to, while still bounding the worst case.
const MAX_INPUT_BUFFER: usize = 1024 * 1024;

/// Raw mode flags (`akuma_terminal::mode_flags`), duplicated locally the same
/// way `userspace/termtest` does — libakuma doesn't re-export them.
mod mode_flags {
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    pub const RAW_MODE_DISABLE: u64 = 0x02;
}

// ============================================================================
// Public entry points
// ============================================================================

pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity: Option<String>,
    pub command: Option<String>,
    pub term: String,
}

pub enum ClientError {
    Net(NetError),
    Msg(String),
}

impl From<NetError> for ClientError {
    fn from(e: NetError) -> Self {
        ClientError::Net(e)
    }
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ClientError::Net(e) => write!(f, "{} ({:?})", e.message, e.kind()),
            ClientError::Msg(m) => write!(f, "{m}"),
        }
    }
}

pub fn run(cfg: ClientConfig) -> Result<i32, ClientError> {
    let ip = resolve_target(&cfg.host)?;
    let addr = format!("{}:{}", net::format_ip(ip), cfg.port);
    println(&format!("[ssh] connecting to {addr}..."));
    let stream = TcpStream::connect(&addr)?;

    let mut conn = Connection {
        stream,
        rng: new_seeded_rng(),
        crypto: CryptoState::new(),
        input_buffer: Vec::new(),
        own_newkeys_sent: false,
        peer_newkeys_received: false,
    };

    let client_version = CLIENT_VERSION[..CLIENT_VERSION.len() - 2].to_vec();
    conn.stream.write_all(CLIENT_VERSION)?;
    let server_version = read_version_line(&mut conn)?;

    let client_kexinit = client_wire::build_kexinit(&mut conn.rng);
    conn.send_payload(&client_kexinit)?;
    let (msg_type, kex_payload) = conn.recv_packet()?;
    if msg_type != SSH_MSG_KEXINIT {
        return Err(ClientError::Msg(format!(
            "expected KEXINIT, got message type {msg_type}"
        )));
    }
    let mut server_kexinit = vec![SSH_MSG_KEXINIT];
    server_kexinit.extend_from_slice(&kex_payload);
    let peer_algos = client_wire::parse_peer_kexinit(&kex_payload)
        .ok_or_else(|| ClientError::Msg(String::from("malformed KEXINIT")))?;
    require_algo(&peer_algos.kex, client_wire::KEX_ALGO, "key exchange")?;
    require_algo(&peer_algos.host_key, client_wire::HOST_KEY_ALGO, "host key")?;
    require_algo(&peer_algos.enc_c2s, client_wire::CIPHER_ALGO, "client-to-server cipher")?;
    require_algo(&peer_algos.enc_s2c, client_wire::CIPHER_ALGO, "server-to-client cipher")?;
    require_algo(&peer_algos.mac_c2s, client_wire::MAC_ALGO, "client-to-server MAC")?;
    require_algo(&peer_algos.mac_s2c, client_wire::MAC_ALGO, "server-to-client MAC")?;

    // --- ECDH key exchange ---
    // `conn.rng` (`SimpleRng`, an xorshift64 with 64 bits of state) is fine
    // for the KEXINIT cookie and AES-CTR packet padding — neither is secret
    // or security-load-bearing. The ephemeral X25519 secret is exactly the
    // opposite: 64 bits of effective entropy for what's supposed to be a
    // 256-bit key would collapse this exchange's real security margin to
    // whatever it takes to brute-force a 64-bit RNG state. Pull directly
    // from the kernel's hardware entropy instead, and refuse to proceed
    // rather than silently fall back to a weak secret if that fails.
    let mut secret_bytes = [0u8; 32];
    if getrandom(&mut secret_bytes).is_err() {
        return Err(ClientError::Msg(String::from(
            "couldn't obtain secure random bytes for the key exchange \u{2014} refusing to proceed",
        )));
    }
    let client_secret = StaticSecret::from(secret_bytes);
    let client_public = X25519PublicKey::from(&client_secret);

    let mut ecdh_init = vec![SSH_MSG_KEX_ECDH_INIT];
    write_string(&mut ecdh_init, client_public.as_bytes());
    conn.send_payload(&ecdh_init)?;

    let (msg_type, reply_payload) = conn.recv_packet()?;
    if msg_type != SSH_MSG_KEX_ECDH_REPLY {
        return Err(ClientError::Msg(format!(
            "expected KEX_ECDH_REPLY, got message type {msg_type}"
        )));
    }
    let mut off = 0;
    let host_key_blob = req_string(&reply_payload, &mut off, "KEX_ECDH_REPLY host key")?.to_vec();
    let server_pub_slice = req_string(&reply_payload, &mut off, "KEX_ECDH_REPLY server pubkey")?;
    let server_pub_bytes: [u8; 32] = server_pub_slice
        .try_into()
        .map_err(|_| ClientError::Msg(String::from("server ephemeral key has the wrong length")))?;
    let sig_blob = req_string(&reply_payload, &mut off, "KEX_ECDH_REPLY signature")?.to_vec();

    let host_key = parse_key_blob(&host_key_blob)
        .ok_or_else(|| ClientError::Msg(String::from("host key is not a valid ssh-ed25519 key")))?;

    let server_public = X25519PublicKey::from(server_pub_bytes);
    let shared_secret = client_secret.diffie_hellman(&server_public).as_bytes().to_vec();

    let exchange_hash = client_wire::kex_exchange_hash(
        &client_version,
        &server_version,
        &client_kexinit,
        &server_kexinit,
        &host_key_blob,
        client_public.as_bytes(),
        &server_pub_bytes,
        &shared_secret,
    );
    let session_id = exchange_hash;

    let signature = parse_signature_blob(&sig_blob)
        .ok_or_else(|| ClientError::Msg(String::from("malformed host key signature")))?;
    if host_key.verify(&exchange_hash, &signature).is_err() {
        return Err(ClientError::Msg(String::from(
            "host key signature verification FAILED \u{2014} refusing to continue (possible man-in-the-middle)",
        )));
    }

    let host_spec = format!("{}:{}", cfg.host, cfg.port);
    match keys::lookup_known_host(&host_spec, &host_key) {
        keys::HostKeyStatus::Known => {}
        keys::HostKeyStatus::Mismatch => {
            return Err(ClientError::Msg(format!(
                "REMOTE HOST IDENTIFICATION HAS CHANGED for {host_spec}!\n\
                 Someone could be eavesdropping (man-in-the-middle attack), or the host key was legitimately regenerated.\n\
                 Fingerprint offered: {}\n\
                 Refusing to connect. Remove the old entry from {} first if this change is expected.",
                keys::fingerprint_sha256(&host_key),
                keys::known_hosts_path()
            )));
        }
        keys::HostKeyStatus::New => {
            println(&format!("The authenticity of host '{host_spec}' can't be established."));
            println(&format!(
                "ED25519 key fingerprint is {}.",
                keys::fingerprint_sha256(&host_key)
            ));
            if !prompt_yes_no("Are you sure you want to continue connecting (yes/no)? ") {
                return Err(ClientError::Msg(String::from("host key not accepted")));
            }
            keys::add_known_host(&host_spec, &host_key);
        }
    }

    let iv_c2s = derive_key(&shared_secret, &exchange_hash, b'A', &session_id, AES_IV_SIZE);
    let iv_s2c = derive_key(&shared_secret, &exchange_hash, b'B', &session_id, AES_IV_SIZE);
    let key_c2s = derive_key(&shared_secret, &exchange_hash, b'C', &session_id, AES_KEY_SIZE);
    let key_s2c = derive_key(&shared_secret, &exchange_hash, b'D', &session_id, AES_KEY_SIZE);
    let mac_c2s = derive_key(&shared_secret, &exchange_hash, b'E', &session_id, MAC_KEY_SIZE);
    let mac_s2c = derive_key(&shared_secret, &exchange_hash, b'F', &session_id, MAC_KEY_SIZE);
    use ctr::cipher::KeyIvInit;
    // Our outgoing direction is client-to-server (letters A/C/E); our
    // incoming is server-to-client (B/D/F) — swapped relative to sshd's own
    // encrypt/decrypt naming, but keyed on the same letters, which is what
    // actually has to match on the wire.
    conn.crypto.encrypt_cipher = Some(Aes128Ctr::new(
        key_c2s[..AES_KEY_SIZE].into(),
        iv_c2s[..AES_IV_SIZE].into(),
    ));
    conn.crypto.encrypt_mac_key.copy_from_slice(&mac_c2s[..MAC_KEY_SIZE]);
    conn.crypto.decrypt_cipher = Some(Aes128Ctr::new(
        key_s2c[..AES_KEY_SIZE].into(),
        iv_s2c[..AES_IV_SIZE].into(),
    ));
    conn.crypto.decrypt_mac_key.copy_from_slice(&mac_s2c[..MAC_KEY_SIZE]);

    conn.send_payload(&[SSH_MSG_NEWKEYS])?;
    conn.own_newkeys_sent = true;
    let (msg_type, _) = conn.recv_packet()?;
    if msg_type != SSH_MSG_NEWKEYS {
        return Err(ClientError::Msg(format!(
            "expected NEWKEYS, got message type {msg_type}"
        )));
    }
    conn.peer_newkeys_received = true;

    let mut svc = vec![SSH_MSG_SERVICE_REQUEST];
    write_string(&mut svc, b"ssh-userauth");
    conn.send_payload(&svc)?;
    let (msg_type, _) = conn.recv_packet()?;
    if msg_type != SSH_MSG_SERVICE_ACCEPT {
        return Err(ClientError::Msg(format!(
            "service request refused (message type {msg_type})"
        )));
    }

    let identity = keys::load_identity(cfg.identity.as_deref())
        .ok_or_else(|| ClientError::Msg(String::from("no usable SSH identity key available")))?;
    authenticate(&mut conn, &session_id, &cfg.username, &identity)?;

    let mut open_payload = vec![SSH_MSG_CHANNEL_OPEN];
    write_string(&mut open_payload, b"session");
    write_u32(&mut open_payload, LOCAL_CHANNEL);
    write_u32(&mut open_payload, INITIAL_WINDOW);
    write_u32(&mut open_payload, MAX_PACKET);
    conn.send_payload(&open_payload)?;
    let (msg_type, payload) = conn.recv_packet()?;
    let (remote_channel, send_window, send_max_packet) = match msg_type {
        SSH_MSG_CHANNEL_OPEN_CONFIRMATION => {
            let mut off = 0;
            let _recipient = read_u32(&payload, &mut off);
            let remote_channel = read_u32(&payload, &mut off).ok_or_else(|| {
                ClientError::Msg(String::from("malformed CHANNEL_OPEN_CONFIRMATION"))
            })?;
            let window = read_u32(&payload, &mut off).unwrap_or(0);
            let max_packet = read_u32(&payload, &mut off).unwrap_or(MAX_PACKET).max(1);
            (remote_channel, window, max_packet)
        }
        SSH_MSG_CHANNEL_OPEN_FAILURE => {
            return Err(ClientError::Msg(String::from(
                "server refused to open a session channel",
            )));
        }
        other => {
            return Err(ClientError::Msg(format!(
                "expected CHANNEL_OPEN_CONFIRMATION, got message type {other}"
            )));
        }
    };

    let want_pty = cfg.command.is_none();
    if want_pty {
        let (cols, rows) = get_local_winsize();
        let mut req = vec![SSH_MSG_CHANNEL_REQUEST];
        write_u32(&mut req, remote_channel);
        write_string(&mut req, b"pty-req");
        req.push(1); // want_reply
        write_string(&mut req, cfg.term.as_bytes());
        write_u32(&mut req, u32::from(cols));
        write_u32(&mut req, u32::from(rows));
        write_u32(&mut req, 0);
        write_u32(&mut req, 0);
        write_string(&mut req, b"");
        conn.send_payload(&req)?;
        expect_channel_reply(&mut conn, "pty-req")?;
    }

    let mut req = vec![SSH_MSG_CHANNEL_REQUEST];
    write_u32(&mut req, remote_channel);
    if let Some(cmd) = &cfg.command {
        write_string(&mut req, b"exec");
        req.push(1);
        write_string(&mut req, cmd.as_bytes());
        conn.send_payload(&req)?;
        expect_channel_reply(&mut conn, "exec")?;
    } else {
        write_string(&mut req, b"shell");
        req.push(1);
        conn.send_payload(&req)?;
        expect_channel_reply(&mut conn, "shell")?;
    }

    if want_pty {
        set_terminal_attributes(fd::STDIN as u64, 0, mode_flags::RAW_MODE_ENABLE);
    }
    set_nonblocking(conn.stream.as_raw_fd(), true);
    set_nonblocking(fd::STDIN, true);
    let result = pump(&mut conn, remote_channel, send_window, send_max_packet);
    if want_pty {
        set_terminal_attributes(fd::STDIN as u64, 0, mode_flags::RAW_MODE_DISABLE);
    }
    println("");
    result
}

// ============================================================================
// Connection: packet framing over the raw socket
// ============================================================================

struct Connection {
    stream: TcpStream,
    rng: SimpleRng,
    crypto: CryptoState,
    input_buffer: Vec<u8>,
    /// RFC 4253: each side's own send encryption activates right after IT
    /// sends its own NEWKEYS, independent of when the peer sends theirs.
    own_newkeys_sent: bool,
    /// ...and each side's receive decryption activates right after IT
    /// receives the peer's NEWKEYS. Tracked separately from
    /// `own_newkeys_sent` rather than coupling both directions to one
    /// "handshake done" flag, so this stays correct even if a real server
    /// pipelines its NEWKEYS + first encrypted packet ahead of ours.
    peer_newkeys_received: bool,
}

impl Connection {
    fn send_payload(&mut self, payload: &[u8]) -> Result<(), ClientError> {
        let packet = if self.own_newkeys_sent
            && let Some(cipher) = self.crypto.encrypt_cipher.as_mut()
        {
            let seq = self.crypto.encrypt_seq;
            self.crypto.encrypt_seq = seq.wrapping_add(1);
            build_encrypted_packet(payload, cipher, &self.crypto.encrypt_mac_key, seq, &mut self.rng)
        } else {
            self.crypto.encrypt_seq = self.crypto.encrypt_seq.wrapping_add(1);
            build_packet(payload)
        };
        self.stream.write_all(&packet).map_err(ClientError::from)
    }

    /// Delegates to the host-tested framing in `sshd::client_wire` (this
    /// binary itself can't be host-tested — it links `libakuma`
    /// unconditionally — so the actual byte-parsing logic lives there).
    ///
    /// `Ok(None)` means "not enough bytes yet, try again once more arrive".
    /// `Err` means the buffered bytes don't parse as a valid packet at all
    /// (bad MAC, or a length field that can never become valid) — fatal,
    /// since retrying can't change the outcome for bytes already in hand.
    fn try_take_packet(&mut self) -> Result<Option<(u8, Vec<u8>)>, ClientError> {
        let outcome = if self.peer_newkeys_received && self.crypto.decrypt_cipher.is_some() {
            client_wire::take_encrypted_packet(&mut self.input_buffer, &mut self.crypto)
        } else {
            client_wire::take_unencrypted_packet(&mut self.input_buffer, &mut self.crypto)
        };
        match outcome {
            client_wire::TakePacket::Ready(msg_type, payload) => Ok(Some((msg_type, payload))),
            client_wire::TakePacket::Incomplete => Ok(None),
            client_wire::TakePacket::Malformed => Err(ClientError::Msg(String::from(
                "peer sent a malformed SSH packet \u{2014} disconnecting",
            ))),
        }
    }

    /// Blocking: pull more bytes off the socket until at least one full
    /// packet is available, transparently discarding `SSH_MSG_IGNORE` /
    /// `SSH_MSG_DEBUG` / `SSH_MSG_UNIMPLEMENTED` (RFC 4253 permits these
    /// anywhere in the protocol). Used only during the handshake — the
    /// interactive phase switches to non-blocking + `try_take_packet` so it
    /// can also service local stdin in the same loop.
    fn recv_packet(&mut self) -> Result<(u8, Vec<u8>), ClientError> {
        loop {
            while let Some((msg_type, payload)) = self.try_take_packet()? {
                match msg_type {
                    SSH_MSG_IGNORE | SSH_MSG_DEBUG | SSH_MSG_UNIMPLEMENTED => continue,
                    _ => return Ok((msg_type, payload)),
                }
            }
            if self.input_buffer.len() > MAX_INPUT_BUFFER {
                return Err(ClientError::Msg(String::from(
                    "peer sent an oversized or never-completing packet \u{2014} disconnecting",
                )));
            }
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf)?;
            if n == 0 {
                return Err(ClientError::Msg(String::from("connection closed by peer")));
            }
            self.input_buffer.extend_from_slice(&buf[..n]);
        }
    }
}

// ============================================================================
// KEXINIT algorithm negotiation
//
// The actual byte-layout logic (`build_kexinit`, `parse_peer_kexinit`,
// `namelist_has`) lives in `sshd::client_wire`, which is host-tested.
// ============================================================================

fn require_algo(list: &[u8], want: &str, label: &str) -> Result<(), ClientError> {
    if client_wire::namelist_has(list, want) {
        Ok(())
    } else {
        Err(ClientError::Msg(format!(
            "server does not support {label} algorithm '{want}' \u{2014} this client only speaks \
             curve25519-sha256 / ssh-ed25519 / aes128-ctr / hmac-sha2-256"
        )))
    }
}

fn req_string<'a>(data: &'a [u8], off: &mut usize, what: &str) -> Result<&'a [u8], ClientError> {
    read_string(data, off).ok_or_else(|| ClientError::Msg(format!("malformed {what}")))
}

// ============================================================================
// Auth
// ============================================================================

fn authenticate(
    conn: &mut Connection,
    session_id: &[u8; 32],
    username: &str,
    identity: &SigningKey,
) -> Result<(), ClientError> {
    // Query with "none" first, like a real client, so a server that doesn't
    // offer publickey at all is reported clearly instead of us just trying
    // to sign against it and getting a generic failure back.
    let mut none_req = vec![SSH_MSG_USERAUTH_REQUEST];
    write_string(&mut none_req, username.as_bytes());
    write_string(&mut none_req, b"ssh-connection");
    write_string(&mut none_req, b"none");
    conn.send_payload(&none_req)?;

    let (msg_type, payload) = conn.recv_packet()?;
    match msg_type {
        SSH_MSG_USERAUTH_SUCCESS => return Ok(()), // server accepts anyone (disable_key_verification-style)
        SSH_MSG_USERAUTH_FAILURE => {
            let mut off = 0;
            let methods = read_string(&payload, &mut off).unwrap_or(b"");
            let methods_str = core::str::from_utf8(methods).unwrap_or("");
            if !methods_str.split(',').any(|m| m == "publickey") {
                return Err(ClientError::Msg(format!(
                    "server does not offer publickey authentication (offers: {methods_str})"
                )));
            }
        }
        other => {
            return Err(ClientError::Msg(format!(
                "unexpected reply to auth query (message type {other})"
            )));
        }
    }

    let verifying_key = identity.verifying_key();
    let mut key_blob = Vec::new();
    write_string(&mut key_blob, b"ssh-ed25519");
    write_string(&mut key_blob, verifying_key.as_bytes());

    let signed_data = build_signed_data(session_id, username.as_bytes(), b"ssh-connection", b"ssh-ed25519", &key_blob);
    let signature = identity.sign(&signed_data);
    let mut sig_blob = Vec::new();
    write_string(&mut sig_blob, b"ssh-ed25519");
    write_string(&mut sig_blob, signature.to_bytes().as_slice());

    let mut req = vec![SSH_MSG_USERAUTH_REQUEST];
    write_string(&mut req, username.as_bytes());
    write_string(&mut req, b"ssh-connection");
    write_string(&mut req, b"publickey");
    req.push(1); // has_signature = true
    write_string(&mut req, b"ssh-ed25519");
    write_string(&mut req, &key_blob);
    write_string(&mut req, &sig_blob);
    conn.send_payload(&req)?;

    let (msg_type, payload) = conn.recv_packet()?;
    match msg_type {
        SSH_MSG_USERAUTH_SUCCESS => Ok(()),
        SSH_MSG_USERAUTH_FAILURE => {
            let mut off = 0;
            let methods = read_string(&payload, &mut off).unwrap_or(b"");
            Err(ClientError::Msg(format!(
                "publickey authentication failed for user '{username}' (server offers: {})",
                core::str::from_utf8(methods).unwrap_or("")
            )))
        }
        other => Err(ClientError::Msg(format!(
            "unexpected reply to publickey auth (message type {other})"
        ))),
    }
}

fn expect_channel_reply(conn: &mut Connection, what: &str) -> Result<(), ClientError> {
    let (msg_type, _payload) = conn.recv_packet()?;
    match msg_type {
        SSH_MSG_CHANNEL_SUCCESS => Ok(()),
        SSH_MSG_CHANNEL_FAILURE => Err(ClientError::Msg(format!("server rejected '{what}' request"))),
        other => Err(ClientError::Msg(format!(
            "expected a reply to '{what}', got message type {other}"
        ))),
    }
}

// ============================================================================
// Interactive pump
// ============================================================================

/// Forwards local stdin <-> the session channel until it closes. Real flow
/// control (channel window + max-packet) is honored on the outbound side,
/// and inbound window credit is returned once it builds up — both needed for
/// a well-behaved third-party server, not just this repo's own `sshd`.
fn pump(
    conn: &mut Connection,
    remote_channel: u32,
    mut send_window: u32,
    send_max_packet: u32,
) -> Result<i32, ClientError> {
    let mut stdin_pending: Vec<u8> = Vec::new();
    let mut local_input_done = false;
    let mut channel_eof_sent = false;
    let mut recv_credit: u32 = 0;
    let mut exit_code: Option<i32> = None;

    loop {
        let mut did_io = false;

        if !local_input_done {
            let mut buf = [0u8; 4096];
            let n = read(fd::STDIN, &mut buf);
            if n > 0 {
                stdin_pending.extend_from_slice(&buf[..n as usize]);
                did_io = true;
            } else if n == 0 {
                local_input_done = true;
            }
            // n < 0: EAGAIN — stdin is non-blocking; nothing to do this tick.
        }

        if !stdin_pending.is_empty() && send_window > 0 {
            let chunk_len = stdin_pending
                .len()
                .min(send_max_packet as usize)
                .min(send_window as usize);
            if chunk_len > 0 {
                let mut data_payload = vec![SSH_MSG_CHANNEL_DATA];
                write_u32(&mut data_payload, remote_channel);
                write_string(&mut data_payload, &stdin_pending[..chunk_len]);
                conn.send_payload(&data_payload)?;
                stdin_pending.drain(..chunk_len);
                send_window -= chunk_len as u32;
                did_io = true;
            }
        }

        if local_input_done && stdin_pending.is_empty() && !channel_eof_sent {
            let mut eof_payload = vec![SSH_MSG_CHANNEL_EOF];
            write_u32(&mut eof_payload, remote_channel);
            conn.send_payload(&eof_payload)?;
            channel_eof_sent = true;
            did_io = true;
        }

        let mut sock_buf = [0u8; 4096];
        match conn.stream.read(&mut sock_buf) {
            Ok(0) => return Ok(exit_code.unwrap_or(255)),
            Ok(n) => {
                conn.input_buffer.extend_from_slice(&sock_buf[..n]);
                if conn.input_buffer.len() > MAX_INPUT_BUFFER {
                    return Err(ClientError::Msg(String::from(
                        "peer sent an oversized or never-completing packet \u{2014} disconnecting",
                    )));
                }
                did_io = true;
            }
            Err(e) if e.kind() == NetErrorKind::WouldBlock => {}
            Err(e) => return Err(ClientError::from(e)),
        }

        let mut should_close = false;
        while let Some((msg_type, payload)) = conn.try_take_packet()? {
            did_io = true;
            match msg_type {
                SSH_MSG_CHANNEL_DATA => {
                    let mut off = 0;
                    if read_u32(&payload, &mut off) != Some(LOCAL_CHANNEL) {
                        continue; // not our channel — we only ever open one
                    }
                    if let Some(data) = read_string(&payload, &mut off) {
                        write(fd::STDOUT, data);
                        recv_credit += data.len() as u32;
                    }
                }
                SSH_MSG_CHANNEL_EXTENDED_DATA => {
                    let mut off = 0;
                    if read_u32(&payload, &mut off) != Some(LOCAL_CHANNEL) {
                        continue;
                    }
                    let _data_type = read_u32(&payload, &mut off);
                    if let Some(data) = read_string(&payload, &mut off) {
                        write(fd::STDOUT, data);
                        recv_credit += data.len() as u32;
                    }
                }
                SSH_MSG_CHANNEL_WINDOW_ADJUST => {
                    let mut off = 0;
                    if read_u32(&payload, &mut off) != Some(LOCAL_CHANNEL) {
                        continue;
                    }
                    if let Some(add) = read_u32(&payload, &mut off) {
                        send_window = send_window.saturating_add(add);
                    }
                }
                SSH_MSG_CHANNEL_REQUEST => {
                    let mut off = 0;
                    if read_u32(&payload, &mut off) != Some(LOCAL_CHANNEL) {
                        continue;
                    }
                    if let Some(req_type) = read_string(&payload, &mut off) {
                        if req_type == b"exit-status" {
                            off += 1; // want_reply, always false per spec
                            if let Some(code) = read_u32(&payload, &mut off) {
                                exit_code = Some((code & 0xFF) as i32);
                            }
                        } else if req_type == b"exit-signal" {
                            off += 1; // want_reply
                            if let Some(sig_name) = read_string(&payload, &mut off) {
                                eprintln(&format!(
                                    "ssh: remote process terminated by signal {}",
                                    String::from_utf8_lossy(sig_name)
                                ));
                            }
                            exit_code = Some(255);
                        }
                    }
                }
                SSH_MSG_GLOBAL_REQUEST => {
                    let mut off = 0;
                    let _req_name = read_string(&payload, &mut off);
                    let want_reply = payload.get(off).copied().unwrap_or(0) != 0;
                    if want_reply {
                        // Ignored on failure: a broken connection surfaces on
                        // the next `conn.stream.read`/`send_payload` in this
                        // same loop anyway, which does propagate.
                        let _ = conn.send_payload(&[SSH_MSG_REQUEST_FAILURE]);
                    }
                }
                SSH_MSG_CHANNEL_EOF => {} // remote stopped sending; keep pumping until CLOSE
                SSH_MSG_CHANNEL_CLOSE => {
                    let mut off = 0;
                    if read_u32(&payload, &mut off) == Some(LOCAL_CHANNEL) {
                        should_close = true;
                    }
                }
                SSH_MSG_DISCONNECT => {
                    let mut off = 0;
                    let _reason_code = read_u32(&payload, &mut off);
                    if let Some(msg) = read_string(&payload, &mut off) {
                        eprintln(&format!(
                            "ssh: disconnected by peer: {}",
                            String::from_utf8_lossy(msg)
                        ));
                    }
                    return Ok(exit_code.unwrap_or(255));
                }
                SSH_MSG_IGNORE | SSH_MSG_DEBUG | SSH_MSG_UNIMPLEMENTED => {}
                _ => {}
            }
        }

        if recv_credit >= WINDOW_ADJUST_THRESHOLD {
            let mut adjust = vec![SSH_MSG_CHANNEL_WINDOW_ADJUST];
            write_u32(&mut adjust, remote_channel);
            write_u32(&mut adjust, recv_credit);
            conn.send_payload(&adjust)?;
            recv_credit = 0;
        }

        if should_close {
            // Best-effort: we're returning either way, and a failure here
            // just means the peer already dropped the connection.
            let mut close_payload = vec![SSH_MSG_CHANNEL_CLOSE];
            write_u32(&mut close_payload, remote_channel);
            if let Err(e) = conn.send_payload(&close_payload) {
                eprintln(&format!("ssh: couldn't send CHANNEL_CLOSE on the way out: {e}"));
            }
            return Ok(exit_code.unwrap_or(255));
        }

        if !did_io {
            sleep_ms(1);
        }
    }
}

// ============================================================================
// Small local helpers
// ============================================================================

fn resolve_target(host: &str) -> Result<[u8; 4], ClientError> {
    if let Some(ip) = SocketAddrV4::parse_ip(host) {
        return Ok(ip);
    }
    resolve_host(host).map_err(|errno| {
        ClientError::Msg(format!("cannot resolve host '{host}' (errno {errno})"))
    })
}

fn read_version_line(conn: &mut Connection) -> Result<Vec<u8>, ClientError> {
    const MAX_BANNER_LINES: u32 = 100;
    let mut banner_lines = 0u32;
    loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = conn.stream.read(&mut byte)?;
            if n == 0 {
                return Err(ClientError::Msg(String::from(
                    "connection closed during version exchange",
                )));
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
            if line.len() > 1024 {
                return Err(ClientError::Msg(String::from("version line too long")));
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.starts_with(b"SSH-") {
            return Ok(line);
        }
        banner_lines += 1;
        if banner_lines > MAX_BANNER_LINES {
            return Err(ClientError::Msg(String::from(
                "too many lines before the SSH version string \u{2014} giving up",
            )));
        }
        // RFC 4253 §4.2: a server may send other lines (e.g. a banner) before
        // its version line — print them and keep reading.
        println(&String::from_utf8_lossy(&line));
    }
}

fn prompt_yes_no(prompt: &str) -> bool {
    print(prompt);
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = read(fd::STDIN, &mut byte);
        if n <= 0 {
            return false; // EOF or error on the prompt: fail safe, don't trust the host
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' && line.len() < 16 {
            line.push(byte[0]);
        }
    }
    let answer = core::str::from_utf8(&line).unwrap_or("").trim().to_lowercase();
    answer == "yes" || answer == "y"
}

fn get_local_winsize() -> (u16, u16) {
    const TIOCGWINSZ: u64 = 0x5413;
    let mut winsz: [u16; 4] = [0; 4]; // ws_row, ws_col, ws_xpixel, ws_ypixel
    let ret = syscall(syscall::IOCTL, fd::STDIN as u64, TIOCGWINSZ, winsz.as_mut_ptr() as u64, 0, 0, 0);
    if (ret as i64) < 0 || winsz[1] == 0 {
        return (80, 24);
    }
    (winsz[1], winsz[0]) // (cols, rows)
}
