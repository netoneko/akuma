//! SSH-2 Protocol Implementation (Userspace)

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;

use ed25519_dalek::{SigningKey, Signer};
use embedded_io_async::{Read, Write};
use hmac::Mac;
use sha2::{Digest, Sha256};
use x25519_dalek::PublicKey as X25519PublicKey;

use super::auth::{self, AuthResult};
use super::config::SshdConfig;
use super::crypto::{
    AES_IV_SIZE, AES_KEY_SIZE, Aes128Ctr, CryptoState, HmacSha256, MAC_KEY_SIZE, MAC_SIZE,
    SimpleRng, build_encrypted_packet, build_packet, derive_key, read_string, read_u32,
    write_namelist, write_string, write_u32,
};
use super::keys;
// Channel-message byte layout lives in the crate's lib target so it can be
// unit-tested on the host — see `wire.rs`.
use sshd::wire;
use crate::SshStream;
use libakuma::*;
use libakuma::net::Error as NetError;

// ============================================================================
// SSH Constants
// ============================================================================

const SSH_VERSION: &[u8] = b"SSH-2.0-Akuma_0.1\r\n";

const SSH_MSG_DISCONNECT: u8 = 1;
const SSH_MSG_SERVICE_REQUEST: u8 = 5;
const SSH_MSG_SERVICE_ACCEPT: u8 = 6;
const SSH_MSG_KEXINIT: u8 = 20;
const SSH_MSG_NEWKEYS: u8 = 21;
const SSH_MSG_KEX_ECDH_INIT: u8 = 30;
const SSH_MSG_KEX_ECDH_REPLY: u8 = 31;
const SSH_MSG_USERAUTH_REQUEST: u8 = 50;
const SSH_MSG_CHANNEL_OPEN: u8 = 90;
const SSH_MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const SSH_MSG_CHANNEL_DATA: u8 = 94;
const SSH_MSG_CHANNEL_EOF: u8 = 96;
const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
const SSH_MSG_CHANNEL_REQUEST: u8 = 98;
const SSH_MSG_CHANNEL_SUCCESS: u8 = 99;

const KEX_ALGO: &str = "curve25519-sha256";
const HOST_KEY_ALGO: &str = "ssh-ed25519";
const CIPHER_ALGO: &str = "aes128-ctr";
const MAC_ALGO: &str = "hmac-sha2-256";
const COMPRESS_ALGO: &str = "none";

// ============================================================================
// SSH Session
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshState {
    AwaitingVersion,
    AwaitingKexInit,
    AwaitingKexEcdhInit,
    AwaitingNewKeys,
    AwaitingServiceRequest,
    AwaitingUserAuth,
    Authenticated,
}

struct SshSession {
    state: SshState,
    rng: SimpleRng,
    client_version: Vec<u8>,
    server_version: Vec<u8>,
    client_kexinit: Vec<u8>,
    server_kexinit: Vec<u8>,
    session_id: [u8; 32],
    host_key: Option<SigningKey>,
    crypto: CryptoState,
    input_buffer: Vec<u8>,
    channel_open: bool,
    client_channel: u32,
    config: SshdConfig,
    /// PTY dimensions from the client's `pty-req` (columns / rows). Applied to
    /// the spawned login shell via `TIOCSWINSZ` after `spawn_pty` — without
    /// this, full-screen apps (vi, less) read 80x24 from `TIOCGWINSZ` and
    /// ignore the real terminal size. Defaults to 80x24.
    term_width: u32,
    term_height: u32,
}

impl SshSession {
    fn new(config: SshdConfig) -> Self {
        Self {
            state: SshState::AwaitingVersion,
            rng: super::crypto::new_seeded_rng(),
            client_version: Vec::new(),
            server_version: SSH_VERSION[..SSH_VERSION.len() - 2].to_vec(),
            client_kexinit: Vec::new(),
            server_kexinit: Vec::new(),
            session_id: [0u8; 32],
            host_key: keys::get_host_key(),
            crypto: CryptoState::new(),
            input_buffer: Vec::new(),
            channel_open: false,
            client_channel: 0,
            config,
            term_width: 80,
            term_height: 24,
        }
    }
}

// ============================================================================
// Shell Handling
// ============================================================================

/// One-shot `ssh host <cmd>` (SSH `exec` channel request, as opposed to an
/// interactive `shell` request). Spawns the configured shell with `-c <cmd>`
/// and pumps it through the same `bridge_process` used for interactive
/// sessions, so exit-on-child-exit / stdin-forwarding / stdout-draining
/// behave identically. There is no built-in fallback — a spawn failure ends
/// the session with an error message.
async fn run_exec_session(
    stream: &mut SshStream,
    session: &mut SshSession,
    command: &[u8],
) -> Result<(), NetError> {
    let cmd_str = core::str::from_utf8(command).unwrap_or("");
    let shell_path = session.config.shell.clone();
    let mut arg_refs: Vec<&str> = session.config.shell_args.iter().map(|s| s.as_str()).collect();
    arg_refs.push("-c");
    arg_refs.push(cmd_str);
    println(&format!("[SSH] Exec: {} {:?}", shell_path, arg_refs));
    if let Some(res) = spawn(&shell_path, Some(&arg_refs)) {
        return bridge_process(stream, session, res.pid, res.stdout_fd, false).await;
    }
    fail_spawn(stream, session, &shell_path, "exec").await
}

/// ASCII-art welcome banner, mirroring the in-kernel SSH server's login
/// banner (`src/ssh/protocol.rs`). Kept as a local copy (`akuma_40.txt`)
/// rather than reaching across into the kernel's source tree.
const BANNER_ART: &str = include_str!("akuma_40.txt");

/// Builds the same banner text the in-kernel SSH server prints on login,
/// minus the "Type 'help'..." line (there's no built-in shell here — the
/// spawned shell prints its own prompt).
fn build_banner() -> String {
    let mut welcome = String::from("\r\n");
    for line in BANNER_ART.lines() {
        welcome.push_str(line);
        welcome.push_str("\r\n");
    }
    let boxed = [
        "      Welcome to Akuma SSH Server",
        "   now with sick beats by Tokyo Rider",
        " https://tokyorider.bandcamp.com/album/omegashima",
    ];
    let longest = boxed.iter().map(|l| l.len()).max().unwrap_or(0);
    let divider = "=".repeat(core::cmp::min(longest + 1, 50));
    welcome.push_str("\r\n");
    welcome.push_str(&divider);
    welcome.push_str("\r\n");
    for line in boxed {
        welcome.push_str(line);
        welcome.push_str("\r\n");
    }
    welcome.push_str(&divider);
    welcome.push_str("\r\n\r\n");
    welcome
}

/// Interactive `shell` channel request. There is no built-in fallback shell
/// — a spawn failure ends the session with an error message.
async fn run_shell_session(
    stream: &mut SshStream,
    session: &mut SshSession,
) -> Result<(), NetError> {
    if session.config.banner {
        let banner = build_banner();
        send_channel_data(stream, session, banner.as_bytes()).await?;
    }
    let shell_path = session.config.shell.clone();
    // Extra argv for multicall shells (busybox/toybox): the kernel sets
    // argv[0] = shell_path, then these follow (e.g. ["sh"]).
    let arg_refs: Vec<&str> = session.config.shell_args.iter().map(|s| s.as_str()).collect();
    let args = if arg_refs.is_empty() { None } else { Some(arg_refs.as_slice()) };
    println(&format!("[SSH] Spawning shell: {} {:?}", shell_path, session.config.shell_args));
    // Interactive login shell: request a pty so the kernel runs its canonical
    // line discipline (ICRNL CR->NL, echo, line editing) on the shell's
    // stdin. `ssh -tt` sends a `pty-req` ahead of this `shell` request; the
    // shell is cooked like a real terminal instead of a raw pipe. (A future
    // refinement could gate this on a tracked `pty-req` flag so a
    // no-pty client gets a pipe.)
    if let Some(res) = spawn_pty(&shell_path, args) {
        // Push the client's pty-req dimensions into the child's TerminalState
        // so TIOCGWINSZ (vi, less, `stty size`) sees the real size, not 80x24.
        // Must happen before the first full-screen redraw. The child got a
        // fresh TerminalState from the pty spawn, so its fd (ChildStdout) is
        // the handle the kernel resolves to that state.
        set_terminal_size(res.stdout_fd as i32, session.term_width as u16, session.term_height as u16);
        return bridge_process(stream, session, res.pid, res.stdout_fd, true).await;
    }
    fail_spawn(stream, session, &shell_path, "shell").await
}

/// Report a shell-spawn failure to both the log and the client, then end the
/// session cleanly (no built-in shell to fall back to).
async fn fail_spawn(
    stream: &mut SshStream,
    session: &mut SshSession,
    shell_path: &str,
    kind: &str,
) -> Result<(), NetError> {
    let msg = format!("sshd: failed to spawn '{shell_path}' for {kind}\r\n");
    println(&format!("[SSH] {}", msg.trim_end()));
    send_channel_data(stream, session, msg.as_bytes()).await?;
    // 127 — the shell convention for "command not found". Without an explicit
    // status the client would report 255 (see `send_exit_report`), which reads
    // as a connection failure rather than "your command could not be run".
    // Synthesised as a clean exit: nothing was ever spawned, so there is no
    // signal to report.
    send_exit_report(stream, session, WaitStatus { pid: 0, raw: (127 << 8) }).await
}

/// `\n` → `\r\n` for a client PTY, byte-identical passthrough otherwise. A PTY
/// needs it because the shell's stdout is a pipe, not a terminal, so no line
/// discipline cooks it for us (mirrors the in-kernel sshd's cooked-mode
/// output). An exec channel's stdout is a raw byte stream, not terminal
/// text — translating it corrupts any binary or digest-checked output. See
/// `EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` root cause B.
fn cook_output(data: &[u8], pty: bool) -> Vec<u8> {
    if !pty {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + 8);
    for &byte in data {
        if byte == b'\n' {
            out.push(b'\r');
        }
        out.push(byte);
    }
    out
}

async fn bridge_process(
    stream: &mut SshStream,
    session: &mut SshSession,
    pid: u32,
    stdout_fd: u32,
    pty: bool,
) -> Result<(), NetError> {
    let mut buf = [0u8; 1024];

    // Open the child's stdin ONCE and reuse it for the whole session, instead
    // of open()+write()+close() per keystroke (a full procfs path resolution
    // — parse "/proc/<pid>/fd/0", look up the process, build a fresh
    // FileDescriptor — on every single character forwarded). Interactive
    // typing was paying 2 extra syscalls per keystroke for no reason; the
    // channel this resolves to doesn't change for the life of the session.
    let stdin_fd = open(&format!("/proc/{}/fd/0", pid), open_flags::O_WRONLY);
    if stdin_fd < 0 {
        eprintln(&format!("[SSHD] bridge_process: couldn't open stdin for pid {}", pid));
    }

    // CRITICAL: make BOTH ends non-blocking before the bridge loop. The child's
    // stdout fd (ChildStdout) and the SSH socket both block by default. Without
    // this, the loop parks in `read_fd(stdout_fd)` — busybox is waiting in ppoll
    // on its stdin and emits no output — and never reaches the keystroke
    // forwarding below, so the shell never receives input: a deadlock (bridge
    // waits on stdout, shell waits on stdin). Non-blocking lets the loop poll
    // both directions; `read_fd` returns EAGAIN (<0) and `stream.read` surfaces
    // it as Err, both of which the loop already tolerates.
    set_nonblocking(stdout_fd as i32, true);
    set_nonblocking(stream.as_raw_fd(), true);

    // The session ends when the SHELL exits, not when the client stops sending.
    // A non-interactive client (`echo cmd | ssh`, or anything that closes its
    // stdin) sends CHANNEL_EOF right after the command bytes; tearing down then
    // would drop the shell's output. So after EOF we stop reading input but keep
    // pumping stdout until waitpid(pid) reports the shell has exited.
    let mut client_done = false;

    // Evaluates to how the child ended, which `send_exit_report` below relays to
    // the client. `waitpid_status`, not `waitpid`: the latter returns only
    // WEXITSTATUS, which is 0 for a signal death and so cannot be told apart
    // from a clean success. The only way out of this loop other than the reap is
    // a `?`, and those all mean the client is already gone.
    let status = loop {
        // 1. Shell exited → drain remaining stdout, then stop. Must apply the
        //    same cook_output() gating as the live loop below — this drain
        //    used to send raw bytes unconditionally, so which bytes of a PTY
        //    session's output got \r\n-translated depended on whether the
        //    bridge happened to read them before or after the child exited.
        if let Some(status) = waitpid_status(pid) {
            loop {
                let n = read_fd(stdout_fd as i32, &mut buf);
                if n > 0 {
                    let out = cook_output(&buf[..n as usize], pty);
                    send_channel_data(stream, session, &out).await?;
                } else {
                    break;
                }
            }
            break status;
        }

        let mut did_io = false;

        // 2. Output from process to SSH (non-blocking). See cook_output().
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 {
            let out = cook_output(&buf[..n as usize], pty);
            send_channel_data(stream, session, &out).await?;
            did_io = true;
        }

        // 3. Input from SSH to process (non-blocking; EAGAIN surfaces as Err).
        //    Skip once the client has signalled it is done sending.
        if !client_done {
            let mut ssh_buf = [0u8; 512];
            // Non-suspending: this loop must also keep draining the child's
            // stdout in the same tick, so it can't await a would-block read
            // (see `SshStream::try_read`).
            match stream.try_read(&mut ssh_buf) {
                Ok(0) => client_done = true, // peer closed its write side (TCP)
                Ok(n) => {
                    did_io = true;
                    session.input_buffer.extend_from_slice(&ssh_buf[..n]);
                }
                Err(_) => {} // EAGAIN / WouldBlock — nothing new to read right now
            }

            // ALWAYS drain buffered SSH packets — not just when the read above
            // returned new bytes. The client's CHANNEL_DATA can already sit in
            // `input_buffer` (buffered while the handshake completed, before this
            // bridge took over); if we only processed on a fresh read, that data
            // would never be forwarded and the shell would hang waiting for input.
            while let Some((msg_type, payload)) = process_encrypted_packet(session) {
                did_io = true;
                if msg_type == SSH_MSG_CHANNEL_DATA {
                    let mut offset = 0;
                    let _recipient = read_u32(&payload, &mut offset);
                    if let Some(data) = read_string(&payload, &mut offset) {
                        // Forward to the child's stdin (opened once above).
                        if stdin_fd >= 0 {
                            write_fd(stdin_fd, data);
                        }
                    }
                } else if msg_type == SSH_MSG_CHANNEL_EOF || msg_type == SSH_MSG_CHANNEL_CLOSE {
                    // Client is done sending input (`ssh -tt` with piped stdin
                    // EOFs — and may CLOSE — right after the command bytes).
                    // Deliver EOF to the shell's stdin so a shell reading a piped
                    // script (busybox `sh`) stops waiting for more input and runs
                    // to completion, then keep draining its output until the shell
                    // ITSELF exits. Tearing down here would drop the command
                    // output; if the client has truly gone, send_channel_data
                    // above fails and the `?` unwinds the loop.
                    close_child_stdin(pid);
                    client_done = true;
                } else if msg_type == SSH_MSG_CHANNEL_REQUEST {
                    // A live resize: forward new columns/rows to the child so
                    // full-screen apps (vi, less) reflow. Format after the
                    // recipient channel: string req_type, boolean want_reply,
                    // u32 width, u32 height, u32 pixel_w, u32 pixel_h.
                    let mut offset = 0;
                    let _recipient = read_u32(&payload, &mut offset);
                    if read_string(&payload, &mut offset) == Some(b"window-change") {
                        let _want_reply = payload.get(offset).copied().unwrap_or(0);
                        offset += 1;
                        if let (Some(w), Some(h)) = (read_u32(&payload, &mut offset), read_u32(&payload, &mut offset)) {
                            session.term_width = w;
                            session.term_height = h;
                            set_terminal_size(stdout_fd as i32, w as u16, h as u16);
                        }
                    }
                }
            }
        }

        // Only yield when both directions were idle, to keep latency low while busy.
        // `yield_now` (not `sleep_ms`) so this suspends *this session's* future
        // instead of blocking sshd's whole executor thread — see its doc comment.
        if !did_io {
            crate::yield_now().await;
        }
    };
    // Close our read end of the child's stdout. The kernel keeps the child's
    // ProcessChannel alive past waitpid while output is still buffered (so the
    // drain above doesn't lose it); closing the ChildStdout fd here is what frees
    // that channel now. Without it, sshd (which handles connections serially in
    // one long-lived process) would leak one channel per login.
    close(stdout_fd as i32);
    if stdin_fd >= 0 {
        close(stdin_fd);
    }

    // Only now that the fds are released (so a write error can't leak them via
    // `?`) tell the client how the command ended.
    send_exit_report(stream, session, status).await
}

/// RFC 4254 §6.10: report how the command ended, then tear the channel down
/// (`CHANNEL_EOF` + `CHANNEL_CLOSE`).
///
/// This is what makes `ssh host cmd; echo $?` print the command's real status.
/// OpenSSH's client seeds its own exit status with **255** and only overwrites
/// it when one of these requests arrives; a server that just closes the
/// connection leaves that 255 in place, so *every* remote command — including
/// a perfectly successful one — looked like a connection failure. Reporting is
/// the server's job, not something the client can infer.
///
/// §6.10 defines two mutually exclusive reports, and which one applies is
/// exactly the distinction `WEXITSTATUS` alone cannot make:
///
/// - clean exit → `exit-status` with the code.
/// - killed by a signal → `exit-signal` naming the signal. Its exit code is 0,
///   so sending `exit-status` here would report a crash as a success.
///
/// Never both, per the spec.
async fn send_exit_report(
    stream: &mut SshStream,
    session: &mut SshSession,
    status: WaitStatus,
) -> Result<(), NetError> {
    if !session.channel_open {
        return Ok(());
    }

    let channel = session.client_channel;
    let payload = match status.term_signal() {
        Some(sig) => {
            println(&format!(
                "[SSH] Killed by signal {} ({})",
                sig,
                wire::signal_name(sig)
            ));
            let msg = format!("terminated by signal {sig}");
            wire::exit_signal_payload(channel, sig, &msg)
        }
        None => {
            println(&format!("[SSH] Exit status: {}", status.exit_code()));
            wire::exit_status_payload(channel, status.exit_code())
        }
    };
    send_packet(stream, &payload, session).await?;

    // The client returns once the channel closes, using the status recorded
    // above. EOF first, then CLOSE, so it knows no more data is coming.
    send_packet(stream, &wire::channel_eof_payload(channel), session).await?;
    send_packet(stream, &wire::channel_close_payload(channel), session).await?;

    // Guard against a second send on a channel we've already closed.
    session.channel_open = false;

    // Then wait for the client to close the channel back before returning —
    // because returning drops `SshStream`, which closes the socket, and
    // `write_all` above only *queued* those three packets in the TCP stack. A
    // close that races the flush discards them, and the client falls back to its
    // 255 placeholder: observed as a signal-killed command intermittently
    // reporting no exit request at all (~1 in 10), with sshd's own log showing
    // it had sent one. Reading until the peer hangs up keeps the socket open
    // until the report has actually gone out.
    //
    // Bounded, because a client that holds the connection open would otherwise
    // pin this session forever. `yield_now` (not `sleep_ms`) so the wait costs
    // other sessions nothing — see its doc comment.
    const CLOSE_WAIT_TICKS: u32 = 500;
    let mut scratch = [0u8; 512];
    for _ in 0..CLOSE_WAIT_TICKS {
        match stream.try_read(&mut scratch) {
            // Peer hung up: the report necessarily reached it first (TCP is
            // ordered), so there is nothing left to flush.
            Ok(0) => break,
            Ok(n) => {
                // Its CHANNEL_CLOSE may be in here; decrypt so we can stop as
                // soon as it arrives instead of spinning to the bound.
                session.input_buffer.extend_from_slice(&scratch[..n]);
                let mut saw_close = false;
                while let Some((msg_type, _)) = process_encrypted_packet(session) {
                    if msg_type == SSH_MSG_CHANNEL_CLOSE {
                        saw_close = true;
                    }
                }
                if saw_close {
                    break;
                }
            }
            Err(_) => {} // EAGAIN — nothing yet
        }
        crate::yield_now().await;
    }

    Ok(())
}

// ============================================================================
// Message Handlers
// ============================================================================

enum MessageResult { Continue, StartShell, StartExec(Vec<u8>), Disconnect }

async fn handle_message(
    stream: &mut SshStream,
    msg_type: u8,
    payload: &[u8],
    session: &mut SshSession,
) -> Result<MessageResult, NetError> {
    match msg_type {
        SSH_MSG_KEXINIT => {
            let mut full = vec![SSH_MSG_KEXINIT];
            full.extend_from_slice(payload);
            session.client_kexinit = full;
            let kexinit = build_kexinit(&mut session.rng);
            session.server_kexinit = kexinit.clone();
            send_unencrypted_packet(stream, &kexinit, session).await?;
            session.state = SshState::AwaitingKexEcdhInit;
        }
        SSH_MSG_KEX_ECDH_INIT => {
            let mut offset = 0;
            if let Some(client_pubkey) = read_string(payload, &mut offset) {
                if let Some(reply) = handle_kex_ecdh_init(session, client_pubkey) {
                    send_unencrypted_packet(stream, &reply, session).await?;
                    let newkeys = vec![SSH_MSG_NEWKEYS];
                    send_unencrypted_packet(stream, &newkeys, session).await?;
                    session.state = SshState::AwaitingNewKeys;
                }
            }
        }
        SSH_MSG_NEWKEYS => { session.state = SshState::AwaitingServiceRequest; }
        SSH_MSG_SERVICE_REQUEST => {
            let mut offset = 0;
            if let Some(service) = read_string(payload, &mut offset) {
                let mut reply = vec![SSH_MSG_SERVICE_ACCEPT];
                write_string(&mut reply, service);
                send_packet(stream, &reply, session).await?;
                session.state = SshState::AwaitingUserAuth;
            }
        }
        SSH_MSG_USERAUTH_REQUEST => {
            let (result, reply) = auth::handle_userauth_request(payload, &session.session_id, &session.config).await;
            send_packet(stream, &reply, session).await?;
            if let AuthResult::Success = result { session.state = SshState::Authenticated; }
        }
        SSH_MSG_CHANNEL_OPEN => {
            let mut offset = 0;
            let _type = read_string(payload, &mut offset);
            let sender = read_u32(payload, &mut offset).unwrap_or(0);
            session.client_channel = sender;
            session.channel_open = true;
            let mut reply = vec![SSH_MSG_CHANNEL_OPEN_CONFIRMATION];
            write_u32(&mut reply, sender);
            write_u32(&mut reply, 0);
            write_u32(&mut reply, 0x100000);
            write_u32(&mut reply, 0x4000);
            send_packet(stream, &reply, session).await?;
        }
        SSH_MSG_CHANNEL_REQUEST => {
            let mut offset = 0;
            let _recipient = read_u32(payload, &mut offset);
            let req_type = read_string(payload, &mut offset).unwrap_or(b"");
            let want_reply = if offset < payload.len() { payload[offset] != 0 } else { false };
            if want_reply {
                let mut full_reply = vec![SSH_MSG_CHANNEL_SUCCESS];
                write_u32(&mut full_reply, session.client_channel);
                send_packet(stream, &full_reply, session).await?;
            }
            if req_type == b"shell" { return Ok(MessageResult::StartShell); }
            if req_type == b"exec" {
                // The want_reply byte (already peeked above, not yet consumed)
                // precedes the exec command string.
                let mut cmd_offset = offset + 1;
                if let Some(command) = read_string(payload, &mut cmd_offset) {
                    return Ok(MessageResult::StartExec(command.to_vec()));
                }
            }
            if req_type == b"pty-req" {
                // Format after want_reply: string TERM, u32 width, u32 height,
                // u32 pixel_width, u32 pixel_height, string modes. Stash the
                // dimensions; run_shell_session applies them to the spawned
                // shell via TIOCSWINSZ once it has the child fd.
                let mut off = offset + 1; // skip want_reply
                let _term = read_string(payload, &mut off);
                if let (Some(w), Some(h)) = (read_u32(payload, &mut off), read_u32(payload, &mut off)) {
                    session.term_width = w;
                    session.term_height = h;
                }
            }
        }
        SSH_MSG_DISCONNECT => return Ok(MessageResult::Disconnect),
        _ => {}
    }
    Ok(MessageResult::Continue)
}

// ============================================================================
// Packet Helpers
// ============================================================================

fn build_kexinit(rng: &mut SimpleRng) -> Vec<u8> {
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

fn handle_kex_ecdh_init(session: &mut SshSession, client_pubkey: &[u8]) -> Option<Vec<u8>> {
    let mut secret_bytes = [0u8; 32];
    session.rng.fill_bytes(&mut secret_bytes);
    let server_secret = x25519_dalek::StaticSecret::from(secret_bytes);
    let server_public = X25519PublicKey::from(&server_secret);
    let server_pubkey = server_public.as_bytes();
    let client_pubkey_bytes: [u8; 32] = client_pubkey.try_into().ok()?;
    let client_public = X25519PublicKey::from(client_pubkey_bytes);
    let shared_secret = server_secret.diffie_hellman(&client_public).as_bytes().to_vec();
    let host_key = session.host_key.as_ref()?;
    let mut host_key_blob = Vec::new();
    write_string(&mut host_key_blob, b"ssh-ed25519");
    write_string(&mut host_key_blob, host_key.verifying_key().as_bytes());
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
    if session.session_id == [0u8; 32] { session.session_id = exchange_hash; }
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
    use ctr::cipher::KeyIvInit;
    session.crypto.decrypt_cipher = Some(Aes128Ctr::new(key_c2s[..AES_KEY_SIZE].try_into().unwrap(), iv_c2s[..AES_IV_SIZE].try_into().unwrap()));
    session.crypto.decrypt_mac_key.copy_from_slice(&mac_c2s[..MAC_KEY_SIZE]);
    session.crypto.encrypt_cipher = Some(Aes128Ctr::new(key_s2c[..AES_KEY_SIZE].try_into().unwrap(), iv_s2c[..AES_IV_SIZE].try_into().unwrap()));
    session.crypto.encrypt_mac_key.copy_from_slice(&mac_s2c[..MAC_KEY_SIZE]);
    let mut reply = Vec::new();
    reply.push(SSH_MSG_KEX_ECDH_REPLY);
    write_string(&mut reply, &host_key_blob);
    write_string(&mut reply, server_pubkey);
    write_string(&mut reply, &sig_blob);
    Some(reply)
}

async fn send_packet(stream: &mut SshStream, payload: &[u8], session: &mut SshSession) -> Result<(), NetError> {
    if session.crypto.encrypt_cipher.is_some() && session.state != SshState::AwaitingNewKeys {
        let seq = session.crypto.encrypt_seq;
        session.crypto.encrypt_seq = seq.wrapping_add(1);
        let packet = build_encrypted_packet(
            payload,
            session.crypto.encrypt_cipher.as_mut().unwrap(),
            &session.crypto.encrypt_mac_key,
            seq,
            &mut session.rng,
        );
        stream.write_all(&packet).await
    } else {
        let packet = build_packet(payload);
        session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
        stream.write_all(&packet).await
    }
}

async fn send_unencrypted_packet(stream: &mut SshStream, payload: &[u8], session: &mut SshSession) -> Result<(), NetError> {
    let packet = build_packet(payload);
    session.crypto.encrypt_seq = session.crypto.encrypt_seq.wrapping_add(1);
    stream.write_all(&packet).await
}

async fn send_channel_data(stream: &mut SshStream, session: &mut SshSession, data: &[u8]) -> Result<(), NetError> {
    if !session.channel_open { return Ok(()); }
    let mut payload = vec![SSH_MSG_CHANNEL_DATA];
    write_u32(&mut payload, session.client_channel);
    write_string(&mut payload, data);
    send_packet(stream, &payload, session).await
}

fn process_encrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 4 { return None; }
    let cipher = session.crypto.decrypt_cipher.as_mut()?;
    use ctr::cipher::StreamCipher;
    let mut peek_cipher = cipher.clone();
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&session.input_buffer[..4]);
    peek_cipher.apply_keystream(&mut len_buf);
    let packet_len = u32::from_be_bytes(len_buf) as usize;
    let total_needed = 4 + packet_len + MAC_SIZE;
    if session.input_buffer.len() < total_needed { return None; }
    let encrypted_data = &session.input_buffer[..4 + packet_len];
    let received_mac = &session.input_buffer[4 + packet_len..total_needed];
    let mut decrypted = encrypted_data.to_vec();
    cipher.apply_keystream(&mut decrypted);
    let seq = session.crypto.decrypt_seq;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&session.crypto.decrypt_mac_key).ok()?;
    mac.update(&seq.to_be_bytes());
    mac.update(&decrypted);
    if mac.verify_slice(received_mac).is_err() { return None; }
    session.crypto.decrypt_seq = seq.wrapping_add(1);
    let padding_len = decrypted[4] as usize;
    let payload_len = packet_len - padding_len - 1;
    let msg_type = decrypted[5];
    let payload = decrypted[6..5 + payload_len].to_vec();
    session.input_buffer = session.input_buffer[total_needed..].to_vec();
    Some((msg_type, payload))
}

fn process_unencrypted_packet(session: &mut SshSession) -> Option<(u8, Vec<u8>)> {
    if session.input_buffer.len() < 5 { return None; }
    let packet_len = u32::from_be_bytes(session.input_buffer[..4].try_into().ok()?) as usize;
    let total_len = 4 + packet_len;
    if session.input_buffer.len() < total_len { return None; }
    let padding_len = session.input_buffer[4] as usize;
    let payload_len = packet_len - padding_len - 1;
    let msg_type = session.input_buffer[5];
    let payload = session.input_buffer[6..5 + payload_len].to_vec();
    session.crypto.decrypt_seq = session.crypto.decrypt_seq.wrapping_add(1);
    session.input_buffer = session.input_buffer[total_len..].to_vec();
    Some((msg_type, payload))
}

pub async fn handle_connection(mut stream: SshStream, config: SshdConfig) {
    let mut session = SshSession::new(config);
    let _ = stream.write_all(SSH_VERSION).await;
    
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                session.input_buffer.extend_from_slice(&buf[..n]);
                if session.state == SshState::AwaitingVersion {
                    if let Some(pos) = session.input_buffer.iter().position(|&b| b == b'\n') {
                        let line = session.input_buffer[..pos].to_vec();
                        session.input_buffer = session.input_buffer[pos+1..].to_vec();
                        session.client_version = if line.ends_with(b"\r") { line[..line.len()-1].to_vec() } else { line };
                        session.state = SshState::AwaitingKexInit;
                    }
                    continue;
                }
                
                while let Some((msg_type, payload)) = if !matches!(session.state, SshState::AwaitingNewKeys | SshState::AwaitingKexInit | SshState::AwaitingKexEcdhInit) {
                    process_encrypted_packet(&mut session)
                } else {
                    process_unencrypted_packet(&mut session)
                } {
                    match handle_message(&mut stream, msg_type, &payload, &mut session).await {
                        Ok(MessageResult::Continue) => {}
                        Ok(MessageResult::StartShell) => {
                            let _ = run_shell_session(&mut stream, &mut session).await;
                            return;
                        }
                        Ok(MessageResult::StartExec(cmd)) => {
                            let _ = run_exec_session(&mut stream, &mut session, &cmd).await;
                            return;
                        }
                        Ok(MessageResult::Disconnect) => return,
                        Err(_) => return,
                    }
                }
            }
            Err(_) => break,
        }
    }
}
