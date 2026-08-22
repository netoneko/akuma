//! Pure, host-testable pieces of the `ssh` client's wire handling: KEXINIT
//! build/parse, the KEX exchange-hash computation, and encrypted/unencrypted
//! packet framing.
//!
//! Split out into the lib target for the same reason `wire.rs` was: the
//! client binary (`src/bin/ssh.rs`) links `libakuma` unconditionally (same
//! as `main.rs`), so nothing under `src/bin/ssh/` can be host-tested
//! directly. Anything here that's just logic over bytes lives in this
//! module instead, and `src/bin/ssh/protocol.rs` calls into it via
//! `sshd::client_wire::...` — the same cross-binary-to-lib pattern
//! `main.rs`/`protocol.rs` already use for `sshd::wire`.
//!
//! ```text
//! cargo test -p sshd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```

use alloc::vec;
use alloc::vec::Vec;

use hmac::Mac;
use sha2::{Digest, Sha256};

use akuma_ssh_crypto::crypto::{
    CryptoState, HmacSha256, MAC_SIZE, SimpleRng, read_string, write_namelist, write_string,
    write_u32,
};

pub const SSH_MSG_KEXINIT: u8 = 20;

pub const KEX_ALGO: &str = "curve25519-sha256";
pub const HOST_KEY_ALGO: &str = "ssh-ed25519";
pub const CIPHER_ALGO: &str = "aes128-ctr";
pub const MAC_ALGO: &str = "hmac-sha2-256";
pub const COMPRESS_ALGO: &str = "none";

/// Build the client's KEXINIT payload (tag byte included), advertising
/// exactly the one algorithm this client speaks in each slot.
#[must_use]
pub fn build_kexinit(rng: &mut SimpleRng) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_KEXINIT];
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
    payload.push(0); // first_kex_packet_follows = false
    write_u32(&mut payload, 0); // reserved
    payload
}

/// The six algorithm name-lists we care about out of a peer's KEXINIT.
pub struct PeerKexAlgos {
    pub kex: Vec<u8>,
    pub host_key: Vec<u8>,
    pub enc_c2s: Vec<u8>,
    pub enc_s2c: Vec<u8>,
    pub mac_c2s: Vec<u8>,
    pub mac_s2c: Vec<u8>,
}

/// Parse a peer's KEXINIT payload (tag byte **excluded** — same convention
/// `read_string`'s callers use throughout this codebase). `None` on any
/// truncation.
#[must_use]
pub fn parse_peer_kexinit(payload: &[u8]) -> Option<PeerKexAlgos> {
    if payload.len() < 16 {
        return None;
    }
    let mut off = 16usize; // skip the 16-byte cookie
    let kex = read_string(payload, &mut off)?.to_vec();
    let host_key = read_string(payload, &mut off)?.to_vec();
    let enc_c2s = read_string(payload, &mut off)?.to_vec();
    let enc_s2c = read_string(payload, &mut off)?.to_vec();
    let mac_c2s = read_string(payload, &mut off)?.to_vec();
    let mac_s2c = read_string(payload, &mut off)?.to_vec();
    // compression (x2) + languages (x2) follow; this client only ever offers
    // "none"/"" so it has no use for them, but still consumed so a caller
    // that reads past this point (there isn't one today) sees the right offset.
    read_string(payload, &mut off)?;
    read_string(payload, &mut off)?;
    read_string(payload, &mut off)?;
    read_string(payload, &mut off)?;
    Some(PeerKexAlgos { kex, host_key, enc_c2s, enc_s2c, mac_c2s, mac_s2c })
}

/// Does the comma-separated name-list `list` contain the exact name `want`?
/// Exact per-token match, not a substring search — `"ssh-ed25519"` must not
/// match inside `"ssh-ed25519-cert-v01@openssh.com"`.
#[must_use]
pub fn namelist_has(list: &[u8], want: &str) -> bool {
    core::str::from_utf8(list)
        .map(|s| s.split(',').any(|a| a == want))
        .unwrap_or(false)
}

/// RFC 4253 §8 key-exchange hash: `H = SHA256(V_C || V_S || I_C || I_S || K_S
/// || e || f || K)`, with `K` (the shared secret) mpint-encoded per §5
/// (a leading zero byte inserted when the high bit of the first byte is set,
/// so it isn't misread as a negative number).
#[must_use]
pub fn kex_exchange_hash(
    client_version: &[u8],
    server_version: &[u8],
    client_kexinit: &[u8],
    server_kexinit: &[u8],
    host_key_blob: &[u8],
    client_ephemeral_pub: &[u8],
    server_ephemeral_pub: &[u8],
    shared_secret: &[u8],
) -> [u8; 32] {
    let mut hash_data = Vec::new();
    write_string(&mut hash_data, client_version);
    write_string(&mut hash_data, server_version);
    write_string(&mut hash_data, client_kexinit);
    write_string(&mut hash_data, server_kexinit);
    write_string(&mut hash_data, host_key_blob);
    write_string(&mut hash_data, client_ephemeral_pub);
    write_string(&mut hash_data, server_ephemeral_pub);
    if !shared_secret.is_empty() && shared_secret[0] & 0x80 != 0 {
        write_u32(&mut hash_data, (shared_secret.len() + 1) as u32);
        hash_data.push(0);
    } else {
        write_u32(&mut hash_data, shared_secret.len() as u32);
    }
    hash_data.extend_from_slice(shared_secret);
    let mut hasher = Sha256::new();
    hasher.update(&hash_data);
    hasher.finalize().into()
}

/// Try to take one complete AES-128-CTR + HMAC-SHA256 packet off the front of
/// `input_buffer`. Returns `None` (without consuming anything or advancing
/// `crypto.decrypt_seq`) if fewer bytes are buffered than a full packet
/// needs, if the MAC doesn't verify, or if `decrypt_cipher` isn't set yet.
///
/// `padding_len >= packet_len` is rejected explicitly rather than trusted:
/// it's peer-controlled, this workspace builds with `overflow-checks =
/// false`, and an unchecked underflow here would wrap to a huge `usize` and
/// panic the (`panic = "abort"`) process on the slice index below.
#[must_use]
pub fn take_encrypted_packet(
    input_buffer: &mut Vec<u8>,
    crypto: &mut CryptoState,
) -> Option<(u8, Vec<u8>)> {
    if input_buffer.len() < 4 {
        return None;
    }
    let cipher = crypto.decrypt_cipher.as_mut()?;
    use ctr::cipher::StreamCipher;
    let mut peek_cipher = cipher.clone();
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&input_buffer[..4]);
    peek_cipher.apply_keystream(&mut len_buf);
    let packet_len = u32::from_be_bytes(len_buf) as usize;
    // A packet needs at least a padding_len byte plus one payload byte
    // (the message type). Without this, packet_len 0 or 1 makes
    // `decrypted[4]`/`decrypted[5]` below index past the end of a
    // `4 + packet_len`-byte buffer and panic — reachable by any
    // authenticated peer (it only needs a packet that MACs correctly under
    // keys it already holds), which for a client means a malicious server.
    if packet_len < 2 {
        return None;
    }
    let total_needed = 4 + packet_len + MAC_SIZE;
    if input_buffer.len() < total_needed {
        return None;
    }
    let encrypted_data = &input_buffer[..4 + packet_len];
    let received_mac = &input_buffer[4 + packet_len..total_needed];
    let mut decrypted = encrypted_data.to_vec();
    cipher.apply_keystream(&mut decrypted);
    let seq = crypto.decrypt_seq;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&crypto.decrypt_mac_key).ok()?;
    mac.update(&seq.to_be_bytes());
    mac.update(&decrypted);
    if mac.verify_slice(received_mac).is_err() {
        return None;
    }
    crypto.decrypt_seq = seq.wrapping_add(1);
    let padding_len = decrypted[4] as usize;
    if padding_len >= packet_len {
        return None;
    }
    let payload_len = packet_len - padding_len - 1;
    let msg_type = decrypted[5];
    let payload = decrypted[6..5 + payload_len].to_vec();
    *input_buffer = input_buffer[total_needed..].to_vec();
    Some((msg_type, payload))
}

/// Same as [`take_encrypted_packet`], for the pre-`NEWKEYS` framing (no
/// cipher, no MAC). `crypto.decrypt_seq` still advances on every full packet
/// taken here — RFC 4253 counts sequence numbers from the very first packet
/// exchanged, encrypted or not.
#[must_use]
pub fn take_unencrypted_packet(
    input_buffer: &mut Vec<u8>,
    crypto: &mut CryptoState,
) -> Option<(u8, Vec<u8>)> {
    if input_buffer.len() < 5 {
        return None;
    }
    let packet_len = u32::from_be_bytes(input_buffer[..4].try_into().ok()?) as usize;
    // See the identical check in `take_encrypted_packet`: packet_len 0 or 1
    // (with padding_len 0) reads `input_buffer[5]` one past a 5-byte buffer
    // and panics. This framing has no MAC, so — unlike the encrypted path —
    // any TCP peer can trigger it with 5 crafted bytes before KEX even
    // completes.
    if packet_len < 2 {
        return None;
    }
    let total_len = 4 + packet_len;
    if input_buffer.len() < total_len {
        return None;
    }
    let padding_len = input_buffer[4] as usize;
    if padding_len >= packet_len {
        return None;
    }
    let payload_len = packet_len - padding_len - 1;
    let msg_type = input_buffer[5];
    let payload = input_buffer[6..5 + payload_len].to_vec();
    crypto.decrypt_seq = crypto.decrypt_seq.wrapping_add(1);
    *input_buffer = input_buffer[total_len..].to_vec();
    Some((msg_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_ssh_crypto::crypto::{
        AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, build_encrypted_packet, build_packet, read_u32,
    };
    use ctr::cipher::StreamCipher;

    #[test]
    fn build_kexinit_has_a_16_byte_cookie_and_advertises_one_algo_per_slot() {
        let mut rng = SimpleRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8]);
        let payload = build_kexinit(&mut rng);
        assert_eq!(payload[0], SSH_MSG_KEXINIT);
        let mut off = 1 + 16; // tag + cookie
        for want in [KEX_ALGO, HOST_KEY_ALGO, CIPHER_ALGO, CIPHER_ALGO, MAC_ALGO, MAC_ALGO, COMPRESS_ALGO, COMPRESS_ALGO] {
            assert_eq!(read_string(&payload, &mut off), Some(want.as_bytes()));
        }
        assert_eq!(read_string(&payload, &mut off), Some(&b""[..])); // languages c2s
        assert_eq!(read_string(&payload, &mut off), Some(&b""[..])); // languages s2c
        assert_eq!(payload[off], 0, "first_kex_packet_follows must be false");
        off += 1;
        assert_eq!(read_u32(&payload, &mut off), Some(0), "reserved must be 0");
        assert_eq!(off, payload.len(), "trailing bytes after KEXINIT");
    }

    /// Two calls must not reuse the same cookie — that's the one field in
    /// this payload that has to vary per connection.
    #[test]
    fn build_kexinit_cookie_is_not_fixed() {
        let mut rng = SimpleRng::from_seed([9, 9, 9, 9, 9, 9, 9, 9]);
        let a = build_kexinit(&mut rng);
        let b = build_kexinit(&mut rng);
        assert_ne!(&a[1..17], &b[1..17]);
    }

    #[test]
    fn parse_peer_kexinit_round_trips_our_own_builder() {
        let mut rng = SimpleRng::from_seed([1, 1, 1, 1, 1, 1, 1, 1]);
        let payload = build_kexinit(&mut rng);
        let algos = parse_peer_kexinit(&payload[1..]).expect("parse");
        assert_eq!(algos.kex, KEX_ALGO.as_bytes());
        assert_eq!(algos.host_key, HOST_KEY_ALGO.as_bytes());
        assert_eq!(algos.enc_c2s, CIPHER_ALGO.as_bytes());
        assert_eq!(algos.enc_s2c, CIPHER_ALGO.as_bytes());
        assert_eq!(algos.mac_c2s, MAC_ALGO.as_bytes());
        assert_eq!(algos.mac_s2c, MAC_ALGO.as_bytes());
    }

    #[test]
    fn parse_peer_kexinit_rejects_truncated_payload() {
        assert!(parse_peer_kexinit(&[0u8; 10]).is_none()); // shorter than the cookie alone
        let mut rng = SimpleRng::from_seed([2, 2, 2, 2, 2, 2, 2, 2]);
        let payload = build_kexinit(&mut rng);
        // Cookie (16 bytes) plus only 2 of the first namelist's 4 length
        // bytes: too short for even the first `read_string` to succeed.
        let cut = &payload[1..1 + 16 + 2];
        assert!(parse_peer_kexinit(cut).is_none());
    }

    #[test]
    fn namelist_has_matches_whole_tokens_only() {
        let list = b"curve25519-sha256,diffie-hellman-group14-sha256";
        assert!(namelist_has(list, "curve25519-sha256"));
        assert!(namelist_has(list, "diffie-hellman-group14-sha256"));
        assert!(!namelist_has(list, "curve25519"));
        assert!(!namelist_has(list, "sha256"));

        // A cert variant must not satisfy a plain-key requirement via substring match.
        let host_keys = b"ssh-ed25519-cert-v01@openssh.com,ssh-rsa";
        assert!(!namelist_has(host_keys, "ssh-ed25519"));
    }

    #[test]
    fn namelist_has_handles_single_entry_and_empty_list() {
        assert!(namelist_has(b"ssh-ed25519", "ssh-ed25519"));
        assert!(!namelist_has(b"", "ssh-ed25519"));
    }

    /// Spelled out literally (not rebuilt with `write_string`/`write_u32`,
    /// the same helpers the code under test uses) so a change in field order
    /// or the mpint rule has to fail here — same rationale as `wire.rs`'s
    /// `exit_status_payload_has_exact_rfc_layout`.
    #[test]
    fn kex_exchange_hash_matches_rfc4253_field_order() {
        let v_c = b"SSH-2.0-Client";
        let v_s = b"SSH-2.0-Server";
        let i_c = &[1u8, 2, 3][..];
        let i_s = &[4u8, 5][..];
        let k_s = &[9u8; 4][..];
        let e = &[7u8; 32][..];
        let f = &[8u8; 32][..];
        // High bit set on the first byte -> mpint encoding needs a leading zero.
        let shared_secret = [0x80u8, 0x01, 0x02];

        let mut expected = Vec::new();
        for field in [&v_c[..], &v_s[..], i_c, i_s, k_s, e, f] {
            expected.extend_from_slice(&(field.len() as u32).to_be_bytes());
            expected.extend_from_slice(field);
        }
        expected.extend_from_slice(&((shared_secret.len() + 1) as u32).to_be_bytes());
        expected.push(0);
        expected.extend_from_slice(&shared_secret);
        let want: [u8; 32] = Sha256::digest(&expected).into();

        let got = kex_exchange_hash(v_c, v_s, i_c, i_s, k_s, e, f, &shared_secret);
        assert_eq!(got, want);
    }

    #[test]
    fn kex_exchange_hash_skips_the_leading_zero_when_high_bit_is_clear() {
        let shared_secret = [0x01u8, 0x02, 0x03]; // high bit clear
        let mut expected = Vec::new();
        for field in [&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..], &b"e"[..], &b"f"[..], &b"g"[..]] {
            expected.extend_from_slice(&(field.len() as u32).to_be_bytes());
            expected.extend_from_slice(field);
        }
        expected.extend_from_slice(&(shared_secret.len() as u32).to_be_bytes());
        expected.extend_from_slice(&shared_secret);
        let want: [u8; 32] = Sha256::digest(&expected).into();

        let got = kex_exchange_hash(b"a", b"b", b"c", b"d", b"e", b"f", b"g", &shared_secret);
        assert_eq!(got, want);
    }

    fn test_crypto_pair() -> (CryptoState, CryptoState) {
        // Two independent CryptoStates sharing the same key material, one
        // acting as "sender" (its encrypt side is used to build packets) and
        // one as "receiver" (its decrypt side is used by take_*_packet) —
        // mirrors how the client's encrypt keys and the peer's decrypt keys
        // are the same derived material in a real session.
        let key = [0x11u8; AES_KEY_SIZE];
        let iv = [0x22u8; AES_IV_SIZE];
        let mac_key = [0x33u8; 32];
        use ctr::cipher::KeyIvInit;
        let mut sender = CryptoState::new();
        sender.encrypt_cipher = Some(Aes128Ctr::new((&key).into(), (&iv).into()));
        sender.encrypt_mac_key = mac_key;
        let mut receiver = CryptoState::new();
        receiver.decrypt_cipher = Some(Aes128Ctr::new((&key).into(), (&iv).into()));
        receiver.decrypt_mac_key = mac_key;
        (sender, receiver)
    }

    #[test]
    fn encrypted_packet_round_trips_type_and_payload() {
        let (mut sender, mut receiver) = test_crypto_pair();
        let mut rng = SimpleRng::from_seed([5, 5, 5, 5, 5, 5, 5, 5]);
        let payload = vec![42u8, 1, 2, 3, 4, 5];
        let cipher = sender.encrypt_cipher.as_mut().unwrap();
        let packet = build_encrypted_packet(&payload, cipher, &sender.encrypt_mac_key, 0, &mut rng);

        let mut input_buffer = packet;
        let (msg_type, got_payload) = take_encrypted_packet(&mut input_buffer, &mut receiver)
            .expect("a full packet was supplied");
        assert_eq!(msg_type, 42);
        assert_eq!(got_payload, vec![1, 2, 3, 4, 5]);
        assert!(input_buffer.is_empty(), "the whole packet must be consumed");
        assert_eq!(receiver.decrypt_seq, 1);
    }

    #[test]
    fn encrypted_packet_returns_none_and_consumes_nothing_when_partial() {
        let (mut sender, mut receiver) = test_crypto_pair();
        let mut rng = SimpleRng::from_seed([6, 6, 6, 6, 6, 6, 6, 6]);
        let cipher = sender.encrypt_cipher.as_mut().unwrap();
        let packet = build_encrypted_packet(&[9, 9, 9], cipher, &sender.encrypt_mac_key, 0, &mut rng);

        let mut input_buffer = packet[..packet.len() - 1].to_vec(); // one byte short
        let before = input_buffer.clone();
        assert!(take_encrypted_packet(&mut input_buffer, &mut receiver).is_none());
        assert_eq!(input_buffer, before, "a partial packet must not be consumed");
        assert_eq!(receiver.decrypt_seq, 0, "seq must not advance without a full packet");
    }

    #[test]
    fn encrypted_packet_rejects_a_bad_mac() {
        let (mut sender, mut receiver) = test_crypto_pair();
        let mut rng = SimpleRng::from_seed([7, 7, 7, 7, 7, 7, 7, 7]);
        let cipher = sender.encrypt_cipher.as_mut().unwrap();
        let mut packet = build_encrypted_packet(&[1, 2, 3], cipher, &sender.encrypt_mac_key, 0, &mut rng);
        let last = packet.len() - 1;
        packet[last] ^= 0xFF; // flip a MAC byte

        assert!(take_encrypted_packet(&mut packet, &mut receiver).is_none());
        assert_eq!(receiver.decrypt_seq, 0);
    }

    /// `packet_len` 0 or 1 must be rejected, not index `decrypted[4]`/`[5]`
    /// past the end of a `4 + packet_len`-byte buffer and panic. Regression
    /// test for a real crash: `packet_len=1, padding_len=0` passes the
    /// `padding_len >= packet_len` guard (0 is not `>= 1`) and only fails on
    /// the `decrypted[5]` (msg_type) index, one byte past a 5-byte buffer.
    #[test]
    fn encrypted_packet_rejects_packet_len_too_small_to_hold_a_message_without_panicking() {
        let (mut sender, mut receiver) = test_crypto_pair();

        for packet_len in [0u32, 1] {
            let cipher = sender.encrypt_cipher.as_mut().unwrap();
            let mut len_buf = packet_len.to_be_bytes();
            cipher.clone().apply_keystream(&mut len_buf); // encrypt just the length field the same way build_encrypted_packet would
            let mut packet = len_buf.to_vec();
            packet.extend_from_slice(&[0u8; MAC_SIZE]); // garbage MAC; must be rejected before it's even checked
            assert!(
                take_encrypted_packet(&mut packet, &mut receiver).is_none(),
                "packet_len={packet_len} must be rejected, not panic"
            );
        }
    }

    #[test]
    fn unencrypted_packet_rejects_packet_len_too_small_to_hold_a_message_without_panicking() {
        let mut crypto = CryptoState::new();
        for packet_len in [0u32, 1] {
            let mut input_buffer = packet_len.to_be_bytes().to_vec();
            input_buffer.push(0); // one content byte, enough to pass the initial `len < 5` check for packet_len=1
            assert!(
                take_unencrypted_packet(&mut input_buffer, &mut crypto).is_none(),
                "packet_len={packet_len} must be rejected, not panic"
            );
        }
    }

    /// A malicious/corrupt `padding_len >= packet_len` must be rejected, not
    /// underflow `packet_len - padding_len - 1` and panic — see the doc
    /// comment on `take_encrypted_packet` and `docs/PROTOCOL_UNDER_LOAD.md`.
    #[test]
    fn unencrypted_packet_rejects_oversized_padding_len_without_panicking() {
        let mut crypto = CryptoState::new();
        // packet_len = 4, padding_len = 4 (>= packet_len): payload_len would
        // underflow if unchecked.
        let mut input_buffer = vec![0, 0, 0, 4, 4, 0, 0, 0];
        assert!(take_unencrypted_packet(&mut input_buffer, &mut crypto).is_none());
    }

    #[test]
    fn unencrypted_packet_round_trips_via_build_packet() {
        let mut crypto = CryptoState::new();
        let payload = vec![7u8, 1, 2, 3];
        let mut input_buffer = build_packet(&payload);
        let (msg_type, got_payload) =
            take_unencrypted_packet(&mut input_buffer, &mut crypto).expect("full packet");
        assert_eq!(msg_type, 7);
        assert_eq!(got_payload, vec![1, 2, 3]);
        assert!(input_buffer.is_empty());
        assert_eq!(crypto.decrypt_seq, 1);
    }

    #[test]
    fn unencrypted_packet_returns_none_when_partial() {
        let mut crypto = CryptoState::new();
        let full = build_packet(&[1, 2, 3]);
        let mut input_buffer = full[..full.len() - 2].to_vec();
        assert!(take_unencrypted_packet(&mut input_buffer, &mut crypto).is_none());
        assert_eq!(crypto.decrypt_seq, 0);
    }
}
