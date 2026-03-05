pub const SSH_VERSION: &[u8] = b"SSH-2.0-Akuma_0.1\r\n";

pub const SSH_MSG_DISCONNECT: u8 = 1;
pub const SSH_MSG_IGNORE: u8 = 2;
pub const SSH_MSG_UNIMPLEMENTED: u8 = 3;
pub const SSH_MSG_DEBUG: u8 = 4;
pub const SSH_MSG_SERVICE_REQUEST: u8 = 5;
pub const SSH_MSG_SERVICE_ACCEPT: u8 = 6;
pub const SSH_MSG_KEXINIT: u8 = 20;
pub const SSH_MSG_NEWKEYS: u8 = 21;
pub const SSH_MSG_KEX_ECDH_INIT: u8 = 30;
#[allow(dead_code)]
pub const SSH_MSG_KEX_ECDH_REPLY: u8 = 31;
pub const SSH_MSG_USERAUTH_REQUEST: u8 = 50;
#[allow(dead_code)]
pub const SSH_MSG_USERAUTH_SUCCESS: u8 = 52;
pub const SSH_MSG_GLOBAL_REQUEST: u8 = 80;
pub const SSH_MSG_REQUEST_FAILURE: u8 = 82;
pub const SSH_MSG_CHANNEL_OPEN: u8 = 90;
pub const SSH_MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub const SSH_MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
pub const SSH_MSG_CHANNEL_DATA: u8 = 94;
pub const SSH_MSG_CHANNEL_EOF: u8 = 96;
pub const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
pub const SSH_MSG_CHANNEL_REQUEST: u8 = 98;
pub const SSH_MSG_CHANNEL_SUCCESS: u8 = 99;
pub const SSH_MSG_CHANNEL_FAILURE: u8 = 100;

pub const KEX_ALGO: &str = "curve25519-sha256";
pub const HOST_KEY_ALGO: &str = "ssh-ed25519";
pub const CIPHER_ALGO: &str = "aes128-ctr";
pub const MAC_ALGO: &str = "hmac-sha2-256";
pub const COMPRESS_ALGO: &str = "none";
