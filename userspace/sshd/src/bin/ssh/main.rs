//! `ssh` — a minimal interactive SSH-2 client for Akuma OS.
//!
//! Same package as `sshd` (`userspace/sshd`), built as a second binary
//! target rather than a separate crate: the two sides of the same wire
//! format belong together, and this way `cargo build --release -p sshd`
//! (already the whole build's normal path) produces both for free. See
//! `userspace/sshd/README.md` for scope/usage and
//! `docs/reference/subsystems/ssh.md` for the shared protocol reference.
//!
//! Ed25519/curve25519-sha256/aes128-ctr/hmac-sha2-256 only (the same suite
//! `sshd` speaks) — no cipher negotiation, no SFTP/SCP, no port/agent/X11
//! forwarding. Interactive shell (with a pty) or a single remote command
//! (`ssh host cmd`, no pty), which is what "terminal features only" means
//! here.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

mod crypto;
mod keys;
mod protocol;

use libakuma::*;

fn print_usage() {
    println("Usage: ssh [-p port] [-l user] [-i identity_file] [-t term] [user@]host [command...]");
    println("");
    println("  -p port           Port to connect to (default 22)");
    println("  -l user           Remote login name (default: root, or from user@host)");
    println("  -i identity_file  Ed25519 identity file, Akuma's raw 32-byte format");
    println("                    (default: $HOME/.ssh/id_ed25519, falling back to");
    println("                    /etc/sshd/id_ed25519, i.e. sshd's own host key)");
    println("  -t term           TERM to advertise to the remote pty (default xterm-256color)");
    println("");
    println("With no command, opens an interactive shell over a pty.");
    println("With a command, runs it non-interactively (no pty) and exits with its status.");
}

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    let mut args_iter = args();
    args_iter.next(); // argv[0]

    let mut port: u16 = 22;
    let mut login: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut term = String::from("xterm-256color");
    let mut host_arg: Option<String> = None;
    let mut command_parts: Vec<String> = Vec::new();

    while let Some(a) = args_iter.next() {
        match a {
            "-p" => {
                if let Some(v) = args_iter.next() {
                    match v.parse::<u16>() {
                        Ok(p) => port = p,
                        Err(_) => {
                            eprintln(&format!("ssh: invalid port '{v}'"));
                            exit(1);
                        }
                    }
                }
            }
            "-l" => {
                if let Some(v) = args_iter.next() {
                    login = Some(String::from(v));
                }
            }
            "-i" => {
                if let Some(v) = args_iter.next() {
                    identity = Some(String::from(v));
                }
            }
            "-t" => {
                if let Some(v) = args_iter.next() {
                    term = String::from(v);
                }
            }
            "-h" | "--help" => {
                print_usage();
                return;
            }
            _ if host_arg.is_none() => host_arg = Some(String::from(a)),
            other => command_parts.push(String::from(other)),
        }
    }

    let Some(host_arg) = host_arg else {
        print_usage();
        exit(1);
    };

    let (user_from_host, host) = match host_arg.split_once('@') {
        Some((u, h)) => (Some(String::from(u)), String::from(h)),
        None => (None, host_arg),
    };
    let username = login.or(user_from_host).unwrap_or_else(|| String::from("root"));
    let command = if command_parts.is_empty() {
        None
    } else {
        Some(command_parts.join(" "))
    };

    let cfg = protocol::ClientConfig { host, port, username, identity, command, term };

    match protocol::run(cfg) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln(&format!("ssh: {e}"));
            exit(255);
        }
    }
}
