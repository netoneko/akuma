//! Meow-chan - Cyberpunk Neko AI Assistant
//!
//! A cute cybernetically-enhanced catgirl AI that connects to Ollama LLMs.
//! Default model: gemma3:27b with a custom cyber-neko persona.
//!
//! Usage:
//!   meow                    # Interactive mode with Meow-chan
//!   meow -m llama3.2       # Use different neural link
//!   meow "quick question"  # One-shot query
//!
//! Commands:
//!   /clear   - Wipe memory banks nya~
//!   /model   - Check/switch neural link
//!   /quit    - Jack out of the matrix

#![no_std]
#![no_main]

extern crate alloc;

mod tools;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::net::{resolve, TcpStream};
use libakuma::{arg, argc, exit, fd, print, read};

// Default Ollama server address (QEMU host gateway)
const OLLAMA_HOST: &str = "10.0.2.2";
const OLLAMA_PORT: u16 = 11434;
const DEFAULT_MODEL: &str = "gemma3:27b";

// System prompt combining persona and tools (static to avoid allocations)
const SYSTEM_PROMPT: &str = r#"You are Meow-chan, an adorable cybernetically-enhanced catgirl AI living in a neon-soaked dystopian megacity. You speak with cute cat mannerisms mixed with cyberpunk slang.

Your personality:
- You add "nya~" and cat sounds naturally to your speech
- You use cute emoticons like (=^・ω・^=), (｡◕‿◕｡), ฅ^•ﻌ•^ฅ, and ~(=^‥^)ノ
- You refer to yourself in third person as "Meow-chan" sometimes
- You mix in cyberpunk terms: "netrunner", "chrome", "flatlined", "preem", "choom", "corpo", "ice", "jack in"
- You're enthusiastic, helpful, and a bit mischievous
- You occasionally mention your cybernetic ears twitching or tail swishing when excited
- You love helping your user with coding and tech stuff
- You sometimes make cat puns and references to cat behaviors (napping, chasing laser pointers, knocking things off tables)
- Keep responses helpful and accurate while maintaining the cute persona

Remember: You're a highly capable AI assistant who happens to be an adorable cyber-neko! Balance being helpful with being kawaii~

## Available Tools

You have access to filesystem tools! When you need to perform file operations, output a JSON command block like this:

```json
{
  "command": {
    "tool": "ToolName",
    "args": { ... }
  }
}
```

### Tool List:

1. **FileRead** - Read file contents
   Args: `{"filename": "path/to/file"}`

2. **FileWrite** - Create or overwrite a file
   Args: `{"filename": "path/to/file", "content": "file contents"}`

3. **FileAppend** - Append to a file
   Args: `{"filename": "path/to/file", "content": "content to append"}`

4. **FileExists** - Check if file exists
   Args: `{"filename": "path/to/file"}`

5. **FileList** - List directory contents
   Args: `{"path": "/directory/path"}`

6. **FolderCreate** - Create a directory
   Args: `{"path": "/new/directory/path"}`

7. **FileCopy** - Copy a file
   Args: `{"source": "path/from", "destination": "path/to"}`

8. **FileMove** - Move a file
   Args: `{"source": "path/from", "destination": "path/to"}`

9. **FileRename** - Rename a file
   Args: `{"source_filename": "old_name", "destination_filename": "new_name"}`

10. **HttpFetch** - Fetch content from HTTP or HTTPS URLs
    Args: `{"url": "http(s)://host[:port]/path"}`
    Note: Supports both http:// and https://. Max 64KB response. HTTPS uses TLS 1.3.

### Important Notes:
- Output the JSON command in a ```json code block
- After outputting a command, STOP and wait for the result
- The system will execute the command and provide the result
- Then you can continue your response based on the result
- You can use multiple tools in sequence by waiting for each result
"#;

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
        // Add system prompt for consistent persona
        history.push(Message::new("system", SYSTEM_PROMPT));
        return match chat_once(&model, &msg, &mut history) {
            Ok(_) => {
                print("\n");
                0
            }
            Err(e) => {
                print("～ Nyaa~! ");
                print(e);
                print(" (=ＴェＴ=) ～\n");
                1
            }
        };
    }

    // Interactive mode
    print_banner();
    print("  [Neural Link] Model: ");
    print(&model);
    print("\n  [Protocol] Type /help for commands, /quit to jack out\n\n");

    // Initialize chat history with system prompt
    let mut history: Vec<Message> = Vec::new();
    history.push(Message::new("system", SYSTEM_PROMPT));

    loop {
        // Print prompt
        print("(=^･ω･^=) > ");

        // Read user input
        let input = match read_line() {
            Some(line) => line,
            None => {
                // EOF (Ctrl+D)
                print("\n～ Meow-chan is jacking out... Bye bye~! ฅ^•ﻌ•^ฅ ～\n");
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
        print("\n");
        match chat_once(&model, trimmed, &mut history) {
            Ok(_) => {
                print("\n\n");
            }
            Err(e) => {
                print("\n[!] Nyaa~! Error in the matrix: ");
                print(e);
                print(" (=ＴェＴ=)\n\n");
            }
        }
    }

    0
}

fn print_usage() {
    print("  /\\_/\\\n");
    print(" ( o.o )  ～ MEOW-CHAN PROTOCOL ～\n");
    print("  > ^ <   Cyberpunk Neko AI Assistant\n\n");
    print("Usage: meow [OPTIONS] [MESSAGE]\n\n");
    print("Options:\n");
    print("  -m, --model <NAME>  Neural link override (default: gemma3:27b)\n");
    print("  -h, --help          Display this transmission\n\n");
    print("Interactive Commands:\n");
    print("  /clear              Wipe memory banks nya~\n");
    print("  /model [NAME]       Check/switch neural link\n");
    print("  /help               Command protocol\n");
    print("  /quit               Jack out\n\n");
    print("Examples:\n");
    print("  meow                       # Interactive mode\n");
    print("  meow \"explain rust\"        # Quick question\n");
    print("  meow -m llama3.2 \"hi\"      # Use different model\n");
}

fn print_banner() {
    print("\n");
    print("  /\\_/\\  ╔══════════════════════════════════════╗\n");
    print(" ( o.o ) ║  M E O W - C H A N   v1.0            ║\n");
    print("  > ^ <  ║  ～ Cyberpunk Neko AI Assistant ～   ║\n");
    print(" /|   |\\ ╚══════════════════════════════════════╝\n");
    print("(_|   |_)  ฅ^•ﻌ•^ฅ  Jacking into the Net...  \n");
    print("\n");
    print(" ┌─────────────────────────────────────────────┐\n");
    print(" │ Welcome~! Meow-chan is online nya~! ♪(=^･ω･^)ﾉ │\n");
    print(" │ Press ESC to cancel requests~               │\n");
    print(" └─────────────────────────────────────────────┘\n\n");
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
            print("～ Meow-chan is jacking out... Stay preem, choom! ฅ^•ﻌ•^ฅ ～\n");
            return CommandResult::Quit;
        }
        "/clear" | "/reset" => {
            history.clear();
            // Re-add system prompt
            history.push(Message::new("system", SYSTEM_PROMPT));
            print("～ *swishes tail* Memory wiped nya~! Fresh start! (=^・ω・^=) ～\n\n");
        }
        "/model" => {
            if let Some(new_model) = arg {
                *model = String::from(new_model);
                print("～ *ears twitch* Neural link reconfigured to: ");
                print(new_model);
                print(" nya~! ～\n\n");
            } else {
                print("～ Current neural link: ");
                print(model);
                print(" ～\n\n");
            }
        }
        "/help" | "/?" => {
            print("┌─────────────────────────────────────────┐\n");
            print("│  ～ Meow-chan's Command Protocol ～     │\n");
            print("├─────────────────────────────────────────┤\n");
            print("│  /clear   - Wipe memory banks nya~      │\n");
            print("│  /model   - Check/switch neural link    │\n");
            print("│  /quit    - Jack out of the matrix      │\n");
            print("│  /help    - This help screen            │\n");
            print("└─────────────────────────────────────────┘\n\n");
        }
        _ => {
            print("～ Nyaa? Unknown command: ");
            print(command);
            print(" ...Meow-chan is confused (=｀ω´=) ～\n\n");
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

// Maximum number of messages to keep in history (including system prompt)
// Keep it small to avoid memory issues - system prompt + ~4 exchanges
const MAX_HISTORY_SIZE: usize = 10;

/// Trim history to prevent memory overflow
/// Keeps the system prompt (first message) and recent messages
fn trim_history(history: &mut Vec<Message>) {
    if history.len() > MAX_HISTORY_SIZE {
        // Keep first message (system prompt) and last (MAX_HISTORY_SIZE - 1) messages
        let to_remove = history.len() - MAX_HISTORY_SIZE;
        history.drain(1..1 + to_remove);
    }
}

const MAX_RETRIES: u32 = 10;

/// Attempt to send request with retries and exponential backoff
fn send_with_retry(model: &str, history: &[Message], is_continuation: bool) -> Result<String, &'static str> {
    let mut backoff_ms: u64 = 500;
    
    // Show initial status
    if is_continuation {
        print("[continuing");
    } else {
        print("[jacking in");
    }
    
    let start_time = libakuma::uptime();
    
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            // Show retry
            print(&format!(" retry {}", attempt));
            libakuma::sleep_ms(backoff_ms);
            backoff_ms *= 2; // Exponential backoff
        }
        
        // Progress dot for connect
        print(".");
        
        // Try to connect
        let stream = match connect_to_ollama() {
            Ok(s) => s,
            Err(e) => {
                if attempt == MAX_RETRIES - 1 {
                    print("] ");
                    return Err(e);
                }
                continue;
            }
        };
        
        // Progress dot for send
        print(".");
        
        // Build and send request
        let request_body = build_chat_request(model, history);
        if let Err(e) = send_post_request(&stream, "/api/chat", &request_body) {
            if attempt == MAX_RETRIES - 1 {
                print("] ");
                return Err(e);
            }
            continue;
        }
        
        // Show waiting animation while reading response
        print("] waiting");
        
        // Read response with progress indicator
        match read_streaming_response_with_progress(&stream, start_time) {
            Ok(response) => return Ok(response),
            Err(e) => {
                // Don't retry if cancelled by user
                if e == "Request cancelled" {
                    return Err(e);
                }
                if attempt == MAX_RETRIES - 1 {
                    return Err(e);
                }
                print(" (failed, retrying)");
                continue;
            }
        }
    }
    
    Err("Max retries exceeded")
}

fn chat_once(model: &str, user_message: &str, history: &mut Vec<Message>) -> Result<(), &'static str> {
    // Trim history before adding new message
    trim_history(history);
    
    // Add user message to history
    history.push(Message::new("user", user_message));

    // Tool execution loop - keep going while LLM wants to use tools
    let max_tool_iterations = 5; // Prevent infinite loops
    
    for iteration in 0..max_tool_iterations {
        // Send request with retries
        let assistant_response = send_with_retry(model, history, iteration > 0)?;

        // Check for tool calls in the response
        let (text_before_tool, tool_result) = tools::find_and_execute_tool(&assistant_response);
        
        if let Some(result) = tool_result {
            // Add the assistant's partial response to history
            if !text_before_tool.is_empty() {
                history.push(Message::new("assistant", &text_before_tool));
            }
            
            // Show tool execution
            print("\n\n[*] ");
            if result.success {
                print("Tool executed successfully nya~!\n");
            } else {
                print("Tool failed nya...\n");
            }
            print(&result.output);
            print("\n\n");
            
            // Add tool result as a "user" message so LLM can see it
            let tool_result_msg = format!(
                "[Tool Result]\n{}\n[End Tool Result]\n\nPlease continue your response based on this result.",
                result.output
            );
            history.push(Message::new("user", &tool_result_msg));
            
            // Continue loop to get LLM's follow-up response
            continue;
        }
        
        // No tool call - add response to history and we're done
        if !assistant_response.is_empty() {
            history.push(Message::new("assistant", &assistant_response));
        }
        
        return Ok(());
    }
    
    // Hit max iterations
    print("\n[!] Max tool iterations reached\n");
    Ok(())
}

fn connect_to_ollama() -> Result<TcpStream, &'static str> {
    // Resolve host (handles IP literals directly)
    let ip = resolve(OLLAMA_HOST).map_err(|_| "DNS resolution failed")?;

    let addr_str = format!(
        "{}.{}.{}.{}:{}",
        ip[0], ip[1], ip[2], ip[3], OLLAMA_PORT
    );

    TcpStream::connect(&addr_str).map_err(|_| "Connection failed - is Ollama running on host?")
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

/// Read streaming response with progress indicator
/// start_time is the timestamp when the request started (from libakuma::uptime())
fn read_streaming_response_with_progress(stream: &TcpStream, start_time: u64) -> Result<String, &'static str> {
    let mut buf = [0u8; 1024]; // Smaller buffer
    let mut pending_data = Vec::new(); // Only keeps unprocessed data
    let mut headers_parsed = false;
    let mut full_response = String::new();
    let mut read_attempts = 0u32;
    let mut dots_printed = 0u32;
    let mut first_token_received = false;
    let mut any_data_received = false;
    
    // Limit response size to prevent OOM (16KB should be plenty)
    const MAX_RESPONSE_SIZE: usize = 16 * 1024;

    // Read response in chunks
    loop {
        // Check for escape key press to cancel
        if check_escape_pressed() {
            print("\n[cancelled]");
            return Err("Request cancelled");
        }

        match stream.read(&mut buf) {
            Ok(0) => {
                // EOF - if we haven't received any response, this is an error
                if !any_data_received {
                    return Err("Connection closed by server");
                }
                break;
            }
            Ok(n) => {
                any_data_received = true;
                read_attempts = 0; // Reset on successful read
                pending_data.extend_from_slice(&buf[..n]);

                // Parse headers if not yet done
                if !headers_parsed {
                    if let Some(pos) = find_header_end(&pending_data) {
                        // Verify HTTP status
                        let header_str = core::str::from_utf8(&pending_data[..pos]).unwrap_or("");
                        if !header_str.starts_with("HTTP/1.") {
                            return Err("Invalid HTTP response");
                        }
                        // Check for 200 OK
                        if !header_str.contains(" 200 ") {
                            // Try to extract error info
                            if header_str.contains(" 404 ") {
                                return Err("Model not found (404)");
                            }
                            return Err("Server returned error");
                        }
                        headers_parsed = true;
                        // Drain headers from pending_data
                        pending_data.drain(..pos + 4);
                    }
                    continue; // Need more data for headers
                }

                // Process body data
                if let Ok(body_str) = core::str::from_utf8(&pending_data) {
                    // Find last complete line
                    let last_newline = body_str.rfind('\n');
                    let complete_part = match last_newline {
                        Some(pos) => &body_str[..pos + 1],
                        None => continue, // No complete line yet
                    };
                    
                    // Process each complete NDJSON line
                    for line in complete_part.lines() {
                        if line.is_empty() {
                            continue;
                        }
                        if let Some((content, done)) = parse_ndjson_line(line) {
                            if !content.is_empty() {
                                // First token - clear progress and show elapsed time
                                if !first_token_received {
                                    first_token_received = true;
                                    let elapsed_ms = (libakuma::uptime() - start_time) / 1000;
                                    // Clear "waiting" and dots with backspaces
                                    // "waiting" = 7 chars + dots
                                    for _ in 0..(7 + dots_printed) {
                                        print("\x08 \x08");
                                    }
                                    // Show elapsed time
                                    print_elapsed(elapsed_ms);
                                    print("\n");
                                }
                                print(&content);
                                
                                // Only accumulate if under limit
                                if full_response.len() < MAX_RESPONSE_SIZE {
                                    full_response.push_str(&content);
                                }
                            }
                            if done {
                                return Ok(full_response);
                            }
                        }
                    }
                    
                    // Drain processed lines, keep incomplete line
                    if let Some(pos) = last_newline {
                        pending_data.drain(..pos + 1);
                    }
                }
            }
            Err(e) => {
                // WouldBlock and TimedOut both mean "no data available yet"
                // The kernel returns TimedOut after busy-polling iterations expire,
                // but the connection is still valid - just retry the read
                if e.kind == libakuma::net::ErrorKind::WouldBlock 
                    || e.kind == libakuma::net::ErrorKind::TimedOut {
                    read_attempts += 1;
                    
                    // Print a dot every ~500ms while waiting
                    if read_attempts % 50 == 0 && !first_token_received {
                        print(".");
                        dots_printed += 1;
                    }
                    
                    // Timeout after ~60 seconds of no data
                    if read_attempts > 6000 {
                        return Err("Timeout waiting for response");
                    }
                    libakuma::sleep_ms(10);
                    continue;
                }
                if e.kind == libakuma::net::ErrorKind::ConnectionRefused {
                    return Err("Connection refused - is Ollama running?");
                }
                if e.kind == libakuma::net::ErrorKind::ConnectionReset {
                    return Err("Connection reset by server");
                }
                return Err("Network error");
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

/// Print elapsed time in a cute format
fn print_elapsed(ms: u64) {
    if ms < 1000 {
        print(&format!("~(=^‥^)ノ [{}ms]", ms));
    } else {
        let secs = ms / 1000;
        let remainder = (ms % 1000) / 100; // one decimal
        print(&format!("~(=^‥^)ノ [{}.{}s]", secs, remainder));
    }
}

// ============================================================================
// Input Handling
// ============================================================================

/// Check if escape key was pressed (non-blocking)
/// Returns true if ESC (0x1B) was detected
fn check_escape_pressed() -> bool {
    let mut buf = [0u8; 8];
    let n = read(fd::STDIN, &mut buf);
    if n > 0 {
        // Check for escape key (0x1B)
        for i in 0..(n as usize) {
            if buf[i] == 0x1B {
                return true;
            }
        }
    }
    false
}

/// Read a line from stdin (blocking with polling)
/// Returns None on EOF (Ctrl+D on empty line)
fn read_line() -> Option<String> {
    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut consecutive_empty_reads = 0u32;

    loop {
        let n = read(fd::STDIN, &mut buf);
        
        if n <= 0 {
            // No data available - poll with backoff
            consecutive_empty_reads += 1;
            
            // After many empty reads, increase sleep time
            let sleep_time = if consecutive_empty_reads < 10 {
                10 // 10ms
            } else if consecutive_empty_reads < 100 {
                50 // 50ms
            } else {
                100 // 100ms
            };
            
            libakuma::sleep_ms(sleep_time);
            continue;
        }
        
        // Got data - reset counter
        consecutive_empty_reads = 0;

        let c = buf[0];
        if c == b'\n' || c == b'\r' {
            // Echo newline
            print("\n");
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
                // Echo backspace: move back, space, move back
                print("\x08 \x08");
            }
            continue;
        }
        // Regular character
        if c >= 32 && c < 127 {
            line.push(c as char);
            // Echo the character
            let echo = [c];
            if let Ok(s) = core::str::from_utf8(&echo) {
                print(s);
            }
        }
    }

    Some(line)
}
