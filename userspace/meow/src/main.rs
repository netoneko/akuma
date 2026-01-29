//! Meow - Interactive LLM chat client for Ollama
//!
//! Usage:
//!   meow                    # Start interactive chat (default model: llama3.2)
//!   meow -m mistral        # Use specific model
//!   meow "quick question"  # One-shot mode
//!
//! Commands:
//!   /clear   - Clear conversation history
//!   /model   - Show or change model
//!   /quit    - Exit the chat

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, TcpStream};
use libakuma::{arg, argc, exit, fd, print, read};

// Default Ollama server address (QEMU host gateway)
const OLLAMA_HOST: &str = "10.0.2.2";
const OLLAMA_PORT: u16 = 11434;
const DEFAULT_MODEL: &str = "llama3.2";

// ============================================================================
// Entry Point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let code = main();
    exit(code);
}

fn main() -> i32 {
    let mut model = String::from(DEFAULT_MODEL);
    let mut one_shot_message: Option<String> = None;

    // Parse command line arguments
    let mut i = 1;
    while i < argc() {
        if let Some(arg_str) = arg(i) {
            if arg_str == "-m" || arg_str == "--model" {
                // Next arg is model name
                i += 1;
                if let Some(m) = arg(i) {
                    model = String::from(m);
                } else {
                    print("meow: -m requires a model name\n");
                    return 1;
                }
            } else if arg_str == "-h" || arg_str == "--help" {
                print_usage();
                return 0;
            } else if !arg_str.starts_with('-') {
                // Treat as one-shot message
                one_shot_message = Some(String::from(arg_str));
            }
        }
        i += 1;
    }

    // One-shot mode
    if let Some(msg) = one_shot_message {
        let mut history = Vec::new();
        return match chat_once(&model, &msg, &mut history) {
            Ok(_) => {
                print("\n");
                0
            }
            Err(e) => {
                print("meow: ");
                print(e);
                print("\n");
                1
            }
        };
    }

    // Interactive mode
    print_banner();
    print("Model: ");
    print(&model);
    print("\nType /help for commands, /quit to exit.\n\n");

    let mut history: Vec<Message> = Vec::new();

    loop {
        // Print prompt
        print("you> ");

        // Read user input
        let input = match read_line() {
            Some(line) => line,
            None => {
                // EOF (Ctrl+D)
                print("\nGoodbye!\n");
                break;
            }
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Handle commands
        if trimmed.starts_with('/') {
            match handle_command(trimmed, &mut model, &mut history) {
                CommandResult::Continue => continue,
                CommandResult::Quit => break,
            }
        }

        // Send message to Ollama
        print("meow> ");
        match chat_once(&model, trimmed, &mut history) {
            Ok(_) => {
                print("\n\n");
            }
            Err(e) => {
                print("\n[Error: ");
                print(e);
                print("]\n\n");
            }
        }
    }

    0
}

fn print_usage() {
    print("Usage: meow [OPTIONS] [MESSAGE]\n\n");
    print("Options:\n");
    print("  -m, --model <NAME>  Use specific model (default: llama3.2)\n");
    print("  -h, --help          Show this help\n\n");
    print("Commands (in interactive mode):\n");
    print("  /clear              Clear conversation history\n");
    print("  /model [NAME]       Show or change model\n");
    print("  /help               Show commands\n");
    print("  /quit               Exit\n");
}

fn print_banner() {
    print("\n");
    print("  __  __                  \n");
    print(" |  \\/  | ___  _____      __\n");
    print(" | |\\/| |/ _ \\/ _ \\ \\ /\\ / /\n");
    print(" | |  | |  __/ (_) \\ V  V / \n");
    print(" |_|  |_|\\___|\\___/ \\_/\\_/  \n");
    print("\n");
    print(" Chat with LLMs via Ollama\n\n");
}

// ============================================================================
// Command Handling
// ============================================================================

enum CommandResult {
    Continue,
    Quit,
}

fn handle_command(cmd: &str, model: &mut String, history: &mut Vec<Message>) -> CommandResult {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    let command = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match command {
        "/quit" | "/exit" | "/q" => {
            print("Goodbye!\n");
            return CommandResult::Quit;
        }
        "/clear" | "/reset" => {
            history.clear();
            print("[Conversation cleared]\n\n");
        }
        "/model" => {
            if let Some(new_model) = arg {
                *model = String::from(new_model);
                print("[Model changed to: ");
                print(new_model);
                print("]\n\n");
            } else {
                print("[Current model: ");
                print(model);
                print("]\n\n");
            }
        }
        "/help" | "/?" => {
            print("Commands:\n");
            print("  /clear   - Clear conversation history\n");
            print("  /model   - Show or change model\n");
            print("  /quit    - Exit the chat\n\n");
        }
        _ => {
            print("[Unknown command: ");
            print(command);
            print("]\n\n");
        }
    }

    CommandResult::Continue
}

// ============================================================================
// Chat Message Types
// ============================================================================

#[derive(Clone)]
struct Message {
    role: String,
    content: String,
}

impl Message {
    fn new(role: &str, content: &str) -> Self {
        Self {
            role: String::from(role),
            content: String::from(content),
        }
    }

    fn to_json(&self) -> String {
        let escaped_content = json_escape(&self.content);
        format!(
            "{{\"role\":\"{}\",\"content\":\"{}\"}}",
            self.role, escaped_content
        )
    }
}

// ============================================================================
// Ollama API Communication
// ============================================================================

fn chat_once(model: &str, user_message: &str, history: &mut Vec<Message>) -> Result<(), &'static str> {
    // Add user message to history
    history.push(Message::new("user", user_message));

    // Build the request body
    let request_body = build_chat_request(model, history);

    // Connect to Ollama
    let stream = connect_to_ollama()?;

    // Send HTTP POST request
    send_post_request(&stream, "/api/chat", &request_body)?;

    // Read and stream the response
    let assistant_response = read_streaming_response(&stream)?;

    // Add assistant response to history
    if !assistant_response.is_empty() {
        history.push(Message::new("assistant", &assistant_response));
    }

    Ok(())
}

fn connect_to_ollama() -> Result<TcpStream, &'static str> {
    // Resolve host (handles IP literals directly)
    let ip = resolve(OLLAMA_HOST).map_err(|_| "DNS resolution failed")?;

    let addr_str = format!(
        "{}.{}.{}.{}:{}",
        ip[0], ip[1], ip[2], ip[3], OLLAMA_PORT
    );

    TcpStream::connect(&addr_str).map_err(|_| "Connection failed")
}

fn build_chat_request(model: &str, history: &[Message]) -> String {
    let mut messages_json = String::from("[");
    for (i, msg) in history.iter().enumerate() {
        if i > 0 {
            messages_json.push(',');
        }
        messages_json.push_str(&msg.to_json());
    }
    messages_json.push(']');

    format!(
        "{{\"model\":\"{}\",\"messages\":{},\"stream\":true}}",
        model, messages_json
    )
}

// ============================================================================
// HTTP Client
// ============================================================================

fn send_post_request(stream: &TcpStream, path: &str, body: &str) -> Result<(), &'static str> {
    let request = format!(
        "POST {} HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        path,
        OLLAMA_HOST,
        OLLAMA_PORT,
        body.len(),
        body
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|_| "Failed to send request")
}

fn read_streaming_response(stream: &TcpStream) -> Result<String, &'static str> {
    let mut buf = [0u8; 4096];
    let mut response_data = Vec::new();
    let mut headers_parsed = false;
    let mut body_start = 0;
    let mut full_response = String::new();

    // Read response in chunks
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                response_data.extend_from_slice(&buf[..n]);

                // Parse headers if not yet done
                if !headers_parsed {
                    if let Some(pos) = find_header_end(&response_data) {
                        // Verify HTTP status
                        let header_str = core::str::from_utf8(&response_data[..pos]).unwrap_or("");
                        if !header_str.starts_with("HTTP/1.") {
                            return Err("Invalid HTTP response");
                        }
                        // Check for 200 OK
                        if !header_str.contains(" 200 ") {
                            return Err("Server returned error");
                        }
                        headers_parsed = true;
                        body_start = pos + 4; // Skip \r\n\r\n
                    }
                }

                // Process body data if headers are parsed
                if headers_parsed && body_start < response_data.len() {
                    let body_data = &response_data[body_start..];
                    if let Ok(body_str) = core::str::from_utf8(body_data) {
                        // Process each complete NDJSON line
                        for line in body_str.lines() {
                            if line.is_empty() {
                                continue;
                            }
                            if let Some((content, done)) = parse_ndjson_line(line) {
                                if !content.is_empty() {
                                    print(&content);
                                    full_response.push_str(&content);
                                }
                                if done {
                                    return Ok(full_response);
                                }
                            }
                        }
                        // Keep only incomplete line data
                        if let Some(last_newline) = body_str.rfind('\n') {
                            body_start = body_start + last_newline + 1;
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind == libakuma::net::ErrorKind::WouldBlock {
                    libakuma::sleep_ms(10);
                    continue;
                }
                return Err("Read error");
            }
        }
    }

    Ok(full_response)
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

// ============================================================================
// JSON Parsing (minimal, for NDJSON response)
// ============================================================================

/// Parse a single NDJSON line from Ollama response
/// Returns (content, done) where content is the token and done indicates completion
fn parse_ndjson_line(line: &str) -> Option<(String, bool)> {
    // Look for "done":true or "done":false
    let done = line.contains("\"done\":true") || line.contains("\"done\": true");

    // Extract content from: "message":{"role":"assistant","content":"..."}
    // We look for "content":" and extract until the next unescaped quote
    let content = extract_json_string(line, "content").unwrap_or_default();

    Some((content, done))
}

/// Extract a string value from JSON by key
/// Handles basic escape sequences
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    // Build search pattern: "key":"
    let pattern = format!("\"{}\":\"", key);
    let start = json.find(&pattern)?;
    let value_start = start + pattern.len();

    // Find the end quote (handling escapes)
    let rest = &json[value_start..];
    let mut result = String::new();
    let mut chars = rest.chars().peekable();
    
    while let Some(c) = chars.next() {
        match c {
            '"' => break, // End of string
            '\\' => {
                // Handle escape sequences
                if let Some(&next) = chars.peek() {
                    chars.next();
                    match next {
                        'n' => result.push('\n'),
                        'r' => result.push('\r'),
                        't' => result.push('\t'),
                        '"' => result.push('"'),
                        '\\' => result.push('\\'),
                        '/' => result.push('/'),
                        'u' => {
                            // Unicode escape: \uXXXX
                            let mut hex = String::new();
                            for _ in 0..4 {
                                if let Some(h) = chars.next() {
                                    hex.push(h);
                                }
                            }
                            if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(code) {
                                    result.push(ch);
                                }
                            }
                        }
                        _ => {
                            result.push('\\');
                            result.push(next);
                        }
                    }
                }
            }
            _ => result.push(c),
        }
    }

    Some(result)
}

/// Escape a string for JSON
fn json_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                // Use \uXXXX for other control characters
                let code = c as u32;
                result.push_str(&format!("\\u{:04x}", code));
            }
            _ => result.push(c),
        }
    }
    result
}

// ============================================================================
// Input Handling
// ============================================================================

/// Read a line from stdin (blocking)
/// Returns None on EOF
fn read_line() -> Option<String> {
    let mut line = String::new();
    let mut buf = [0u8; 1];

    loop {
        let n = read(fd::STDIN, &mut buf);
        if n <= 0 {
            // EOF or error
            if line.is_empty() {
                return None;
            }
            break;
        }

        let c = buf[0];
        if c == b'\n' || c == b'\r' {
            break;
        }
        if c == 4 {
            // Ctrl+D
            if line.is_empty() {
                return None;
            }
            break;
        }
        // Handle backspace
        if c == 8 || c == 127 {
            if !line.is_empty() {
                line.pop();
            }
            continue;
        }
        // Regular character
        if c >= 32 && c < 127 {
            line.push(c as char);
        }
    }

    Some(line)
}
