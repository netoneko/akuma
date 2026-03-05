use alloc::vec::Vec;
use akuma_ssh_crypto::crypto::{CryptoState, SimpleRng};
use ed25519_dalek::SigningKey;

use crate::config::SshdConfig;
use crate::constants::SSH_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshState {
    AwaitingVersion,
    AwaitingKexInit,
    AwaitingKexEcdhInit,
    AwaitingNewKeys,
    AwaitingServiceRequest,
    AwaitingUserAuth,
    Authenticated,
    Disconnected,
}

pub struct SshSession {
    pub state: SshState,
    pub rng: SimpleRng,
    pub client_version: Vec<u8>,
    pub server_version: Vec<u8>,
    pub client_kexinit: Vec<u8>,
    pub server_kexinit: Vec<u8>,
    pub session_id: [u8; 32],
    pub host_key: Option<SigningKey>,
    pub crypto: CryptoState,
    pub input_buffer: Vec<u8>,
    pub channel_open: bool,
    pub client_channel: u32,
    pub channel_data_buffer: Vec<u8>,
    pub channel_eof: bool,
    pub term_width: u32,
    pub term_height: u32,
    pub resize_pending: bool,
    pub config: SshdConfig,
}

impl SshSession {
    #[must_use]
    pub fn new(config: SshdConfig, host_key: Option<SigningKey>, rng: SimpleRng) -> Self {
        Self {
            state: SshState::AwaitingVersion,
            rng,
            client_version: Vec::new(),
            server_version: SSH_VERSION[..SSH_VERSION.len() - 2].to_vec(),
            client_kexinit: Vec::new(),
            server_kexinit: Vec::new(),
            session_id: [0u8; 32],
            host_key,
            crypto: CryptoState::new(),
            input_buffer: Vec::new(),
            channel_open: false,
            client_channel: 0,
            channel_data_buffer: Vec::new(),
            channel_eof: false,
            term_width: 80,
            term_height: 24,
            resize_pending: false,
            config,
        }
    }
}
