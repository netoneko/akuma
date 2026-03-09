use alloc::vec::Vec;
use akuma_ssh_crypto::crypto::{CryptoState, SimpleRng};
use ed25519_dalek::SigningKey;

use crate::config::SshdConfig;
use crate::constants::SSH_VERSION;

/// Maximum size for the SSH input buffer (pending undecoded data).
/// Protects against a malicious or misbehaving client flooding the kernel.
pub const INPUT_BUFFER_MAX: usize = 256 * 1024; // 256 KB

/// Maximum size for the channel data buffer (decoded terminal input).
pub const CHANNEL_DATA_BUFFER_MAX: usize = 64 * 1024; // 64 KB

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
    /// Append data to the input buffer, enforcing the size limit.
    /// Returns false if the buffer is full and data was dropped.
    pub fn feed_input(&mut self, data: &[u8]) -> bool {
        if self.input_buffer.len() + data.len() > INPUT_BUFFER_MAX {
            log::warn!("[SSH] Input buffer overflow ({} + {} > {}), dropping data",
                self.input_buffer.len(), data.len(), INPUT_BUFFER_MAX);
            return false;
        }
        self.input_buffer.extend_from_slice(data);
        true
    }

    /// Append data to the channel data buffer, enforcing the size limit.
    /// Returns false if the buffer is full and data was dropped.
    pub fn feed_channel_data(&mut self, data: &[u8]) -> bool {
        if self.channel_data_buffer.len() + data.len() > CHANNEL_DATA_BUFFER_MAX {
            log::warn!("[SSH] Channel data buffer overflow, dropping data");
            return false;
        }
        self.channel_data_buffer.extend_from_slice(data);
        true
    }

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
