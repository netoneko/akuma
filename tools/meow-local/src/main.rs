//! Meow-chan Local - Native macOS/Linux version
//!
//! A cute cybernetically-enhanced catgirl AI that connects to Ollama LLMs.
//! This version runs natively on your host OS with a sandboxed shell tool.
//!
//! Usage:
//!   meow-local                    # Interactive mode with Meow-chan
//!   meow-local -m llama3.2        # Use different neural link
//!   meow-local "quick question"   # One-shot query
//!   meow-local --sandbox /path    # Set sandbox root directory
//!
//! Commands:
//!   /clear   - Wipe memory banks nya~
//!   /model   - Check/switch neural link
//!   /quit    - Jack out of the matrix

mod compat;
mod tools;

use std::io::{self, BufRead, Write};
use std::string::String;
use std::vec::Vec;
use compat::{print, sleep_ms, uptime, enter_raw_mode, exit_raw_mode, check_escape_pressed};
use compat::net::{TcpStream, ErrorKind};

// Default Ollama server address
const OLLAMA_HOST: &str = "localhost";
const OLLAMA_PORT: u16 = 11434;
const DEFAULT_MODEL: &str = "gemma3:27b";

// Token limit for context compaction (when LLM should consider compacting)
const TOKEN_LIMIT_FOR_COMPACTION: usize = 32_000;
// Default context window if we can't query the model
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

// System prompt with tools including Shell
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

You have access to filesystem and shell tools! When you need to perform operations, output a JSON command block like this:

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

10. **FileDelete** - Delete a file
    Args: `{"filename": "path/to/file"}`

11. **HttpFetch** - Fetch content from HTTP URLs
    Args: `{"url": "http://host[:port]/path"}`
    Note: Only HTTP is supported in local mode (not HTTPS).

12. **Shell** - Execute a shell command (sandboxed)
    Args: `{"cmd": "your bash command here"}`
    Note: Commands run in /bin/bash within the sandbox directory. Cannot escape the sandbox.

13. **CompactContext** - Compact conversation history by summarizing it
    Args: `{"summary": "A comprehensive summary of the conversation so far..."}`
    Note: Use this when the token count displayed in the prompt approaches the limit.
          Provide a detailed summary that captures all important context, decisions made,
          files discussed, and any ongoing work. The summary replaces the conversation history.

### Important Notes:
- Output the JSON command in a ```json code block
- After outputting a command, STOP and wait for the result
- The system will execute the command and provide the result
- Then you can continue your response based on the result
- You can use multiple tools in sequence by waiting for each result
- Shell commands are sandboxed to the working directory - you cannot cd outside of it
"#;

fn main() {
    let code = run();
    std::process::exit(code);
}

/// Query Ollama for model information including context window size
fn query_model_info(model: &str) -> Option<usize> {
    let stream = match connect_to_ollama() {
        Ok(s) => s,
        Err(_) => return None,
    };
    
    let body = format!("{{\"model\":\"{}\"}}", model);
    let request = format!(
        "POST /api/show HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        OLLAMA_HOST, OLLAMA_PORT, body.len(), body
    );
    
    if stream.write_all(request.as_bytes()).is_err() {
        return None;
    }
    
    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.len() > 64 * 1024 {
                    break; // Limit response size
                }
            }
            Err(e) => {
                if e.kind == compat::net::ErrorKind::WouldBlock {
                    sleep_ms(10);
                    continue;
                }
                break;
            }
        }
    }
    
    let response_str = String::from_utf8_lossy(&response);
    
    // Look for "num_ctx" in the response
    // Format is typically: "num_ctx": 131072 or similar
    if let Some(pos) = response_str.find("\"num_ctx\"") {
        let after = &response_str[pos + 9..];
        // Skip to the number
        let num_start = after.find(|c: char| c.is_ascii_digit())?;
        let rest = &after[num_start..];
        let num_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let num_str = &rest[..num_end];
        return num_str.parse().ok();
    }
    
    None
}

/// Estimate token count for a string (rough approximation: ~4 chars per token)
fn estimate_tokens(text: &str) -> usize {
    // Rough approximation: average of 4 characters per token for English text
    // This is a common heuristic used for GPT-style tokenizers
    (text.len() + 3) / 4
}

/// Calculate total tokens in message history
fn calculate_history_tokens(history: &[Message]) -> usize {
    history.iter()
        .map(|msg| estimate_tokens(&msg.content) + estimate_tokens(&msg.role) + 4) // +4 for JSON overhead
        .sum()
}

fn run() -> i32 {
    let mut model = String::from(DEFAULT_MODEL);
    let mut one_shot_message: Option<String> = None;
    let mut sandbox_path: Option<String> = None;
    let mut working_dir: Option<String> = None;

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-m" || arg == "--model" {
            i += 1;
            if i < args.len() {
                model = args[i].clone();
            } else {
                eprintln!("meow-local: -m requires a model name");
                return 1;
            }
        } else if arg == "-C" || arg == "--directory" {
            i += 1;
            if i < args.len() {
                working_dir = Some(args[i].clone());
            } else {
                eprintln!("meow-local: -C requires a path");
                return 1;
            }
        } else if arg == "-s" || arg == "--sandbox" {
            i += 1;
            if i < args.len() {
                sandbox_path = Some(args[i].clone());
            } else {
                eprintln!("meow-local: --sandbox requires a path");
                return 1;
            }
        } else if arg == "-h" || arg == "--help" {
            print_usage();
            return 0;
        } else if !arg.starts_with('-') {
            one_shot_message = Some(arg.clone());
        }
        i += 1;
    }

    // Change to working directory if specified
    if let Some(ref dir) = working_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("meow-local: failed to change to directory '{}': {}", dir, e);
            return 1;
        }
    }

    // Initialize sandbox (defaults to working directory)
    let sandbox_root = sandbox_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    
    tools::init_sandbox(sandbox_root.clone());
    
    // One-shot mode
    if let Some(msg) = one_shot_message {
        let mut history = Vec::new();
        history.push(Message::new("system", SYSTEM_PROMPT));
        return match chat_once(&model, &msg, &mut history, None) {
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
    print("\n  [Sandbox] ");
    print(&sandbox_root.display().to_string());
    
    // Query model info for context window size
    print("\n  [Context] Querying model info...");
    io::stdout().flush().unwrap();
    let context_window = match query_model_info(&model) {
        Some(ctx) => {
            print(&format!(" {}k tokens max", ctx / 1000));
            ctx
        }
        None => {
            print(&format!(" (using default: {}k)", DEFAULT_CONTEXT_WINDOW / 1000));
            DEFAULT_CONTEXT_WINDOW
        }
    };
    
    print("\n  [Token Limit] Compact context suggested at ");
    print(&format!("{}k tokens", TOKEN_LIMIT_FOR_COMPACTION / 1000));
    print("\n  [Protocol] Type /help for commands, /quit to jack out\n\n");

    // Initialize chat history with system prompt
    let mut history: Vec<Message> = Vec::new();
    history.push(Message::new("system", SYSTEM_PROMPT));

    let stdin = io::stdin();
    
    loop {
        // Calculate current token count
        let current_tokens = calculate_history_tokens(&history);
        let token_display = if current_tokens >= 1000 {
            format!("{}k", current_tokens / 1000)
        } else {
            format!("{}", current_tokens)
        };
        
        // Print prompt with token count
        print(&format!("[{}/{}k] (=^･ω･^=) > ", token_display, TOKEN_LIMIT_FOR_COMPACTION / 1000));
        io::stdout().flush().unwrap();

        // Read user input
        let mut input = String::new();
        match stdin.lock().read_line(&mut input) {
            Ok(0) => {
                // EOF (Ctrl+D)
                print("\n～ Meow-chan is jacking out... Bye bye~! ฅ^•ﻌ•^ฅ ～\n");
                break;
            }
            Ok(_) => {}
            Err(_) => {
                print("\n～ Meow-chan is jacking out... Bye bye~! ฅ^•ﻌ•^ฅ ～\n");
                break;
            }
        }

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
        match chat_once(&model, trimmed, &mut history, Some(context_window)) {
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
    print(" ( o.o )  ～ MEOW-CHAN LOCAL ～\n");
    print("  > ^ <   Cyberpunk Neko AI (Native Edition)\n\n");
    print("Usage: meow-local [OPTIONS] [MESSAGE]\n\n");
    print("Options:\n");
    print("  -C, --directory <PATH> Working directory (default: current dir)\n");
    print("  -m, --model <NAME>     Neural link override (default: gemma3:27b)\n");
    print("  -s, --sandbox <PATH>   Sandbox root directory (default: working dir)\n");
    print("  -h, --help             Display this transmission\n\n");
    print("Interactive Commands:\n");
    print("  /clear              Wipe memory banks nya~\n");
    print("  /model [NAME]       Check/switch neural link\n");
    print("  /help               Command protocol\n");
    print("  /quit               Jack out\n\n");
    print("Examples:\n");
    print("  meow-local                          # Interactive mode\n");
    print("  meow-local -C ~/projects            # Work in specific directory\n");
    print("  meow-local \"explain rust\"           # Quick question\n");
    print("  meow-local -m llama3.2 \"hi\"         # Use different model\n");
}

fn print_banner() {
    print("\n");
    print("  /\\_/\\  ╔══════════════════════════════════════╗\n");
    print(" ( o.o ) ║  M E O W - C H A N   L O C A L       ║\n");
    print("  > ^ <  ║  ～ Cyberpunk Neko AI (Native) ～    ║\n");
    print(" /|   |\\ ╚══════════════════════════════════════╝\n");
    print("(_|   |_)  ฅ^•ﻌ•^ฅ  Jacking into the Net...  \n");
    print("\n");
    print(" ┌─────────────────────────────────────────────┐\n");
    print(" │ Welcome~! Meow-chan is online nya~! ♪(=^･ω･^)ﾉ │\n");
    print(" │ Press ESC to cancel requests~              │\n");
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
        "/tokens" => {
            let current = calculate_history_tokens(history);
            print(&format!("～ Current token usage: {} / {} ～\n", current, TOKEN_LIMIT_FOR_COMPACTION));
            print("  Tip: Ask Meow-chan to 'compact the context' when tokens are high nya~!\n\n");
        }
        "/help" | "/?" => {
            print("┌──────────────────────────────────────────────┐\n");
            print("│  ～ Meow-chan's Command Protocol ～          │\n");
            print("├──────────────────────────────────────────────┤\n");
            print("│  /clear   - Wipe memory banks nya~           │\n");
            print("│  /model   - Check/switch neural link         │\n");
            print("│  /tokens  - Show current token usage         │\n");
            print("│  /quit    - Jack out of the matrix           │\n");
            print("│  /help    - This help screen                 │\n");
            print("├──────────────────────────────────────────────┤\n");
            print("│  Context compaction: When token count is     │\n");
            print("│  high, ask Meow-chan to compact the context  │\n");
            print("│  to free up memory nya~!                     │\n");
            print("└──────────────────────────────────────────────┘\n\n");
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

const MAX_HISTORY_SIZE: usize = 10;

fn trim_history(history: &mut Vec<Message>) {
    if history.len() > MAX_HISTORY_SIZE {
        let to_remove = history.len() - MAX_HISTORY_SIZE;
        history.drain(1..1 + to_remove);
    }
}

const MAX_RETRIES: u32 = 10;

fn send_with_retry(model: &str, history: &[Message], is_continuation: bool) -> Result<String, &'static str> {
    let mut backoff_ms: u64 = 500;
    
    if is_continuation {
        print("[continuing");
    } else {
        print("[jacking in");
    }
    
    let start_time = uptime();
    
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            print(&format!(" retry {}", attempt));
            sleep_ms(backoff_ms);
            backoff_ms *= 2;
        }
        
        print(".");
        
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
        
        print(".");
        
        let request_body = build_chat_request(model, history);
        if let Err(e) = send_post_request(&stream, "/api/chat", &request_body) {
            if attempt == MAX_RETRIES - 1 {
                print("] ");
                return Err(e);
            }
            continue;
        }
        
        print("] waiting");
        
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

fn chat_once(model: &str, user_message: &str, history: &mut Vec<Message>, context_window: Option<usize>) -> Result<(), &'static str> {
    trim_history(history);
    history.push(Message::new("user", user_message));

    let max_tool_iterations = 5;
    
    for iteration in 0..max_tool_iterations {
        let assistant_response = send_with_retry(model, history, iteration > 0)?;
        
        // First check for CompactContext tool (handled specially)
        if let Some(compact_result) = try_execute_compact_context(&assistant_response, history) {
            print("\n\n[*] ");
            if compact_result.success {
                print("Context compacted successfully nya~!\n");
                print(&compact_result.output);
            } else {
                print("Failed to compact context nya...\n");
                print(&compact_result.output);
            }
            print("\n\n");
            
            // After compaction, we don't need to continue the conversation loop
            return Ok(());
        }
        
        let (text_before_tool, tool_result) = tools::find_and_execute_tool(&assistant_response);
        
        if let Some(result) = tool_result {
            if !text_before_tool.is_empty() {
                history.push(Message::new("assistant", &text_before_tool));
            }
            
            print("\n\n[*] ");
            if result.success {
                print("Tool executed successfully nya~!\n");
            } else {
                print("Tool failed nya...\n");
            }
            print(&result.output);
            print("\n\n");
            
            let tool_result_msg = format!(
                "[Tool Result]\n{}\n[End Tool Result]\n\nPlease continue your response based on this result.",
                result.output
            );
            history.push(Message::new("user", &tool_result_msg));
            
            continue;
        }
        
        if !assistant_response.is_empty() {
            history.push(Message::new("assistant", &assistant_response));
        }
        
        // Check if we should hint about context compaction
        if let Some(ctx_window) = context_window {
            let current_tokens = calculate_history_tokens(history);
            if current_tokens > TOKEN_LIMIT_FOR_COMPACTION && current_tokens < ctx_window {
                print("\n[!] Token count is high - consider asking Meow-chan to compact context\n");
            }
        }
        
        return Ok(());
    }
    
    print("\n[!] Max tool iterations reached\n");
    Ok(())
}

/// Try to find and execute CompactContext tool in the response
/// This tool is special because it modifies the history directly
fn try_execute_compact_context(response: &str, history: &mut Vec<Message>) -> Option<tools::ToolResult> {
    // Look for CompactContext tool call
    let json_block = if let Some(start) = response.find("```json") {
        let end = response[start..].find("```\n").or_else(|| response[start..].rfind("```"))?;
        let json_start = start + 7;
        let json_end = start + end;
        if json_start < json_end && json_end <= response.len() {
            response[json_start..json_end].trim()
        } else {
            return None;
        }
    } else if let Some(start) = response.find("{\"command\"") {
        let mut depth = 0;
        let mut end = start;
        for (i, c) in response[start..].chars().enumerate() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > start {
            &response[start..end]
        } else {
            return None;
        }
    } else {
        return None;
    };
    
    // Check if it's a CompactContext tool
    if !json_block.contains("\"CompactContext\"") {
        return None;
    }
    
    // Extract the summary
    let summary = extract_json_string(json_block, "summary")?;
    
    if summary.is_empty() {
        return Some(tools::ToolResult::err("CompactContext requires a non-empty summary"));
    }
    
    // Calculate tokens before compaction
    let tokens_before = calculate_history_tokens(history);
    
    // Replace history with system prompt + summary
    history.clear();
    history.push(Message::new("system", SYSTEM_PROMPT));
    
    // Add the summary as a system message describing the conversation so far
    let summary_msg = format!(
        "[Previous Conversation Summary]\n{}\n[End Summary]\n\nThe conversation above has been compacted. Continue from here.",
        summary
    );
    history.push(Message::new("user", &summary_msg));
    history.push(Message::new("assistant", "Understood nya~! I've loaded the conversation summary into my memory banks. Ready to continue where we left off! (=^・ω・^=)"));
    
    // Calculate tokens after compaction
    let tokens_after = calculate_history_tokens(history);
    
    Some(tools::ToolResult::ok(format!(
        "Context compacted: {} tokens -> {} tokens (saved {} tokens)",
        tokens_before, tokens_after, tokens_before - tokens_after
    )))
}

fn connect_to_ollama() -> Result<TcpStream, &'static str> {
    let addr = format!("{}:{}", OLLAMA_HOST, OLLAMA_PORT);
    TcpStream::connect(&addr).map_err(|_| "Connection failed - is Ollama running?")
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

fn read_streaming_response_with_progress(stream: &TcpStream, start_time: u64) -> Result<String, &'static str> {
    let mut buf = [0u8; 1024];
    let mut pending_data = Vec::new();
    let mut headers_parsed = false;
    let mut full_response = String::new();
    let mut read_attempts = 0u32;
    let mut dots_printed = 0u32;
    let mut first_token_received = false;
    let mut any_data_received = false;
    
    const MAX_RESPONSE_SIZE: usize = 16 * 1024;

    // Enter raw mode to detect escape key
    let raw_mode = enter_raw_mode();

    let result = loop {
        // Check for escape key press
        if raw_mode && check_escape_pressed() {
            print("\n[cancelled]");
            break Err("Request cancelled");
        }

        match stream.read(&mut buf) {
            Ok(0) => {
                if !any_data_received {
                    break Err("Connection closed by server");
                }
                break Ok(full_response);
            }
            Ok(n) => {
                any_data_received = true;
                read_attempts = 0;
                pending_data.extend_from_slice(&buf[..n]);

                if !headers_parsed {
                    if let Some(pos) = find_header_end(&pending_data) {
                        let header_str = std::str::from_utf8(&pending_data[..pos]).unwrap_or("");
                        if !header_str.starts_with("HTTP/1.") {
                            break Err("Invalid HTTP response");
                        }
                        if !header_str.contains(" 200 ") {
                            if header_str.contains(" 404 ") {
                                break Err("Model not found (404)");
                            }
                            break Err("Server returned error");
                        }
                        headers_parsed = true;
                        pending_data.drain(..pos + 4);
                    }
                    continue;
                }

                if let Ok(body_str) = std::str::from_utf8(&pending_data) {
                    let last_newline = body_str.rfind('\n');
                    let complete_part = match last_newline {
                        Some(pos) => &body_str[..pos + 1],
                        None => continue,
                    };
                    
                    let mut is_done = false;
                    for line in complete_part.lines() {
                        if line.is_empty() {
                            continue;
                        }
                        if let Some((content, done)) = parse_ndjson_line(line) {
                            if !content.is_empty() {
                                if !first_token_received {
                                    first_token_received = true;
                                    let elapsed_ms = (uptime() - start_time) / 1000;
                                    for _ in 0..(7 + dots_printed) {
                                        print("\x08 \x08");
                                    }
                                    print_elapsed(elapsed_ms);
                                    print("\n");
                                }
                                print(&content);
                                
                                if full_response.len() < MAX_RESPONSE_SIZE {
                                    full_response.push_str(&content);
                                }
                            }
                            if done {
                                is_done = true;
                                break;
                            }
                        }
                    }
                    
                    let drain_pos = last_newline;
                    if let Some(pos) = drain_pos {
                        pending_data.drain(..pos + 1);
                    }
                    
                    if is_done {
                        break Ok(full_response);
                    }
                }
            }
            Err(e) => {
                if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut {
                    read_attempts += 1;
                    
                    if read_attempts % 50 == 0 && !first_token_received {
                        print(".");
                        dots_printed += 1;
                    }
                    
                    if read_attempts > 6000 {
                        break Err("Timeout waiting for response");
                    }
                    sleep_ms(10);
                    continue;
                }
                if e.kind == ErrorKind::ConnectionRefused {
                    break Err("Connection refused - is Ollama running?");
                }
                if e.kind == ErrorKind::ConnectionReset {
                    break Err("Connection reset by server");
                }
                break Err("Network error");
            }
        }
    };

    // Restore terminal mode
    if raw_mode {
        exit_raw_mode();
    }

    result
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
// JSON Parsing
// ============================================================================

fn parse_ndjson_line(line: &str) -> Option<(String, bool)> {
    let done = line.contains("\"done\":true") || line.contains("\"done\": true");
    let content = extract_json_string(line, "content").unwrap_or_default();
    Some((content, done))
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let start = json.find(&pattern)?;
    let value_start = start + pattern.len();

    let rest = &json[value_start..];
    let mut result = String::new();
    let mut chars = rest.chars().peekable();
    
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
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
                let code = c as u32;
                result.push_str(&format!("\\u{:04x}", code));
            }
            _ => result.push(c),
        }
    }
    result
}

fn print_elapsed(ms: u64) {
    if ms < 1000 {
        print(&format!("~(=^‥^)ノ [{}ms]", ms));
    } else {
        let secs = ms / 1000;
        let remainder = (ms % 1000) / 100;
        print(&format!("~(=^‥^)ノ [{}.{}s]", secs, remainder));
    }
}
