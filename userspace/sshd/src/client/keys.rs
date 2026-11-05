//! Client identity key and `known_hosts` handling for the `ssh` binary.
//!
//! An identity file is either Akuma's raw 32-byte Ed25519 seed (the format `sshd` uses for its
//! own host key, `userspace/sshd/src/keys.rs`) or a standard **unencrypted OpenSSH private key**
//! (`-----BEGIN OPENSSH PRIVATE KEY-----`, what `ssh-keygen -t ed25519 -N ''` writes), so a key
//! made on any other machine can be copied over and used with `-i` unchanged. Passphrase-protected
//! keys and key types other than Ed25519 are rejected with a message that says so; the parsing is
//! `akuma_ssh_crypto::keys::parse_openssh_private_key_ed25519` (host-tested against real
//! `ssh-keygen` output).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey, VerifyingKey};

pub use akuma_ssh_crypto::keys::{base64_encode, encode_public_key_ssh, parse_public_key_ssh};
use akuma_ssh_crypto::keys::{looks_like_openssh_private_key, parse_openssh_private_key_ed25519};

use libakuma::*;

/// `/etc/sshd/id_ed25519` — the host key `sshd` generates on first run
/// (`userspace/sshd/src/keys.rs::load_or_generate_host_key`). Reused as the
/// client's default identity when the user has none of their own, so a fresh
/// box can `ssh` out without a separate key-generation step.
const SSHD_HOST_KEY_PATH: &str = "/etc/sshd/id_ed25519";

fn home_dir() -> String {
    String::from(libakuma::env("HOME").unwrap_or("/root"))
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(fd);
    }
    let mut result = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut buf);
        if n < 0 {
            close(fd);
            return Err(n as i32);
        }
        if n == 0 {
            break;
        }
        result.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Ok(result)
}

fn write_vec_to_file(path: &str, data: &[u8]) -> Result<(), i32> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(fd);
    }
    let n = write_fd(fd, data);
    close(fd);
    if n < 0 { Err(n as i32) } else { Ok(()) }
}

/// Directory portion of a path (everything before the last `/`), for
/// `mkdir_p` — `None` if there is no `/` to split on.
fn parent_dir(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// Load an Ed25519 identity from `path`: Akuma's raw 32-byte seed, or an unencrypted OpenSSH
/// private key. `None` when the file is missing or is neither; an OpenSSH file that is
/// recognisable but unusable (passphrase, wrong key type, damaged) says why on stderr, because
/// otherwise the caller's fallback to a default key looks like a mysterious authentication
/// failure further down.
fn try_load_key(path: &str) -> Option<SigningKey> {
    let data = read_file_to_vec(path).ok()?;
    if data.len() == SECRET_KEY_LENGTH {
        let bytes: [u8; SECRET_KEY_LENGTH] = data.try_into().ok()?;
        return Some(SigningKey::from_bytes(&bytes));
    }
    let text = core::str::from_utf8(&data).ok()?;
    if !looks_like_openssh_private_key(text) {
        return None;
    }
    match parse_openssh_private_key_ed25519(text) {
        Ok(seed) => Some(SigningKey::from_bytes(&seed)),
        Err(e) => {
            eprintln(&format!("ssh: {path}: {}", e.describe()));
            None
        }
    }
}

/// Generates and persists a new identity key. `None` if secure randomness
/// wasn't available — this is the long-term key that authenticates every
/// future connection, so (like the ephemeral KEX secret in `protocol.rs`)
/// it must come from real hardware entropy, never `SimpleRng` (64 bits of
/// state is nowhere near enough for a key meant to last).
fn generate_and_save(path: &str) -> Option<SigningKey> {
    let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
    if libakuma::getrandom(&mut key_bytes).is_err() {
        eprintln("ssh: couldn't obtain secure random bytes to generate an identity key");
        return None;
    }
    let key = SigningKey::from_bytes(&key_bytes);

    if let Some(dir) = parent_dir(path)
        && !mkdir_p(dir)
    {
        eprintln(&format!("ssh: warning: couldn't create {dir}"));
    }
    // A failure here is not cosmetic: the caller returns `key` regardless, so
    // a silent failure would mean this "persisted" identity actually isn't —
    // the next invocation generates and signs with a *different* key with no
    // indication why the remote side suddenly rejects it.
    if let Err(e) = write_vec_to_file(path, key.as_bytes()) {
        eprintln(&format!(
            "ssh: warning: couldn't save new identity key to {path} (errno {e}); \
             it will NOT persist past this connection"
        ));
    }
    let pub_line = format!("{}\n", encode_public_key_ssh(&key.verifying_key()));
    if let Err(e) = write_vec_to_file(&format!("{path}.pub"), pub_line.as_bytes()) {
        eprintln(&format!("ssh: warning: couldn't save {path}.pub (errno {e})"));
    }
    Some(key)
}

/// Resolve the client identity key.
///
/// Precedence: an explicit `-i` path, then `$HOME/.ssh/id_ed25519` (default
/// `$HOME` is `/root`), then `sshd`'s own host key
/// (`/etc/sshd/id_ed25519`) if that's the only key already on the box, and
/// finally a freshly generated key persisted to `$HOME/.ssh/id_ed25519`.
///
/// `None` only when no key could be loaded *and* none could be generated
/// (no secure randomness available) — the caller must treat that as fatal,
/// not fall back to any kind of default identity.
pub fn load_identity(explicit: Option<&str>) -> Option<SigningKey> {
    if let Some(p) = explicit {
        if let Some(k) = try_load_key(p) {
            return Some(k);
        }
        eprintln(&format!(
            "ssh: identity file '{p}' not found or not a usable Ed25519 key (raw 32 bytes or unencrypted OpenSSH); falling back to defaults"
        ));
    }

    let user_key_path = format!("{}/.ssh/id_ed25519", home_dir());
    if let Some(k) = try_load_key(&user_key_path) {
        return Some(k);
    }

    if let Some(k) = try_load_key(SSHD_HOST_KEY_PATH) {
        println(&format!(
            "[ssh] no {user_key_path}; using sshd's host key ({SSHD_HOST_KEY_PATH}) as identity"
        ));
        return Some(k);
    }

    println(&format!("[ssh] generating new identity key at {user_key_path}"));
    generate_and_save(&user_key_path)
}

// ============================================================================
// known_hosts (TOFU)
// ============================================================================

pub fn known_hosts_path() -> String {
    format!("{}/.ssh/known_hosts", home_dir())
}

pub enum HostKeyStatus {
    /// No entry for this host at all.
    New,
    /// Entry present and matches.
    Known,
    /// Entry present but the key differs — possible MITM.
    Mismatch,
}

/// Look up `host_spec` (typically `"host:port"`) in `known_hosts`.
pub fn lookup_known_host(host_spec: &str, key: &VerifyingKey) -> HostKeyStatus {
    let Ok(data) = read_file_to_vec(&known_hosts_path()) else {
        return HostKeyStatus::New;
    };
    let Ok(content) = core::str::from_utf8(&data) else {
        return HostKeyStatus::New;
    };

    let mut saw_host = false;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[0] != host_spec {
            continue;
        }
        let rest = format!("{} {}", parts[1], parts[2]);
        if let Some(stored) = parse_public_key_ssh(&rest) {
            saw_host = true;
            if stored.as_bytes() == key.as_bytes() {
                return HostKeyStatus::Known;
            }
        }
    }
    if saw_host { HostKeyStatus::Mismatch } else { HostKeyStatus::New }
}

/// Append a new `known_hosts` entry (TOFU acceptance).
///
/// A failure here is surfaced loudly rather than swallowed: this is the only
/// record that we've already trusted this host key, so a silent failure
/// means the next connection re-runs the TOFU prompt with no memory of this
/// one — which reads as "the host key changed" if the operator isn't
/// watching closely.
pub fn add_known_host(host_spec: &str, key: &VerifyingKey) {
    let path = known_hosts_path();
    if let Some(dir) = parent_dir(&path)
        && !mkdir_p(dir)
    {
        eprintln(&format!("ssh: warning: couldn't create {dir}; not recording host key"));
        return;
    }
    let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND);
    if fd < 0 {
        eprintln(&format!("ssh: warning: couldn't open {path} to record host key (fd {fd})"));
        return;
    }
    let line = format!("{} {}\n", host_spec, encode_public_key_ssh(key));
    let n = write_fd(fd, line.as_bytes());
    close(fd);
    if n < 0 || (n as usize) < line.len() {
        eprintln(&format!(
            "ssh: warning: only wrote {n} of {} bytes recording the host key in {path}",
            line.len()
        ));
    }
}

/// `SHA256:<base64>` fingerprint of a host key blob, for the TOFU prompt —
/// same format `ssh-keygen -lf` prints.
pub fn fingerprint_sha256(key: &VerifyingKey) -> String {
    use sha2::{Digest, Sha256};
    let mut blob = Vec::new();
    crate::crypto::write_string(&mut blob, b"ssh-ed25519");
    crate::crypto::write_string(&mut blob, key.as_bytes());
    let digest = Sha256::digest(&blob);
    format!("SHA256:{}", base64_encode(&digest))
}
