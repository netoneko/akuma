//! Minimal SSH server implementation
//!
//! This module implements a minimal SSH-2 protocol server that:
//! - Accepts any authentication credentials
//! - Provides a shell with basic commands (echo, cat, quit, stats)
//! - Uses simplified protocol handling focused on the state machine

use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::akuma::AKUMA_79;
use crate::console;
use crate::network::{self, SshEvent};

// ============================================================================
// Constants
// ============================================================================

/// SSH protocol version string
const SSH_VERSION: &[u8] = b"SSH-2.0-Akuma_0.1\r\n";

/// Shell prompt
const PROMPT: &[u8] = b"akuma> ";

// ============================================================================
// SSH State Machine
// ============================================================================

/// SSH connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshState {
    /// Waiting for client to connect
    Disconnected,
    /// Waiting for client version string
    AwaitingVersion,
    /// Key exchange phase (simplified - we skip actual crypto)
    KeyExchange,
    /// Authentication phase (accept anything)
    Authenticating,
    /// Session is active, channel open
    SessionActive,
    /// Shell is ready for commands
    ShellReady,
}

/// SSH session state
struct SshSession {
    state: SshState,
    /// Input buffer for accumulating partial data
    input_buffer: Vec<u8>,
    /// Line buffer for command accumulation
    line_buffer: Vec<u8>,
}

impl SshSession {
    const fn new() -> Self {
        Self {
            state: SshState::Disconnected,
            input_buffer: Vec::new(),
            line_buffer: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.state = SshState::Disconnected;
        self.input_buffer.clear();
        self.line_buffer.clear();
    }
}

static SESSION: Spinlock<SshSession> = Spinlock::new(SshSession::new());

// ============================================================================
// Response Helpers
// ============================================================================

/// Write a string to the response buffer
pub fn write_string(response: &mut Vec<u8>, s: &str) {
    response.extend_from_slice(s.as_bytes());
}

/// Write a newline (CRLF for terminal compatibility) to the response buffer
pub fn write_newline(response: &mut Vec<u8>) {
    response.extend_from_slice(b"\r\n");
}

/// Write bytes directly to the response buffer
pub fn write_bytes(response: &mut Vec<u8>, data: &[u8]) {
    response.extend_from_slice(data);
}

// ============================================================================
// Command Handlers
// ============================================================================

/// Parse and execute a command, returning the response
fn execute_command(line: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();

    // Trim whitespace
    let line = trim_bytes(line);

    if line.is_empty() {
        return response;
    }

    // Parse command and arguments
    let (cmd, args) = split_first_word(line);

    match cmd {
        b"echo" => {
            // Echo back the arguments
            if !args.is_empty() {
                write_bytes(&mut response, args);
            }
            write_newline(&mut response);
        }
        b"cat" => {
            // Print AKUMA_79 ASCII art
            write_bytes(&mut response, AKUMA_79);
            // Ensure it ends with newline
            if !AKUMA_79.ends_with(b"\n") {
                write_newline(&mut response);
            }
        }
        b"quit" | b"exit" => {
            write_string(&mut response, "Goodbye!\r\n");
            // Signal disconnect (handled by caller)
        }
        b"stats" => {
            let (connections, bytes_rx, bytes_tx) = network::get_stats();
            write_string(
                &mut response,
                &alloc::format!(
                "Network Statistics:\r\n  Connections: {}\r\n  Bytes RX: {}\r\n  Bytes TX: {}\r\n",
                connections, bytes_rx, bytes_tx
            ),
            );
        }
        b"help" => {
            write_string(&mut response, "Available commands:\r\n");
            write_string(&mut response, "  echo <text>  - Echo back text\r\n");
            write_string(&mut response, "  cat          - Display ASCII art\r\n");
            write_string(
                &mut response,
                "  stats        - Show network statistics\r\n",
            );
            write_string(&mut response, "  help         - Show this help\r\n");
            write_string(&mut response, "  quit/exit    - Close connection\r\n");
        }
        _ => {
            write_string(&mut response, "Unknown command: ");
            write_bytes(&mut response, cmd);
            write_string(&mut response, "\r\nType 'help' for available commands.\r\n");
        }
    }

    response
}

/// Check if command is quit/exit
fn is_quit_command(line: &[u8]) -> bool {
    let line = trim_bytes(line);
    let (cmd, _) = split_first_word(line);
    cmd == b"quit" || cmd == b"exit"
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Trim leading and trailing whitespace from bytes
fn trim_bytes(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|&b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &data[start..end]
}

/// Split at first whitespace, returning (first_word, rest)
fn split_first_word(data: &[u8]) -> (&[u8], &[u8]) {
    if let Some(pos) = data.iter().position(|&b| b.is_ascii_whitespace()) {
        let rest = &data[pos..];
        let rest_trimmed = trim_bytes(rest);
        (&data[..pos], rest_trimmed)
    } else {
        (data, &[])
    }
}

// ============================================================================
// Protocol Handling
// ============================================================================

/// Handle incoming data based on current state
fn handle_data(data: &[u8]) {
    let mut session = SESSION.lock();

    match session.state {
        SshState::Disconnected => {
            // Shouldn't receive data when disconnected
        }
        SshState::AwaitingVersion => {
            // Accumulate data looking for version string ending in \r\n or \n
            session.input_buffer.extend_from_slice(data);

            // Check for complete version line
            if let Some(pos) = session.input_buffer.iter().position(|&b| b == b'\n') {
                // We got a version string (ignore it for now - we accept anything)
                let _version_line = &session.input_buffer[..pos];

                // Log received version
                log("[SSH] Client version received\n");

                // Clear buffer and move to next state
                session.input_buffer.clear();

                // In minimal SSH, we skip key exchange and go straight to shell
                // This won't work with real SSH clients, but works for testing with nc
                session.state = SshState::ShellReady;

                // Send welcome message and prompt
                let mut response = Vec::new();
                write_string(&mut response, "\r\n");
                write_string(&mut response, "=================================\r\n");
                write_string(&mut response, "  Welcome to Akuma SSH Server\r\n");
                write_string(&mut response, "=================================\r\n");
                write_string(&mut response, "\r\n");
                write_string(&mut response, "Type 'help' for available commands.\r\n");
                write_string(&mut response, "\r\n");
                write_bytes(&mut response, PROMPT);

                drop(session);
                network::ssh_send(&response);
            }
        }
        SshState::KeyExchange | SshState::Authenticating | SshState::SessionActive => {
            // In minimal implementation, these are pass-through
            session.state = SshState::ShellReady;
        }
        SshState::ShellReady => {
            // Process shell input
            // Handle line-by-line input with local echo
            for &byte in data {
                match byte {
                    // Carriage return or newline - execute command
                    b'\r' | b'\n' => {
                        if !session.line_buffer.is_empty() {
                            let line = session.line_buffer.clone();
                            let is_quit = is_quit_command(&line);
                            session.line_buffer.clear();

                            // Execute command
                            drop(session);

                            // Send newline first
                            network::ssh_send(b"\r\n");

                            let response = execute_command(&line);
                            if !response.is_empty() {
                                network::ssh_send(&response);
                            }

                            if is_quit {
                                // Close connection
                                network::ssh_close();
                                let mut session = SESSION.lock();
                                session.reset();
                                return;
                            }

                            // Send prompt
                            network::ssh_send(PROMPT);

                            session = SESSION.lock();
                        } else {
                            // Empty line - just send newline and prompt
                            drop(session);
                            network::ssh_send(b"\r\n");
                            network::ssh_send(PROMPT);
                            session = SESSION.lock();
                        }
                    }
                    // Backspace handling
                    0x7F | 0x08 => {
                        if !session.line_buffer.is_empty() {
                            session.line_buffer.pop();
                            // Echo backspace-space-backspace to erase character
                            drop(session);
                            network::ssh_send(b"\x08 \x08");
                            session = SESSION.lock();
                        }
                    }
                    // Ctrl+C - cancel current line
                    0x03 => {
                        session.line_buffer.clear();
                        drop(session);
                        network::ssh_send(b"^C\r\n");
                        network::ssh_send(PROMPT);
                        session = SESSION.lock();
                    }
                    // Ctrl+D - quit if line is empty
                    0x04 => {
                        if session.line_buffer.is_empty() {
                            drop(session);
                            network::ssh_send(b"\r\nGoodbye!\r\n");
                            network::ssh_close();
                            let mut session = SESSION.lock();
                            session.reset();
                            return;
                        }
                    }
                    // Regular printable character
                    _ if byte >= 0x20 && byte < 0x7F => {
                        session.line_buffer.push(byte);
                        // Echo the character
                        drop(session);
                        network::ssh_send(&[byte]);
                        session = SESSION.lock();
                    }
                    // Ignore other control characters
                    _ => {}
                }
            }
        }
    }
}

/// Handle new connection
fn handle_connect() {
    let mut session = SESSION.lock();
    session.reset();
    session.state = SshState::AwaitingVersion;

    log("[SSH] Client connected\n");

    // Send our version string
    drop(session);
    network::ssh_send(SSH_VERSION);
}

/// Handle disconnect
fn handle_disconnect() {
    let mut session = SESSION.lock();
    session.reset();
    log("[SSH] Client disconnected\n");
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

// ============================================================================
// Public API
// ============================================================================

/// SSH server thread entry point
pub fn ssh_server_entry() -> ! {
    log("[SSH] SSH server thread started\n");
    log("[SSH] Connect with: nc localhost 2222\n");

    loop {
        match network::poll_ssh() {
            SshEvent::Connected => handle_connect(),
            SshEvent::Data(data) => handle_data(&data),
            SshEvent::Disconnected => handle_disconnect(),
            SshEvent::None => {}
        }

        crate::threading::yield_now();
    }
}
