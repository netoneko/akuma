//! SSH Server Implementation
//!
//! This module provides a complete SSH-2 server implementation with:
//! - curve25519-sha256 key exchange
//! - ssh-ed25519 host key authentication
//! - aes128-ctr encryption
//! - hmac-sha2-256 MAC
//! - Public key client authentication
//! - Shell with basic commands
//! - Multiple concurrent sessions

pub mod auth;
pub mod config;
pub mod crypto;
pub mod keys;
pub mod protocol;
pub mod server;

// Re-export commonly used items
pub use protocol::init_host_key;
pub use server::run;
