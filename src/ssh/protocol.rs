//! SSH-2 Protocol — kernel integration layer.
//!
//! Protocol logic (state machine, kex, packet processing, message handling)
//! lives in the `akuma_ssh` crate. This module provides the kernel-coupled
//! pieces: connection orchestration with timeouts, `SshChannelStream` (the
//! I/O bridge between SSH channels and the kernel's shell/editor), the
//! interactive shell session, and process bridging.

use alloc::format;
use alloc::vec;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
use spinning_top::Spinlock;

use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey};
use embedded_io_async::{ErrorType, Read, Write};

use akuma_ssh::session::{SshSession, SshState};
use akuma_ssh::constants::*;
use akuma_ssh::message::MessageResult;
use akuma_ssh::util::{RESIZE_SIGNAL_BYTE, translate_input_keys};

use super::auth::KernelAuthProvider;
use super::crypto::{
    SimpleRng, read_string, read_u32, trim_bytes, write_u32,
};
use super::keys;
use akuma_net::smoltcp_net::{TcpError, TcpStream};
use crate::shell::ShellContext;
use crate::shell::{self, commands::create_default_registry};
use akuma_exec::process::{self, Pid};
use akuma_terminal as terminal;
use crate::kernel_timer::Duration;

// ============================================================================
// SSH Timeouts (kernel-specific, not in crate)
// ============================================================================

const SSH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const SSH_READ_TIMEOUT: Duration = Duration::from_secs(60);
const SSH_INTERACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(10);

// ============================================================================
// Shared Host Key (for all sessions)
// ============================================================================

const HOST_KEY_PATH: &str = "/etc/sshd/host_key";

pub fn init_host_key() {
    if keys::get_host_key().is_some() {
        return;
    }

    // Try to load a persisted host key from disk so the host fingerprint
    // is stable across reboots — otherwise every boot triggers
    // `Host key verification failed` on the client side.
    match crate::fs::read_file(HOST_KEY_PATH) {
        Ok(bytes) if bytes.len() == SECRET_KEY_LENGTH => {
            let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
            key_bytes.copy_from_slice(&bytes);
            keys::set_host_key(SigningKey::from_bytes(&key_bytes));
            log("[SSH] Host key loaded from /etc/sshd/host_key\n");
            return;
        }
        Ok(bytes) => {
            safe_print!(
                128,
                "[SSH] WARNING: {} has wrong length ({} != {}), regenerating\n",
                HOST_KEY_PATH,
                bytes.len(),
                SECRET_KEY_LENGTH
            );
        }
        Err(_) => {
            // File missing or unreadable — fall through to generation.
        }
    }

    let mut rng = SimpleRng::new();
    let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
    rng.fill_bytes(&mut key_bytes);
    let key = SigningKey::from_bytes(&key_bytes);
    keys::set_host_key(key);

    // Best-effort persistence: log failures but don't abort, the kernel
    // still works with an ephemeral key (just nags the client).
    match crate::fs::write_file(HOST_KEY_PATH, &key_bytes) {
        Ok(()) => log("[SSH] Generated and persisted new host key to /etc/sshd/host_key\n"),
        Err(e) => safe_print!(
            128,
            "[SSH] WARNING: generated host key but failed to persist to {}: {}\n",
            HOST_KEY_PATH,
            e
        ),
    }
}

// ============================================================================
// SSH Channel Stream (embedded_io_async adapter — kernel-coupled)
// ============================================================================

#[derive(Debug)]
pub struct SshStreamError;

impl embedded_io_async::Error for SshStreamError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

pub struct SshChannelStream<'a> {
    stream: &'a mut TcpStream,
    session: &'a mut SshSession,
    pub current_process_pid: Option<Pid>,
    pub current_process_channel: Option<Arc<akuma_exec::process::ProcessChannel>>,
    /// Count of non-data SSH packets (window-adjust, ignore, debug, etc.)
    /// handled since the last data packet was delivered to the caller.
    /// Reset whenever `read_until_channel_data` returns with data.
    /// Used by the `[SSH-ECHO]` instrumentation to attribute jitter.
    non_data_packets_since_data: u32,
}

impl<'a> SshChannelStream<'a> {
    fn new(stream: &'a mut TcpStream, session: &'a mut SshSession) -> Self {
        Self {
            stream,
            session,
            current_process_pid: None,
            current_process_channel: None,
            non_data_packets_since_data: 0,
        }
    }

    /// Whether the SSH client has signaled CHANNEL_EOF or CHANNEL_CLOSE.
    /// Used by `execute_external_interactive` to break out of its
    /// process-pump loop when the peer is gone (issue #5 in
    /// `docs/STABILITY_URGENT_ISSUES.md`).
    pub fn channel_eof(&self) -> bool {
        self.session.channel_eof
    }

    async fn read_until_channel_data(&mut self) -> Result<(), TcpError> {
        let mut buf = [0u8; 512];

        loop {
            if !self.session.channel_data_buffer.is_empty() || self.session.channel_eof {
                return Ok(());
            }

            let read_result = crate::kernel_timer::with_timeout(
                SSH_READ_TIMEOUT,
                self.stream.read(&mut buf)
            ).await;

            match read_result {
                Err(_timeout) => {
                    self.session.channel_eof = true;
                    return Ok(());
                }
                Ok(Ok(0)) => {
                    self.session.channel_eof = true;
                    return Ok(());
                }
                Ok(Err(e)) => return Err(e),
                Ok(Ok(n)) => {
                    self.session.feed_input(&buf[..n]);

                    loop {
                        let packet = akuma_ssh::packet::process_encrypted_packet(self.session);
                        match packet {
                            Some((msg_type, payload)) => {
                                match self.handle_channel_message(msg_type, &payload).await {
                                    Ok(true) => {
                                        // Data delivered; reset attribution counter.
                                        self.non_data_packets_since_data = 0;
                                        return Ok(());
                                    }
                                    Ok(false) => {
                                        self.non_data_packets_since_data =
                                            self.non_data_packets_since_data.saturating_add(1);
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }

    async fn try_read_interactive(&mut self, buf: &mut [u8]) -> Result<usize, TcpError> {
        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        if self.session.channel_eof {
            return Ok(0);
        }

        let mut tcp_buf = [0u8; 512];
        let read_result = crate::kernel_timer::with_timeout(
            SSH_INTERACTIVE_READ_TIMEOUT,
            self.stream.read(&mut tcp_buf)
        ).await;

        match read_result {
            Err(_timeout) => Ok(0),
            Ok(Ok(0)) => {
                self.session.channel_eof = true;
                Ok(0)
            }
            Ok(Err(e)) => Err(e),
            Ok(Ok(n)) => {
                self.session.feed_input(&tcp_buf[..n]);

                loop {
                    let packet = akuma_ssh::packet::process_encrypted_packet(self.session);
                    match packet {
                        Some((msg_type, payload)) => {
                            let _ = self.handle_channel_message(msg_type, &payload).await;
                        }
                        None => break,
                    }
                }

                if !self.session.channel_data_buffer.is_empty() {
                    let len = buf.len().min(self.session.channel_data_buffer.len());
                    buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
                    self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
                    return Ok(len);
                }

                Ok(0)
            }
        }
    }

    async fn handle_channel_message(
        &mut self,
        msg_type: u8,
        payload: &[u8],
    ) -> Result<bool, TcpError> {
        match msg_type {
            SSH_MSG_CHANNEL_DATA => {
                let mut offset = 0;
                let _recipient = read_u32(payload, &mut offset);
                if let Some(data) = read_string(payload, &mut offset) {
                    self.session.feed_channel_data(data);
                    return Ok(true);
                }
            }
            SSH_MSG_CHANNEL_REQUEST => {
                let mut offset = 0;
                let _recipient = read_u32(payload, &mut offset);
                if let Some(req_type) = read_string(payload, &mut offset) {
                    if req_type == b"window-change" {
                        let _want_reply = if offset < payload.len() {
                            payload[offset] != 0
                        } else {
                            false
                        };
                        offset += 1;
                        if let Some(width) = read_u32(payload, &mut offset) {
                            if let Some(height) = read_u32(payload, &mut offset) {
                                self.session.term_width = width;
                                self.session.term_height = height;
                                self.session.resize_pending = true;
                                log(&format!("[SSH] Terminal resized: {}x{}\n", width, height));
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            SSH_MSG_CHANNEL_EOF | SSH_MSG_CHANNEL_CLOSE => {
                log("[SSH] Channel close/EOF received\n");
                self.session.channel_eof = true;
                return Ok(true);
            }
            SSH_MSG_GLOBAL_REQUEST => {
                let mut offset = 0;
                let _req_name = read_string(payload, &mut offset);
                let want_reply = if offset < payload.len() { payload[offset] != 0 } else { false };
                if want_reply {
                    let reply = alloc::vec![SSH_MSG_REQUEST_FAILURE];
                    let _ = akuma_ssh::transport::send_packet(self.stream, &reply, self.session).await;
                }
            }
            SSH_MSG_CHANNEL_WINDOW_ADJUST => {}
            SSH_MSG_IGNORE | SSH_MSG_DEBUG => {}
            SSH_MSG_DISCONNECT => {
                log("[SSH] Client disconnected\n");
                self.session.state = SshState::Disconnected;
                self.session.channel_eof = true;
                return Ok(true);
            }
            _ => {
                log(&format!(
                    "[SSH] Ignoring message type {} during shell\n",
                    msg_type
                ));
            }
        }
        Ok(false)
    }
}

impl ErrorType for SshChannelStream<'_> {
    type Error = SshStreamError;
}

impl crate::shell::InteractiveRead for SshChannelStream<'_> {
    async fn try_read_interactive(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.try_read_interactive(buf).await.map_err(|_| SshStreamError)
    }
}

impl crate::editor::TermSizeProvider for SshChannelStream<'_> {
    fn get_term_size(&self) -> crate::editor::TermSize {
        crate::editor::TermSize::new(self.session.term_width, self.session.term_height)
    }
}

impl Read for SshChannelStream<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.session.resize_pending {
            self.session.resize_pending = false;
            if !buf.is_empty() {
                buf[0] = RESIZE_SIGNAL_BYTE;
                return Ok(1);
            }
        }

        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        if self.session.channel_eof {
            return Ok(0);
        }

        self.read_until_channel_data()
            .await
            .map_err(|_| SshStreamError)?;

        if self.session.resize_pending {
            self.session.resize_pending = false;
            if !buf.is_empty() {
                buf[0] = RESIZE_SIGNAL_BYTE;
                return Ok(1);
            }
        }

        if !self.session.channel_data_buffer.is_empty() {
            let len = buf.len().min(self.session.channel_data_buffer.len());
            buf[..len].copy_from_slice(&self.session.channel_data_buffer[..len]);
            self.session.channel_data_buffer = self.session.channel_data_buffer[len..].to_vec();
            return Ok(len);
        }

        Ok(0)
    }
}

const SSH_CHANNEL_MAX_CHUNK: usize = 4096;

impl Write for SshChannelStream<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if !self.session.channel_open {
            return Err(SshStreamError);
        }

        let _tx_drops_before = akuma_net::smoltcp_net::tx_drop_count();

        let mut sent = 0;
        while sent < buf.len() {
            let chunk_size = (buf.len() - sent).min(SSH_CHANNEL_MAX_CHUNK);
            let chunk = &buf[sent..sent + chunk_size];
            akuma_ssh::transport::send_channel_data(self.stream, self.session, chunk)
                .await
                .map_err(|_| SshStreamError)?;
            sent += chunk_size;
        }

        if buf.len() > 128 {
            let _ = crate::kernel_timer::with_timeout(
                SSH_INTERACTIVE_READ_TIMEOUT,
                self.flush()
            ).await;
        } else {
            akuma_net::smoltcp_net::poll();
        }

        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.stream.flush().await.map_err(|_| SshStreamError)?;
        Ok(())
    }
}

// ============================================================================
// Shell Handling (kernel-coupled)
// ============================================================================

async fn bridge_process(
    stream: &mut TcpStream,
    session: &mut SshSession,
    pid: u32,
    process_channel: Arc<akuma_exec::process::ProcessChannel>,
    terminal_state: Arc<Spinlock<terminal::TerminalState>>,
) -> Result<(), TcpError> {
    log(&format!("[SSH] Starting I/O bridge for PID {}\n", pid));
    let mut buf = [0u8; 1024];

    loop {
        if let Some((_, _exit_code)) = akuma_exec::process::waitpid(pid) {
            log(&format!("[SSH] Process PID {} exited, ending bridge\n", pid));
            break;
        }

        loop {
            let n = process_channel.read(&mut buf);
            if n == 0 { break; }
            akuma_ssh::transport::send_channel_data(stream, session, &buf[..n]).await?;
        }

        let mut ssh_buf = [0u8; 512];
        let read_res = crate::kernel_timer::with_timeout(
            crate::kernel_timer::Duration::from_millis(10),
            stream.read(&mut ssh_buf)
        ).await;

        match read_res {
            Ok(Ok(n)) if n > 0 => {
                session.feed_input(&ssh_buf[..n]);
                while let Some((msg_type, payload)) = akuma_ssh::packet::process_encrypted_packet(session) {
                    if msg_type == SSH_MSG_CHANNEL_DATA {
                        let mut offset = 0;
                        let _recipient = read_u32(&payload, &mut offset);
                        if let Some(data) = read_string(&payload, &mut offset) {
                            let translated = translate_input_keys(data);
                            let _ = akuma_exec::process::write_to_process_stdin(pid, &translated);
                        }
                    } else if msg_type == SSH_MSG_CHANNEL_REQUEST {
                        let mut offset = 0;
                        let _recipient = read_u32(&payload, &mut offset);
                        if let Some(req_type) = read_string(&payload, &mut offset) {
                            if req_type == b"window-change" {
                                offset += 1;
                                if let Some(width) = read_u32(&payload, &mut offset) {
                                    if let Some(height) = read_u32(&payload, &mut offset) {
                                        session.term_width = width;
                                        session.term_height = height;
                                        let mut ts = terminal_state.lock();
                                        ts.term_width = width as u16;
                                        ts.term_height = height as u16;
                                        log(&format!("[SSH] Bridge: terminal resized to {}x{}\n", width, height));
                                    }
                                }
                            }
                        }
                    } else if msg_type == SSH_MSG_CHANNEL_EOF || msg_type == SSH_MSG_CHANNEL_CLOSE {
                        log("[SSH] Channel closed, ending bridge\n");
                        return Ok(());
                    }
                }
            }
            _ => {}
        }

        akuma_exec::threading::yield_now();
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum EscapeState {
    Normal,
    Escape,
    Bracket,
    BracketNum(u8),
}

fn generate_prompt(ctx: &ShellContext) -> alloc::string::String {
    format!("akuma:{}> ", ctx.cwd())
}

async fn run_shell_session(
    stream: &mut TcpStream,
    session: &mut SshSession,
) -> Result<(), TcpError> {
    log("[SSH] Starting shell session\n");

    let shell_path_opt = session.config.shell.clone();
    let initial_width = session.term_width;
    let initial_height = session.term_height;

    let mut ctx = crate::shell::new_shell_context();
    let mut channel_stream = SshChannelStream::new(stream, session);

    let terminal_state = Arc::new(Spinlock::new(terminal::TerminalState::default()));
    {
        let mut ts = terminal_state.lock();
        ts.term_width = initial_width as u16;
        ts.term_height = initial_height as u16;
    }
    log(&format!("[SSH] Created shared terminal state at {:p}\n", Arc::as_ptr(&terminal_state)));

    let tid = akuma_exec::threading::current_thread_id();
    let channel = Arc::new(akuma_exec::process::ProcessChannel::new());
    akuma_exec::process::register_system_thread_channel(tid, channel.clone());
    akuma_exec::process::register_terminal_state(tid, terminal_state.clone());

    if let Some(shell_path) = shell_path_opt {
        log(&format!("[SSH] Spawning external shell: {}\n", shell_path));
        if let Ok((_tid, proc_channel, pid)) = akuma_exec::process::spawn_process_with_channel(&shell_path, None, None) {
            return bridge_process(stream, session, pid, proc_channel, terminal_state.clone()).await;
        }
        log(&format!("[SSH] Failed to spawn external shell {}, falling back to built-in\n", shell_path));
    }

    let registry = create_default_registry();

    {
        const BANNER_ART: &str = include_str!("../akuma_40.txt");
        let mut welcome = String::from("\r\n");
        for line in BANNER_ART.lines() {
            welcome.push_str(line);
            welcome.push_str("\r\n");
        }
        welcome.push_str("\r\n========================================\r\n      Welcome to Akuma SSH Server\r\n========================================\r\n\r\nType 'help' for available commands.\r\n\r\n");
        let _ = channel_stream.write(welcome.as_bytes()).await;
        let prompt = generate_prompt(&ctx);
        let _ = channel_stream.write(prompt.as_bytes()).await;
    }

    let mut line_buffer: Vec<u8> = Vec::new();
    let mut cursor_pos: usize = 0;
    let mut read_buf = [0u8; 64];
    let mut escape_state = EscapeState::Normal;

    let mut history: Vec<Vec<u8>> = Vec::new();
    let mut history_index: usize = 0;
    let mut saved_line: Vec<u8> = Vec::new();

    let mut last_read_time_us: u64 = 0;

    loop {
        match channel_stream.read(&mut read_buf).await {
            Ok(0) => {
                log("[SSH] Shell session ended (EOF)\n");
                break;
            }
            Ok(n) => {
                let read_time = crate::timer::uptime_us();
                if last_read_time_us > 0 {
                    let gap = read_time - last_read_time_us;
                    if gap < 2_000_000 {
                        safe_print!(256,
                            "[SSH-ECHO] read gap={}us, {} bytes, non_data_pkts={}\n",
                            gap, n, channel_stream.non_data_packets_since_data
                        );
                    }
                }
                last_read_time_us = read_time;

                let is_raw_mode = if let Some(channel) = &channel_stream.current_process_channel {
                    (*channel).is_raw_mode()
                } else {
                    false
                };

                if is_raw_mode {
                    if let Some(pid) = channel_stream.current_process_pid {
                        let translated = translate_input_keys(&read_buf[..n]);
                        let _ = process::write_to_process_stdin(pid, &translated);
                    }
                } else {
                    for &byte in &read_buf[..n] {
                        match escape_state {
                            EscapeState::Normal => {
                                match byte {
                                    0x1B => {
                                        escape_state = EscapeState::Escape;
                                    }
                                    b'\r' | b'\n' => {
                                        let _ = channel_stream.write(b"\r\n").await;

                                        let trimmed = trim_bytes(&line_buffer);
                                        if !trimmed.is_empty() {
                                            history.push(line_buffer.clone());
                                            if history.len() > 50 {
                                                history.remove(0);
                                            }
                                            history_index = history.len();

                                            if trimmed == b"neko" || trimmed.starts_with(b"neko ") {
                                                let filepath = if trimmed.len() > 5 {
                                                    let path_bytes = trim_bytes(&trimmed[5..]);
                                                    if path_bytes.is_empty() {
                                                        None
                                                    } else {
                                                        Some(
                                                            core::str::from_utf8(path_bytes)
                                                                .unwrap_or(""),
                                                        )
                                                    }
                                                } else {
                                                    None
                                                };

                                                if let Err(e) =
                                                    crate::editor::run(&mut channel_stream, filepath)
                                                        .await
                                                {
                                                    let msg = format!("Editor error: {}\r\n", e);
                                                    let _ = channel_stream.write(msg.as_bytes()).await;
                                                }

                                                line_buffer.clear();
                                                cursor_pos = 0;
                                                let prompt = generate_prompt(&ctx);
                                                let _ = channel_stream.write(prompt.as_bytes()).await;
                                                continue;
                                            }

                                            let result = if let Some(streaming_result) =
                                                shell::execute_command_streaming_interactive(
                                                    trimmed, &registry, &mut ctx, &mut channel_stream, None,
                                                ).await
                                            {
                                                streaming_result
                                            } else {
                                                shell::execute_command_chain(
                                                    trimmed, &registry, &mut ctx, &shell::KernelShellBackend,
                                                ).await
                                            };

                                            if !result.output.is_empty() {
                                                let _ = channel_stream.write(&result.output).await;
                                            }

                                            if result.should_exit {
                                                let _ = channel_stream.write(b"Goodbye!\r\n").await;
                                                return Ok(());
                                            }
                                        }

                                        line_buffer.clear();
                                        cursor_pos = 0;
                                        let prompt = generate_prompt(&ctx);
                                        let _ = channel_stream.write(prompt.as_bytes()).await;
                                    }
                                    0x7F | 0x08 => {
                                        if cursor_pos > 0 {
                                            cursor_pos -= 1;
                                            line_buffer.remove(cursor_pos);

                                            let _ = channel_stream.write(b"\x08").await;
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            let _ = channel_stream.write(b" ").await;
                                            let moves = line_buffer.len() - cursor_pos + 1;
                                            for _ in 0..moves {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                        }
                                    }
                                    0x03 => {
                                        line_buffer.clear();
                                        cursor_pos = 0;
                                        let _ = channel_stream.write(b"^C\r\n").await;
                                        let prompt = generate_prompt(&ctx);
                                        let _ = channel_stream.write(prompt.as_bytes()).await;
                                    }
                                    0x04 => {
                                        if line_buffer.is_empty() {
                                            let _ = channel_stream.write(b"\r\nGoodbye!\r\n").await;
                                            return Ok(());
                                        }
                                    }
                                    0x01 => {
                                        while cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    0x05 => {
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            cursor_pos = line_buffer.len();
                                        }
                                    }
                                    0x0B => {
                                        if cursor_pos < line_buffer.len() {
                                            let chars_to_clear = line_buffer.len() - cursor_pos;
                                            line_buffer.truncate(cursor_pos);
                                            for _ in 0..chars_to_clear {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..chars_to_clear {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                        }
                                    }
                                    0x15 => {
                                        if cursor_pos > 0 {
                                            for _ in 0..cursor_pos {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                            let rest: Vec<u8> = line_buffer[cursor_pos..].to_vec();
                                            let _ = channel_stream.write(&rest).await;
                                            for _ in 0..cursor_pos {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..(cursor_pos + rest.len()) {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }
                                            line_buffer = rest;
                                            cursor_pos = 0;
                                        }
                                    }
                                    _ if byte >= 0x20 && byte < 0x7F => {
                                        line_buffer.insert(cursor_pos, byte);
                                        cursor_pos += 1;

                                        let echo_start = crate::timer::uptime_us();
                                        let _ =
                                            channel_stream.write(&line_buffer[cursor_pos - 1..]).await;
                                        let echo_us = crate::timer::uptime_us() - echo_start;
                                        if echo_us > 5_000 {
                                            safe_print!(256,
                                                "[SSH-ECHO-SLOW] echo took {}us for '{}'\n",
                                                echo_us, byte as char
                                            );
                                        }
                                        let moves = line_buffer.len() - cursor_pos;
                                        for _ in 0..moves {
                                            let _ = channel_stream.write(b"\x08").await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            EscapeState::Escape => {
                                if byte == b'[' {
                                    escape_state = EscapeState::Bracket;
                                } else {
                                    escape_state = EscapeState::Normal;
                                }
                            }
                            EscapeState::Bracket => {
                                escape_state = EscapeState::Normal;
                                match byte {
                                    b'A' => {
                                        if !history.is_empty() && history_index > 0 {
                                            if history_index == history.len() {
                                                saved_line = line_buffer.clone();
                                            }
                                            history_index -= 1;

                                            while cursor_pos > 0 {
                                                let _ = channel_stream.write(b"\x08 \x08").await;
                                                cursor_pos -= 1;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }

                                            line_buffer = history[history_index].clone();
                                            cursor_pos = line_buffer.len();
                                            let _ = channel_stream.write(&line_buffer).await;
                                        }
                                    }
                                    b'B' => {
                                        if history_index < history.len() {
                                            history_index += 1;

                                            while cursor_pos > 0 {
                                                let _ = channel_stream.write(b"\x08 \x08").await;
                                                cursor_pos -= 1;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b" ").await;
                                            }
                                            for _ in 0..line_buffer.len() {
                                                let _ = channel_stream.write(b"\x08").await;
                                            }

                                            if history_index < history.len() {
                                                line_buffer = history[history_index].clone();
                                            } else {
                                                line_buffer = saved_line.clone();
                                            }
                                            cursor_pos = line_buffer.len();
                                            let _ = channel_stream.write(&line_buffer).await;
                                        }
                                    }
                                    b'C' => {
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&[line_buffer[cursor_pos]]).await;
                                            cursor_pos += 1;
                                        }
                                    }
                                    b'D' => {
                                        if cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    b'H' => {
                                        while cursor_pos > 0 {
                                            let _ = channel_stream.write(b"\x08").await;
                                            cursor_pos -= 1;
                                        }
                                    }
                                    b'F' => {
                                        if cursor_pos < line_buffer.len() {
                                            let _ =
                                                channel_stream.write(&line_buffer[cursor_pos..]).await;
                                            cursor_pos = line_buffer.len();
                                        }
                                    }
                                    b'1'..=b'8' => {
                                        escape_state = EscapeState::BracketNum(byte - b'0');
                                    }
                                    _ => {}
                                }
                            }
                            EscapeState::BracketNum(num) => {
                                escape_state = EscapeState::Normal;
                                if byte == b'~' {
                                    match num {
                                        3 => {
                                            if cursor_pos < line_buffer.len() {
                                                line_buffer.remove(cursor_pos);
                                                let rest: Vec<u8> = line_buffer[cursor_pos..].to_vec();
                                                let _ = channel_stream.write(&rest).await;
                                                let _ = channel_stream.write(b" ").await;
                                                let moves = rest.len() + 1;
                                                for _ in 0..moves {
                                                    let _ = channel_stream.write(b"\x08").await;
                                                }
                                            }
                                        }
                                        1 => {
                                            while cursor_pos > 0 {
                                                let _ = channel_stream.write(b"\x08").await;
                                                cursor_pos -= 1;
                                            }
                                        }
                                        4 => {
                                            if cursor_pos < line_buffer.len() {
                                                let _ = channel_stream.write(&line_buffer[cursor_pos..]).await;
                                                cursor_pos = line_buffer.len();
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => {
                log("[SSH] Shell session ended (read error)\n");
                break;
            }
        }
    }

    Ok(())
}

/// Handle an SSH exec request by executing a command and sending output.
async fn handle_exec(
    stream: &mut TcpStream,
    session: &mut SshSession,
    cmd_bytes: &[u8],
) {
    crate::console::print("[SSH-EXEC] Got exec request!\n");
    let mut exec_ctx = crate::shell::new_shell_context();
    // Large enough that the trailing `\n` is not lost to StackWriter truncation
    // (safe_print!(64, …) merged this line with stdout / [exit_group] on serial).
    crate::safe_print!(512,
        "[SSH-EXEC] Command: {:?}\n",
        core::str::from_utf8(cmd_bytes)
    );

    let registry = create_default_registry();
    let trimmed = trim_bytes(cmd_bytes);

    {
        let mut channel_stream = SshChannelStream::new(stream, session);

        if let Some(_streaming_result) =
            shell::execute_command_streaming_interactive(
                trimmed, &registry, &mut exec_ctx, &mut channel_stream, None,
            ).await
        {
            // Output was already streamed
        } else {
            let _ = channel_stream.write(b"[DEBUG] Using buffered path\r\n").await;
            let result =
                shell::execute_command_chain(trimmed, &registry, &mut exec_ctx, &shell::KernelShellBackend).await;

            if !result.output.is_empty() {
                let _ = channel_stream.write(&result.output).await;
            }
        }
    }

    let mut eof = vec![SSH_MSG_CHANNEL_EOF];
    write_u32(&mut eof, session.client_channel);
    let _ = akuma_ssh::transport::send_packet(stream, &eof, session).await;
}

// ============================================================================
// Async Connection Handler (per-connection)
// ============================================================================

pub async fn handle_connection(mut stream: TcpStream) {
    // Reject new connections under memory pressure
    if crate::allocator::is_memory_low() {
        log("[SSH] Rejecting connection: kernel memory low\n");
        super::server::note_handshake_fail();
        return;
    }

    let final_state = handle_connection_inner(&mut stream).await;
    classify_session_exit(final_state);
    log("[SSH] Connection ended\n");
}

pub(crate) fn classify_session_exit(state: akuma_ssh::session::SshState) {
    use akuma_ssh::session::SshState::*;
    match state {
        AwaitingVersion | AwaitingKexInit | AwaitingKexEcdhInit
        | AwaitingNewKeys | AwaitingServiceRequest => {
            super::server::note_handshake_fail();
        }
        AwaitingUserAuth => {
            super::server::note_auth_fail();
        }
        Authenticated | Disconnected => {}
    }
}

async fn handle_connection_inner(stream: &mut TcpStream) -> akuma_ssh::session::SshState {
    log("[SSH] New SSH connection\n");

    let config = super::config::get_config();
    let host_key = keys::get_host_key();
    let rng = super::crypto::create_seeded_rng();
    let mut session = SshSession::new(config, host_key, rng);
    let auth = KernelAuthProvider;

    if akuma_ssh::transport::send_raw(stream, SSH_VERSION).await.is_err() {
        log("[SSH] Failed to send version\n");
        return session.state;
    }

    let mut buf = [0u8; 512];
    loop {
        let timeout = if session.state == SshState::Authenticated {
            SSH_IDLE_TIMEOUT
        } else {
            SSH_HANDSHAKE_TIMEOUT
        };

        let read_result = crate::kernel_timer::with_timeout(timeout, stream.read(&mut buf)).await;

        match read_result {
            Err(_timeout) => {
                log("[SSH] Connection timed out\n");
                break;
            }
            Ok(Ok(0)) => {
                log("[SSH] Connection closed by peer\n");
                break;
            }
            Ok(Err(_e)) => {
                log("[SSH] Read error\n");
                break;
            }
            Ok(Ok(n)) => {
                session.feed_input(&buf[..n]);

                if session.state == SshState::AwaitingVersion {
                    if let Some(pos) = session.input_buffer.iter().position(|&b| b == b'\n') {
                        let version_line = session.input_buffer[..pos].to_vec();
                        session.input_buffer = session.input_buffer[pos + 1..].to_vec();

                        let version = if version_line.ends_with(b"\r") {
                            version_line[..version_line.len() - 1].to_vec()
                        } else {
                            version_line
                        };

                        session.client_version = version;
                        session.state = SshState::AwaitingKexInit;
                        log("[SSH] Client version received\n");
                    }
                    continue;
                }

                loop {
                    let use_encryption = !matches!(
                        session.state,
                        SshState::AwaitingNewKeys
                            | SshState::AwaitingKexInit
                            | SshState::AwaitingKexEcdhInit
                    );

                    let packet = if use_encryption {
                        akuma_ssh::packet::process_encrypted_packet(&mut session)
                    } else {
                        akuma_ssh::packet::process_unencrypted_packet(&mut session)
                    };

                    match packet {
                        Some((msg_type, payload)) => {
                            match akuma_ssh::message::handle_message(
                                stream, msg_type, &payload, &mut session, &auth,
                            ).await {
                                Ok(MessageResult::Continue) => {}
                                Ok(MessageResult::StartShell) => {
                                    if run_shell_session(stream, &mut session).await.is_err() {
                                        log("[SSH] Shell session error\n");
                                    }
                                    if session.channel_open {
                                        let mut close = vec![SSH_MSG_CHANNEL_CLOSE];
                                        write_u32(&mut close, session.client_channel);
                                        let _ =
                                            akuma_ssh::transport::send_packet(stream, &close, &mut session).await;
                                        session.channel_open = false;
                                    }
                                    session.state = SshState::Disconnected;
                                    return session.state;
                                }
                                Ok(MessageResult::ExecCommand(cmd)) => {
                                    handle_exec(stream, &mut session, &cmd).await;
                                    // Mirror the StartShell exit path: send
                                    // CHANNEL_CLOSE and disconnect. Without this
                                    // the session sits in the outer loop waiting
                                    // for SSH_IDLE_TIMEOUT (300s) — even after
                                    // the client closes — keeping a session
                                    // slot reserved. Issue #5 in
                                    // docs/STABILITY_URGENT_ISSUES.md, surfaced
                                    // 2026-05-29 by 4-way concurrent exec test.
                                    if session.channel_open {
                                        let mut close = vec![SSH_MSG_CHANNEL_CLOSE];
                                        write_u32(&mut close, session.client_channel);
                                        let _ =
                                            akuma_ssh::transport::send_packet(stream, &close, &mut session).await;
                                        session.channel_open = false;
                                    }
                                    session.state = SshState::Disconnected;
                                    return session.state;
                                }
                                Ok(MessageResult::Disconnect) => {
                                    return session.state;
                                }
                                Err(_) => {
                                    log("[SSH] Error handling message\n");
                                    return session.state;
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    session.state
}

fn log(msg: &str) {
    safe_print!(512, "{}", msg);
}
