//! `ssh-keygen`, folded into the `ssh` binary (busybox-style multicall), for Akuma.
//!
//! Reached two ways: the binary is invoked under the name `ssh-keygen` (a symlink to `ssh`, the usual
//! name, so scripts written for OpenSSH work unchanged), or as `ssh --keygen ...` (the Akuma-native
//! form; a bare `ssh keygen` would be ambiguous with a host called `keygen`). Writes what OpenSSH's
//! `ssh-keygen -t ed25519 -N ''` writes — an unencrypted `openssh-key-v1` private key (mode 0600)
//! and a `<file>.pub` line — so a key made here works with any OpenSSH client and server, and the
//! Akuma `ssh` client (which reads that format) can use it with `-i`. Scope is deliberately
//! small: Ed25519 only, no passphrase (`-N ''` is required, as is any non-interactive caller's),
//! no certificates, no known_hosts tooling.
//!
//! ```text
//! ssh-keygen [-q] -t ed25519 -N '' [-C comment] [-f path]     make a key (refuses to overwrite)
//! ssh-keygen -y [-f path]                                     print the public line of a key
//! ssh --keygen <the same arguments>
//! ```
//!
//! The pure parts (the container writer and parser) are `akuma_ssh_crypto::keys`, host-tested
//! against real `ssh-keygen` output, so what is left here is argument handling and file I/O.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use akuma_ssh_crypto::keys::{
    encode_openssh_private_key_ed25519, encode_public_key_ssh, looks_like_openssh_private_key,
    parse_openssh_private_key_ed25519,
};
use ed25519_dalek::SigningKey;
use libakuma::*;

pub fn usage() {
    println("Usage: ssh-keygen [-q] -t ed25519 -N '' [-C comment] [-f path]");
    println("       ssh-keygen -y [-f path]");
    println("");
    println("  -t ed25519   key type (the only one)");
    println("  -N ''        passphrase; must be empty, passphrases are not supported");
    println("  -C comment   comment stored in the key and its .pub line (default: akuma)");
    println("  -f path      key file (default: $HOME/.ssh/id_ed25519); path.pub is written beside it");
    println("  -q           quiet");
    println("  -y           print the public key line for the private key at -f");
    println("");
    println("Writes an unencrypted OpenSSH private key (mode 0600) and refuses to overwrite one.");
}

fn read_file(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(fd);
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let n = read_fd(fd, &mut buf);
        if n < 0 {
            close(fd);
            return Err(n as i32);
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Ok(out)
}

fn write_file(path: &str, data: &[u8], mode: u32) -> Result<(), i32> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(fd);
    }
    let n = write_fd(fd, data);
    close(fd);
    if n < 0 || (n as usize) != data.len() {
        return Err(if n < 0 { n as i32 } else { -5 });
    }
    // The key is created before its mode is set; that window is short and the file is empty of
    // anything useful until the write above, which happens after the open. Tighten at once.
    let rc = chmod(path, mode);
    if rc < 0 {
        return Err(rc);
    }
    Ok(())
}

fn die(msg: &str) -> ! {
    eprintln(&format!("ssh-keygen: {msg}"));
    exit(1);
}

fn default_path() -> String {
    format!("{}/.ssh/id_ed25519", libakuma::env("HOME").unwrap_or("/root"))
}

fn exists(path: &str) -> bool {
    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        true
    } else {
        false
    }
}

/// Load `path` as raw 32 bytes or an OpenSSH container.
fn load_key(path: &str) -> SigningKey {
    let data = match read_file(path) {
        Ok(d) => d,
        Err(e) => die(&format!("{path}: cannot read (errno {e})")),
    };
    if data.len() == 32 {
        let seed: [u8; 32] = data.try_into().unwrap_or([0u8; 32]);
        return SigningKey::from_bytes(&seed);
    }
    let Ok(text) = core::str::from_utf8(&data) else { die(&format!("{path}: not a key file")) };
    if !looks_like_openssh_private_key(text) {
        die(&format!("{path}: not an OpenSSH private key"));
    }
    match parse_openssh_private_key_ed25519(text) {
        Ok(seed) => SigningKey::from_bytes(&seed),
        Err(e) => die(&format!("{path}: {}", e.describe())),
    }
}

/// Entry point: `argv` is everything after the program name (or after `--keygen`).
pub fn run(argv: Vec<String>) {
    let mut key_type = String::new();
    let mut passphrase_given = false;
    let mut comment = String::from("akuma");
    let mut path: Option<String> = None;
    let mut quiet = false;
    let mut print_public = false;

    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        // The value of a flag that takes one. An empty value (`-N ''`) may have been dropped by the
        // caller's spawn, so a following flag means the value was empty.
        let next_is_value = |j: usize| j + 1 < argv.len() && !argv[j + 1].starts_with('-');
        match a {
            "-q" => quiet = true,
            "-y" => print_public = true,
            "-h" | "--help" => {
                usage();
                return;
            }
            "-t" => {
                if !next_is_value(i) {
                    die("-t needs a key type");
                }
                i += 1;
                key_type = argv[i].clone();
            }
            "-C" => {
                if next_is_value(i) {
                    i += 1;
                    comment = argv[i].clone();
                } else {
                    comment = String::new();
                }
            }
            "-f" => {
                if !next_is_value(i) {
                    die("-f needs a path");
                }
                i += 1;
                path = Some(argv[i].clone());
            }
            "-N" => {
                passphrase_given = true;
                if next_is_value(i) {
                    i += 1;
                    if !argv[i].is_empty() {
                        die("passphrases are not supported: use -N ''");
                    }
                }
            }
            other => die(&format!("unknown argument '{other}' (see ssh-keygen -h)")),
        }
        i += 1;
    }

    let path = path.unwrap_or_else(default_path);

    if print_public {
        let key = load_key(&path);
        let mut line = encode_public_key_ssh(&key.verifying_key());
        if !comment.is_empty() && comment != "akuma" {
            line.push(' ');
            line.push_str(&comment);
        }
        println(&line);
        return;
    }

    if key_type != "ed25519" {
        die("only '-t ed25519' is supported");
    }
    if !passphrase_given {
        die("passphrases are not supported: pass -N '' explicitly");
    }
    if exists(&path) {
        die(&format!("{path} already exists; not overwriting"));
    }
    if let Some(slash) = path.rfind('/')
        && slash > 0
        && !mkdir_p(&path[..slash])
    {
        die(&format!("cannot create {}", &path[..slash]));
    }

    let mut seed = [0u8; 32];
    let mut check = [0u8; 4];
    if getrandom(&mut seed).is_err() || getrandom(&mut check).is_err() {
        die("no secure random bytes available");
    }
    let pem = encode_openssh_private_key_ed25519(&SigningKey::from_bytes(&seed).to_bytes(), &comment, u32::from_be_bytes(check));
    if let Err(e) = write_file(&path, pem.as_bytes(), 0o600) {
        die(&format!("cannot write {path} (errno {e})"));
    }
    let mut pub_line = encode_public_key_ssh(&SigningKey::from_bytes(&seed).verifying_key());
    if !comment.is_empty() {
        pub_line.push(' ');
        pub_line.push_str(&comment);
    }
    pub_line.push('\n');
    if let Err(e) = write_file(&format!("{path}.pub"), pub_line.as_bytes(), 0o644) {
        die(&format!("cannot write {path}.pub (errno {e})"));
    }
    if !quiet {
        println(&format!("Your identification has been saved in {path}"));
        println(&format!("Your public key has been saved in {path}.pub"));
    }
}
