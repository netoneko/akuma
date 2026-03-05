use alloc::vec::Vec;
use core::convert::TryInto;

use akuma_ssh_crypto::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, MAC_KEY_SIZE, SimpleRng,
    derive_key, write_namelist, write_string, write_u32,
};
use ctr::cipher::KeyIvInit;
use ed25519_dalek::Signer;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;

use crate::constants::{
    SSH_MSG_KEXINIT, SSH_MSG_KEX_ECDH_REPLY, KEX_ALGO, HOST_KEY_ALGO,
    CIPHER_ALGO, MAC_ALGO, COMPRESS_ALGO,
};
use crate::session::SshSession;

pub fn build_kexinit(rng: &mut SimpleRng) -> Vec<u8> {
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

pub fn handle_kex_ecdh_init(session: &mut SshSession, client_pubkey: &[u8]) -> Option<Vec<u8>> {
    let mut secret_bytes = [0u8; 32];
    session.rng.fill_bytes(&mut secret_bytes);

    let server_secret = x25519_dalek::StaticSecret::from(secret_bytes);
    let server_public = X25519PublicKey::from(&server_secret);
    let server_pubkey = server_public.as_bytes();

    let client_pubkey_bytes: [u8; 32] = client_pubkey.try_into().ok()?;
    let client_public = X25519PublicKey::from(client_pubkey_bytes);

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

    let iv_c2s = derive_key(&shared_secret, &exchange_hash, b'A', &session.session_id, AES_IV_SIZE);
    let iv_s2c = derive_key(&shared_secret, &exchange_hash, b'B', &session.session_id, AES_IV_SIZE);
    let key_c2s = derive_key(&shared_secret, &exchange_hash, b'C', &session.session_id, AES_KEY_SIZE);
    let key_s2c = derive_key(&shared_secret, &exchange_hash, b'D', &session.session_id, AES_KEY_SIZE);
    let mac_c2s = derive_key(&shared_secret, &exchange_hash, b'E', &session.session_id, MAC_KEY_SIZE);
    let mac_s2c = derive_key(&shared_secret, &exchange_hash, b'F', &session.session_id, MAC_KEY_SIZE);

    session.crypto.decrypt_cipher = Some(Aes128Ctr::new(
        key_c2s[..AES_KEY_SIZE].into(),
        iv_c2s[..AES_IV_SIZE].into(),
    ));
    session.crypto.decrypt_mac_key.copy_from_slice(&mac_c2s[..MAC_KEY_SIZE]);

    session.crypto.encrypt_cipher = Some(Aes128Ctr::new(
        key_s2c[..AES_KEY_SIZE].into(),
        iv_s2c[..AES_IV_SIZE].into(),
    ));
    session.crypto.encrypt_mac_key.copy_from_slice(&mac_s2c[..MAC_KEY_SIZE]);

    let mut reply = Vec::new();
    reply.push(SSH_MSG_KEX_ECDH_REPLY);
    write_string(&mut reply, &host_key_blob);
    write_string(&mut reply, server_pubkey);
    write_string(&mut reply, &sig_blob);

    Some(reply)
}
