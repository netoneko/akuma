use alloc::vec;

use crate::config::SshdConfig;
use crate::constants::SSH_MSG_KEXINIT;
use crate::kex::build_kexinit;
use crate::packet::{process_encrypted_packet, process_unencrypted_packet};
use crate::session::{SshSession, SshState};
use crate::util::translate_input_keys;

use akuma_ssh_crypto::crypto::{
    SimpleRng, build_packet, build_encrypted_packet,
    MAC_KEY_SIZE, AES_KEY_SIZE, AES_IV_SIZE, Aes128Ctr,
};
use ctr::cipher::KeyIvInit;

fn test_rng() -> SimpleRng {
    SimpleRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8])
}

fn test_session() -> SshSession {
    SshSession::new(SshdConfig::default(), None, test_rng())
}

#[test]
fn session_initial_state() {
    let session = test_session();
    assert_eq!(session.state, SshState::AwaitingVersion);
    assert!(!session.channel_open);
    assert!(session.channel_data_buffer.is_empty());
    assert_eq!(session.term_width, 80);
    assert_eq!(session.term_height, 24);
}

#[test]
fn config_parse_empty() {
    let config = SshdConfig::parse("");
    assert!(!config.disable_key_verification);
    assert!(config.shell.is_none());
}

#[test]
fn config_parse_basic() {
    let content = "disable_key_verification = true\nshell = /bin/dash\n";
    let config = SshdConfig::parse(content);
    assert!(config.disable_key_verification);
    assert_eq!(config.shell.as_deref(), Some("/bin/dash"));
}

#[test]
fn config_parse_comments_and_whitespace() {
    let content = "# comment\n\n  disable_key_verification = yes  \n";
    let config = SshdConfig::parse(content);
    assert!(config.disable_key_verification);
}

#[test]
fn build_kexinit_structure() {
    let mut rng = test_rng();
    let payload = build_kexinit(&mut rng);

    assert_eq!(payload[0], SSH_MSG_KEXINIT);
    assert!(payload.len() > 17);
}

#[test]
fn unencrypted_packet_round_trip() {
    let mut session = test_session();

    let payload = b"\x14test-payload";
    let packet = build_packet(payload);
    session.input_buffer.extend_from_slice(&packet);

    let result = process_unencrypted_packet(&mut session);
    assert!(result.is_some());
    let (msg_type, data) = result.unwrap();
    assert_eq!(msg_type, 0x14);
    assert_eq!(&data, b"test-payload");
    assert!(session.input_buffer.is_empty());
}

#[test]
fn unencrypted_packet_incomplete() {
    let mut session = test_session();

    session.input_buffer.extend_from_slice(&[0, 0, 0, 20]);

    let result = process_unencrypted_packet(&mut session);
    assert!(result.is_none());
}

#[test]
fn encrypted_packet_round_trip() {
    let mut session = test_session();
    let mut rng = test_rng();

    let key = [0x42u8; AES_KEY_SIZE];
    let iv = [0x13u8; AES_IV_SIZE];
    let mac_key = [0xABu8; MAC_KEY_SIZE];

    session.crypto.encrypt_cipher = Some(Aes128Ctr::new((&key).into(), (&iv).into()));
    session.crypto.encrypt_mac_key = mac_key;
    session.crypto.encrypt_seq = 0;

    session.crypto.decrypt_cipher = Some(Aes128Ctr::new((&key).into(), (&iv).into()));
    session.crypto.decrypt_mac_key = mac_key;
    session.crypto.decrypt_seq = 0;

    let payload = b"\x15hello-encrypted";
    let seq = session.crypto.encrypt_seq;
    session.crypto.encrypt_seq = seq.wrapping_add(1);
    let packet = build_encrypted_packet(
        payload,
        session.crypto.encrypt_cipher.as_mut().unwrap(),
        &session.crypto.encrypt_mac_key,
        seq,
        &mut rng,
    );

    session.input_buffer.extend_from_slice(&packet);

    let result = process_encrypted_packet(&mut session);
    assert!(result.is_some());
    let (msg_type, data) = result.unwrap();
    assert_eq!(msg_type, 0x15);
    assert_eq!(&data, b"hello-encrypted");
    assert!(session.input_buffer.is_empty());
}

#[test]
fn translate_input_keys_delete() {
    let input = b"\x1b[3~";
    let result = translate_input_keys(input);
    assert_eq!(result, vec![0x7f]);
}

#[test]
fn translate_input_keys_passthrough() {
    let input = b"hello";
    let result = translate_input_keys(input);
    assert_eq!(result, b"hello");
}

#[test]
fn translate_input_keys_mixed() {
    let input = b"ab\x1b[3~cd";
    let result = translate_input_keys(input);
    assert_eq!(result, b"ab\x7fcd");
}
