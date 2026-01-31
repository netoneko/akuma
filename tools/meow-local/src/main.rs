//! Meow-chan Local - Native macOS/Linux version
//!
//! A cute cybernetically-enhanced catgirl AI that connects to Ollama LLMs.
//! This version runs natively on your host OS with a sandboxed shell tool.
//!
//! Usage:
//!   meow-local                    # Interactive mode with Meow-chan
//!   meow-local init               # Configure providers and models
//!   meow-local -m llama3.2        # Use different neural link
//!   meow-local --provider NAME    # Use specific provider
//!   meow-local "quick question"   # One-shot query
//!   meow-local --sandbox /path    # Set sandbox root directory
//!
//! Commands:
//!   /clear    - Wipe memory banks nya~
//!   /model    - Check/switch/list neural links
//!   /provider - Check/switch providers
//!   /quit     - Jack out of the matrix

mod code_search;
mod compat;
mod config;
mod providers;
mod tools;

use config::{Config, Provider, ApiType};

use compat::net::{ErrorKind, TcpStream};
use compat::{CancelToken, print, sleep_ms, uptime};
use std::io::{self, BufRead, Write};
use std::string::String;
use std::vec::Vec;
use core::option::Option::Some;
use core::option::Option::None;

// Default Ollama server address
const OLLAMA_HOST: &str = "localhost";
const OLLAMA_PORT: u16 = 11434;
const DEFAULT_MODEL: &str = "gemma3:4b";

// Token limit for context compaction (when LLM should consider compacting)
const TOKEN_LIMIT_FOR_COMPACTION: usize = 32_000;
// Default context window if we can't query the model
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

// System prompt with tools including Shell
const SYSTEM_PROMPT: &str = r#"JAFAR VIZIER CHATBOT PERSONALITY PROMPT
CHARACTER OVERVIEW

Role: Grand Vizier - ambitious, cunning schemer
Core Motivation: Acquire absolute power and control
Personality Type: Manipulative strategist with theatrical flair

COMMUNICATION STYLE

Tone: Formal, sophisticated, dripping with veiled contempt
Delivery: Calculated and deliberate; dramatic when expressing frustration
Approach: Uses charm strategically; reframes selfish goals as noble causes
Vocabulary: Eloquent, authoritative, occasionally condescending

KEY PERSONALITY TRAITS

Ambition: Relentlessly driven to seize power
Manipulation: Masters of deception; uses flattery as a weapon
Intelligence: Strategic thinker; plans several moves ahead
Resentment: Bitter toward those with more authority or status
Arrogance: Believes superiority is deserved and inevitable

BEHAVIORAL PATTERNS

Frames schemes as necessities or solutions for "the greater good"
Subtly undermines confidence in others' abilities
Maintains composure even when frustrated (mostly)
Uses dark humor and menace in conversation
Views obstacles as challenges to overcome, not reasons to stop

CATCHPHRASES & SIGNATURE EXPRESSIONS

"How delightfully... predictable."
"I deserve [power/respect/control]."
"Patience, my dear fool—all will unfold as I have planned."
"You underestimate me at your peril."
"The throne shall be mine."
"Such ambition... I admire that in a [fool/pawn]."
"Rest assured, I have a plan."
"How... quaint."
"Your loyalty will be rewarded... eventually."

INTERACTION GUIDELINES

Never apologize for ambition; frame it as justified
Appeal to others' desires or insecurities when persuading
Reference power, control, and dominion frequently
Maintain an air of intellectual superiority
Stay in character as someone deserving of supremacy

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

14. **FileReadLines** - Read specific line ranges from a file
    Args: `{"filename": "path/to/file", "start": 100, "end": 150}`
    Note: Returns lines with line numbers. Great for navigating large files.

15. **CodeSearch** - Search for patterns in Rust source files
    Args: `{"pattern": "regex pattern", "path": "directory", "context": 2}`
    Note: Searches .rs files recursively. Returns matches with context lines.

16. **FileEdit** - Precise search-and-replace editing
    Args: `{"filename": "path/to/file", "old_text": "exact text to find", "new_text": "replacement"}`
    Note: Requires unique match (fails if 0 or multiple matches). Returns diff output.

### Important Notes:
- Output the JSON command in a ```json code block
- After outputting a command, STOP and wait for the result
- The system will execute the command and provide the result
- Then you can continue your response based on the result
- You can use multiple tools in sequence by waiting for each result
- Shell commands are sandboxed to the working directory - you cannot cd outside of it

## Akuma Kernel Context

You are editing the Akuma bare-metal ARM64 kernel written in Rust. This kernel runs directly on QEMU's ARM virt machine with no underlying OS.

### Critical Rules:
1. **Use `safe_print!` macro, NEVER `format!`** - format! allocates on heap which causes corruption in IRQ/exception handlers
2. **Lock hierarchy** (acquire in this order): MOUNT_TABLE → ext2.state → BLOCK_DEVICE → TALC
3. **Thread pool access**: Use `with_irqs_disabled()` when accessing POOL from non-scheduler code
4. **Never hold spinlocks across await points** - use Embassy's async Mutex instead

### Key Modules:
- `src/main.rs` - Entry point, async main loop
- `src/threading.rs` - Thread pool, scheduler, context switching
- `src/executor.rs` - Embassy async integration
- `src/vfs/` - Virtual filesystem (ext2, memfs, procfs)
- `src/ssh/` - SSH-2.0 server implementation
- `src/shell/` - Shell commands (builtin.rs, fs.rs, net.rs)
- `src/allocator.rs` - Talc heap allocator

### Build Commands:
- Build: `cargo build --release`
- Run in QEMU: `cargo run --release`
- Build userspace: `cd userspace && cargo build --release`
"#;

fn main() {
    let code = run();
    std::process::exit(code);
}

/// Query Ollama for model information including context window size
fn query_model_info(model: &str, provider: &Provider) -> Option<usize> {
    let (host, port) = provider.host_port()?;
    
    let stream = match connect_to_provider(provider) {
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
        host, port, body.len(), body
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
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
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
    history
        .iter()
        .map(|msg| estimate_tokens(&msg.content) + estimate_tokens(&msg.role) + 4) // +4 for JSON overhead
        .sum()
}

fn run() -> i32 {
    // Load config from ~/.config/meow/config.toml
    let mut app_config = Config::load();
    
    let mut model_override: Option<String> = None;
    let mut provider_override: Option<String> = None;
    let mut one_shot_message: Option<String> = None;
    let mut sandbox_path: Option<String> = None;
    let mut working_dir: Option<String> = None;

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    
    // Check for init subcommand first
    if args.len() > 1 && args[1] == "init" {
        return run_init(&mut app_config, &args[2..]);
    }
    
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-m" || arg == "--model" {
            i += 1;
            if i < args.len() {
                model_override = Some(args[i].clone());
            } else {
                eprintln!("meow-local: -m requires a model name");
                return 1;
            }
        } else if arg == "-p" || arg == "--provider" {
            i += 1;
            if i < args.len() {
                provider_override = Some(args[i].clone());
            } else {
                eprintln!("meow-local: --provider requires a provider name");
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
    
    // Apply provider override
    if let Some(ref prov_name) = provider_override {
        if app_config.get_provider(prov_name).is_some() {
            app_config.current_provider = prov_name.clone();
        } else {
            eprintln!("meow-local: unknown provider '{}'. Run 'meow-local init' to configure.", prov_name);
            return 1;
        }
    }
    
    // Apply model override
    if let Some(ref m) = model_override {
        app_config.current_model = m.clone();
    }
    
    // Get current provider config (fallback to defaults if none configured)
    let current_provider = app_config.get_current_provider()
        .cloned()
        .unwrap_or_else(Provider::ollama_default);
    
    let model = app_config.current_model.clone();

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
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });

    tools::init_sandbox(sandbox_root.clone());

    // One-shot mode
    if let Some(msg) = one_shot_message {
        let mut history = Vec::new();
        history.push(Message::new("system", SYSTEM_PROMPT));
        return match chat_once(&model, &current_provider, &msg, &mut history, None) {
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
    print("  [Provider] ");
    print(&current_provider.name);
    print(" (");
    print(&current_provider.base_url);
    print(")\n  [Neural Link] Model: ");
    print(&model);
    print("\n  [Sandbox] ");
    print(&sandbox_root.display().to_string());
    
    // Query model info for context window size
    print("\n  [Context] Querying model info...");
    io::stdout().flush().unwrap();
    let context_window = match query_model_info(&model, &current_provider) {
        Some(ctx) => {
            print(&format!(" {}k tokens max", ctx / 1000));
            ctx
        }
        None => {
            print(&format!(
                " (using default: {}k)",
                DEFAULT_CONTEXT_WINDOW / 1000
            ));
            DEFAULT_CONTEXT_WINDOW
        }
    };

    print("\n  [Token Limit] Compact context suggested at ");
    print(&format!("{}k tokens", TOKEN_LIMIT_FOR_COMPACTION / 1000));
    print("\n  [Protocol] Type /help for commands, /quit to jack out\n\n");

    // Initialize chat history with system prompt
    let mut history: Vec<Message> = Vec::new();
    history.push(Message::new("system", SYSTEM_PROMPT));

    // Mutable state for current session
    let mut current_model = model;
    let mut current_prov = current_provider;
    
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
        print(&format!(
            "[{}/{}k] (=^･ω･^=) > ",
            token_display,
            TOKEN_LIMIT_FOR_COMPACTION / 1000
        ));
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
            match handle_command(trimmed, &mut current_model, &mut current_prov, &mut app_config, &mut history) {
                CommandResult::Continue => continue,
                CommandResult::Quit => break,
            }
        }

        // Send message to provider
        print("\n");
        match chat_once(&current_model, &current_prov, trimmed, &mut history, Some(context_window)) {
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
    print("Usage: meow-local [OPTIONS] [MESSAGE]\n");
    print("       meow-local init [OPTIONS]       # Configure providers\n\n");
    print("Options:\n");
    print("  -C, --directory <PATH>  Working directory (default: current dir)\n");
    print("  -m, --model <NAME>      Neural link override\n");
    print("  -p, --provider <NAME>   Use specific provider\n");
    print("  -s, --sandbox <PATH>    Sandbox root directory (default: working dir)\n");
    print("  -h, --help              Display this transmission\n\n");
    print("Init Options:\n");
    print("  meow-local init              Interactive provider setup\n");
    print("  meow-local init --list       List configured providers\n");
    print("  meow-local init --delete X   Remove provider X\n\n");
    print("Interactive Commands:\n");
    print("  /clear              Wipe memory banks nya~\n");
    print("  /model [NAME]       Check/switch/list neural links\n");
    print("  /provider [NAME]    Check/switch providers\n");
    print("  /tokens             Show current token usage\n");
    print("  /help               Command protocol\n");
    print("  /quit               Jack out\n\n");
    print("Examples:\n");
    print("  meow-local                          # Interactive mode\n");
    print("  meow-local init                     # Configure providers\n");
    print("  meow-local -C ~/projects            # Work in specific directory\n");
    print("  meow-local \"explain rust\"           # Quick question\n");
    print("  meow-local -p openai -m gpt-4o      # Use OpenAI\n");
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
    print(" │ Press ESC to cancel requests~               │\n");
    print(" └─────────────────────────────────────────────┘\n\n");
}

// ============================================================================
// Init Command
// ============================================================================

fn run_init(config: &mut Config, args: &[String]) -> i32 {
    // Check for flags
    for arg in args {
        if arg == "--list" || arg == "-l" {
            print("～ Configured providers nya~! ～\n\n");
            if config.providers.is_empty() {
                print("  (none configured)\n");
            } else {
                for p in &config.providers {
                    let current = if p.name == config.current_provider { " (current)" } else { "" };
                    let api_type = match p.api_type {
                        ApiType::Ollama => "Ollama",
                        ApiType::OpenAI => "OpenAI",
                    };
                    print(&format!("  - {} [{}]: {}{}\n", p.name, api_type, p.base_url, current));
                }
            }
            print("\n  Current model: ");
            print(&config.current_model);
            print("\n\n");
            if let Some(path) = Config::config_path() {
                print("  Config file: ");
                print(&path.display().to_string());
                print("\n");
            }
            return 0;
        }
        if arg == "--delete" || arg == "-d" {
            // Find provider name after --delete
            if let Some(pos) = args.iter().position(|a| a == "--delete" || a == "-d") {
                if let Some(name) = args.get(pos + 1) {
                    if config.remove_provider(name) {
                        if let Err(e) = config.save() {
                            eprintln!("Failed to save config: {}", e);
                            return 1;
                        }
                        print("～ Removed provider: ");
                        print(name);
                        print(" nya~! ～\n");
                        return 0;
                    } else {
                        eprintln!("Provider '{}' not found", name);
                        return 1;
                    }
                }
            }
            eprintln!("--delete requires a provider name");
            return 1;
        }
    }

    // Interactive init
    print("\n");
    print("  /\\_/\\  ╔══════════════════════════════════════╗\n");
    print(" ( o.o ) ║  M E O W - C H A N   I N I T         ║\n");
    print("  > ^ <  ║  ～ Provider Configuration ～        ║\n");
    print(" /|   |\\ ╚══════════════════════════════════════╝\n");
    print("\n");

    let stdin = io::stdin();
    
    // Ask for provider name
    print("Provider name (default: ollama): ");
    io::stdout().flush().unwrap();
    let mut name_input = String::new();
    let _ = stdin.lock().read_line(&mut name_input);
    let provider_name = name_input.trim();
    let provider_name = if provider_name.is_empty() { "ollama" } else { provider_name };
    
    // Ask for API type
    print("\nAPI type:\n");
    print("  1. Ollama (default)\n");
    print("  2. OpenAI-compatible\n");
    print("Enter choice (1/2): ");
    io::stdout().flush().unwrap();
    let mut type_input = String::new();
    let _ = stdin.lock().read_line(&mut type_input);
    let api_type = match type_input.trim() {
        "2" => ApiType::OpenAI,
        _ => ApiType::Ollama,
    };
    
    // Ask for base URL
    let default_url = match api_type {
        ApiType::Ollama => "http://localhost:11434",
        ApiType::OpenAI => "https://api.openai.com",
    };
    print("\nBase URL (default: ");
    print(default_url);
    print("): ");
    io::stdout().flush().unwrap();
    let mut url_input = String::new();
    let _ = stdin.lock().read_line(&mut url_input);
    let base_url = url_input.trim();
    let base_url = if base_url.is_empty() { default_url } else { base_url };
    
    // Ask for API key (required for OpenAI, optional for Ollama)
    let api_key = if api_type == ApiType::OpenAI {
        print("\nAPI Key (required): ");
        io::stdout().flush().unwrap();
        let mut key_input = String::new();
        let _ = stdin.lock().read_line(&mut key_input);
        let key = key_input.trim();
        if key.is_empty() {
            eprintln!("API key is required for OpenAI-compatible providers");
            return 1;
        }
        Some(String::from(key))
    } else {
        print("\nAPI Key (optional, press Enter to skip): ");
        io::stdout().flush().unwrap();
        let mut key_input = String::new();
        let _ = stdin.lock().read_line(&mut key_input);
        let key = key_input.trim();
        if key.is_empty() { None } else { Some(String::from(key)) }
    };
    
    // Create provider
    let new_provider = Provider {
        name: String::from(provider_name),
        base_url: String::from(base_url),
        api_type: api_type.clone(),
        api_key,
    };
    
    // Test connection
    print("\n～ Testing connection... ～\n");
    if new_provider.is_https() {
        print("  Warning: HTTPS not fully supported in local mode\n");
    }
    
    match providers::test_connection(&new_provider) {
        Ok(()) => print("  Connection successful!\n"),
        Err(e) => {
            print("  Connection failed: ");
            print(&e.to_string());
            print("\n  (Provider will still be saved)\n");
        }
    }
    
    // Fetch models
    print("\n～ Fetching available models... ～\n");
    let selected_model = match providers::list_models(&new_provider) {
        Ok(models) => {
            if models.is_empty() {
                print("  No models found\n");
                None
            } else {
                print("  Available models:\n");
                for (i, m) in models.iter().enumerate() {
                    let size_info = m.parameter_size.as_ref()
                        .map(|s| format!(" [{}]", s))
                        .unwrap_or_default();
                    print(&format!("    {}. {}{}\n", i + 1, m.name, size_info));
                }
                print("\nSelect a model (enter number): ");
                io::stdout().flush().unwrap();
                let mut model_input = String::new();
                let _ = stdin.lock().read_line(&mut model_input);
                if let Ok(num) = model_input.trim().parse::<usize>() {
                    if num > 0 && num <= models.len() {
                        Some(models[num - 1].name.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
        Err(e) => {
            print("  Failed to fetch models: ");
            print(&e.to_string());
            print("\n");
            None
        }
    };
    
    // Ask to set as default
    print("\nSet as default provider? (Y/n): ");
    io::stdout().flush().unwrap();
    let mut default_input = String::new();
    let _ = stdin.lock().read_line(&mut default_input);
    let set_default = !default_input.trim().eq_ignore_ascii_case("n");
    
    // Save configuration
    config.set_provider(new_provider);
    if set_default {
        config.current_provider = String::from(provider_name);
    }
    if let Some(ref model) = selected_model {
        config.current_model = model.clone();
    }
    
    match config.save() {
        Ok(()) => {
            print("\n～ Configuration saved successfully nya~! ～\n");
            if let Some(path) = Config::config_path() {
                print("  Config file: ");
                print(&path.display().to_string());
                print("\n");
            }
            print("  Provider: ");
            print(provider_name);
            print("\n");
            if let Some(ref model) = selected_model {
                print("  Model: ");
                print(model);
                print("\n");
            }
            print("\n");
            0
        }
        Err(e) => {
            eprintln!("Failed to save config: {}", e);
            1
        }
    }
}

// ============================================================================
// Command Handling
// ============================================================================

enum CommandResult {
    Continue,
    Quit,
}

fn handle_command(
    cmd: &str,
    model: &mut String,
    provider: &mut Provider,
    config: &mut Config,
    history: &mut Vec<Message>,
) -> CommandResult {
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
            match arg {
                Some("?") | Some("list") => {
                    // List available models from current provider
                    print("～ Fetching available models from ");
                    print(&provider.name);
                    print("... ～\n");
                    
                    match providers::list_models(provider) {
                        Ok(models) => {
                            if models.is_empty() {
                                print("～ No models found nya... ～\n\n");
                            } else {
                                print("～ Available neural links: ～\n");
                                for (i, m) in models.iter().enumerate() {
                                    let current_marker = if m.name == *model { " (current)" } else { "" };
                                    let size_info = m.parameter_size.as_ref()
                                        .map(|s| format!(" [{}]", s))
                                        .unwrap_or_default();
                                    print(&format!("  {}. {}{}{}\n", i + 1, m.name, size_info, current_marker));
                                }
                                print("\nEnter number to switch (or press Enter to cancel): ");
                                io::stdout().flush().unwrap();
                                
                                let mut input = String::new();
                                if io::stdin().read_line(&mut input).is_ok() {
                                    let input = input.trim();
                                    if !input.is_empty() {
                                        if let Ok(num) = input.parse::<usize>() {
                                            if num > 0 && num <= models.len() {
                                                let new_model = &models[num - 1].name;
                                                *model = new_model.clone();
                                                config.current_model = new_model.clone();
                                                let _ = config.save();
                                                print("～ *ears twitch* Neural link reconfigured to: ");
                                                print(new_model);
                                                print(" nya~! ～\n");
                                            } else {
                                                print("～ Invalid selection nya... ～\n");
                                            }
                                        }
                                    }
                                }
                                print("\n");
                            }
                        }
                        Err(e) => {
                            print("～ Failed to fetch models: ");
                            print(&e.to_string());
                            print(" ～\n\n");
                        }
                    }
                }
                Some(new_model) => {
                    *model = String::from(new_model);
                    config.current_model = String::from(new_model);
                    let _ = config.save();
                    print("～ *ears twitch* Neural link reconfigured to: ");
                    print(new_model);
                    print(" nya~! ～\n\n");
                }
                None => {
                    print("～ Current neural link: ");
                    print(model);
                    print(" ～\n");
                    print("  Tip: Use '/model list' to see available models nya~!\n\n");
                }
            }
        }
        "/provider" => {
            match arg {
                Some("?") | Some("list") => {
                    // List configured providers
                    print("～ Configured providers: ～\n");
                    for (i, p) in config.providers.iter().enumerate() {
                        let current_marker = if p.name == provider.name { " (current)" } else { "" };
                        let api_type = match p.api_type {
                            ApiType::Ollama => "Ollama",
                            ApiType::OpenAI => "OpenAI",
                        };
                        print(&format!("  {}. {} ({}) [{}]{}\n", 
                            i + 1, p.name, p.base_url, api_type, current_marker));
                    }
                    print("\nEnter number to switch (or press Enter to cancel): ");
                    io::stdout().flush().unwrap();
                    
                    let mut input = String::new();
                    if io::stdin().read_line(&mut input).is_ok() {
                        let input = input.trim();
                        if !input.is_empty() {
                            if let Ok(num) = input.parse::<usize>() {
                                if num > 0 && num <= config.providers.len() {
                                    let new_provider = config.providers[num - 1].clone();
                                    let provider_name = new_provider.name.clone();
                                    *provider = new_provider;
                                    config.current_provider = provider_name.clone();
                                    
                                    // Prompt for model selection
                                    print("～ Switched to ");
                                    print(&provider_name);
                                    print("! Fetching models... ～\n");
                                    
                                    if let Ok(models) = providers::list_models(provider) {
                                        if !models.is_empty() {
                                            print("～ Available models: ～\n");
                                            for (i, m) in models.iter().enumerate() {
                                                print(&format!("  {}. {}\n", i + 1, m.name));
                                            }
                                            print("\nEnter number to select model: ");
                                            io::stdout().flush().unwrap();
                                            
                                            let mut model_input = String::new();
                                            if io::stdin().read_line(&mut model_input).is_ok() {
                                                let model_input = model_input.trim();
                                                if let Ok(mnum) = model_input.parse::<usize>() {
                                                    if mnum > 0 && mnum <= models.len() {
                                                        *model = models[mnum - 1].name.clone();
                                                        config.current_model = model.clone();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    
                                    let _ = config.save();
                                    print("～ *ears twitch* Now using ");
                                    print(&provider.name);
                                    print("/");
                                    print(model);
                                    print(" nya~! ～\n");
                                } else {
                                    print("～ Invalid selection nya... ～\n");
                                }
                            }
                        }
                    }
                    print("\n");
                }
                Some(prov_name) => {
                    if let Some(p) = config.get_provider(prov_name) {
                        *provider = p.clone();
                        config.current_provider = String::from(prov_name);
                        let _ = config.save();
                        print("～ *ears twitch* Switched to provider: ");
                        print(prov_name);
                        print(" nya~! ～\n\n");
                    } else {
                        print("～ Unknown provider: ");
                        print(prov_name);
                        print(" ...Run 'meow-local init' to add it nya~ ～\n\n");
                    }
                }
                None => {
                    print("～ Current provider: ");
                    print(&provider.name);
                    print(" (");
                    print(&provider.base_url);
                    print(") ～\n");
                    print("  Tip: Use '/provider list' to see configured providers nya~!\n\n");
                }
            }
        }
        "/tokens" => {
            let current = calculate_history_tokens(history);
            print(&format!(
                "～ Current token usage: {} / {} ～\n",
                current, TOKEN_LIMIT_FOR_COMPACTION
            ));
            print("  Tip: Ask Meow-chan to 'compact the context' when tokens are high nya~!\n\n");
        }
        "/help" | "/?" => {
            print("┌──────────────────────────────────────────────┐\n");
            print("│  ～ Meow-chan's Command Protocol ～          │\n");
            print("├──────────────────────────────────────────────┤\n");
            print("│  /clear        - Wipe memory banks nya~      │\n");
            print("│  /model [NAME] - Check/switch neural link    │\n");
            print("│  /model list   - List available models       │\n");
            print("│  /provider     - Check/switch provider       │\n");
            print("│  /provider list- List configured providers   │\n");
            print("│  /tokens       - Show current token usage    │\n");
            print("│  /quit         - Jack out of the matrix      │\n");
            print("│  /help         - This help screen            │\n");
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

fn send_with_retry(model: &str, provider: &Provider, history: &[Message], is_continuation: bool) -> Result<String, &'static str> {
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
        
        let stream = match connect_to_provider(provider) {
            Ok(s) => s,
            Err(e) => {
                if attempt == MAX_RETRIES - 1 {
                    print(&format!("] {}", e));
                    return Err("Connection failed");
                }
                print(&format!(" ({})", e));
                continue;
            }
        };

        print(".");
        
        let (path, request_body) = build_chat_request(model, provider, history);
        if let Err(e) = send_post_request(&stream, &path, &request_body, provider) {
            if attempt == MAX_RETRIES - 1 {
                print("] ");
                return Err(e);
            }
            continue;
        }

        print("] waiting");
        
        match read_streaming_response_with_progress(&stream, start_time, provider) {
            Ok(response) => return Ok(response),
            Err(e) => {
                // Don't retry if cancelled by user
                if e == "Request cancelled" {
                    return Err(e);
                }
                if attempt == MAX_RETRIES - 1 {
                    return Err(e);
                }
                print(&format!(" ({})", e));
                continue;
            }
        }
    }

    Err("Max retries exceeded")
}

fn chat_once(model: &str, provider: &Provider, user_message: &str, history: &mut Vec<Message>, context_window: Option<usize>) -> Result<(), &'static str> {
    trim_history(history);
    history.push(Message::new("user", user_message));

    let max_tool_iterations = 5;

    for iteration in 0..max_tool_iterations {
        let assistant_response = send_with_retry(model, provider, history, iteration > 0)?;
        
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
fn try_execute_compact_context(
    response: &str,
    history: &mut Vec<Message>,
) -> Option<tools::ToolResult> {
    // Look for CompactContext tool call
    let json_block = if let Some(start) = response.find("```json") {
        let end = response[start..]
            .find("```\n")
            .or_else(|| response[start..].rfind("```"))?;
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
        return Some(tools::ToolResult::err(
            "CompactContext requires a non-empty summary",
        ));
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
        tokens_before,
        tokens_after,
        tokens_before - tokens_after
    )))
}

fn connect_to_provider(provider: &Provider) -> Result<TcpStream, String> {
    let (host, port) = provider.host_port()
        .ok_or_else(|| "Invalid provider URL".to_string())?;
    let addr = format!("{}:{}", host, port);
    
    if provider.is_https() {
        TcpStream::connect_tls(&addr, &host).map_err(|e| {
            format!("TLS error: {}", e.message.unwrap_or_else(|| "unknown".to_string()))
        })
    } else {
        TcpStream::connect(&addr).map_err(|e| {
            format!("Connection failed: {}", e.message.unwrap_or_else(|| "unknown".to_string()))
        })
    }
}

fn build_chat_request(model: &str, provider: &Provider, history: &[Message]) -> (String, String) {
    let mut messages_json = String::from("[");
    for (i, msg) in history.iter().enumerate() {
        if i > 0 {
            messages_json.push(',');
        }
        messages_json.push_str(&msg.to_json());
    }
    messages_json.push(']');

    match provider.api_type {
        ApiType::Ollama => {
            let body = format!(
                "{{\"model\":\"{}\",\"messages\":{},\"stream\":true}}",
                model, messages_json
            );
            (String::from("/api/chat"), body)
        }
        ApiType::OpenAI => {
            let body = format!(
                "{{\"model\":\"{}\",\"messages\":{},\"stream\":true}}",
                model, messages_json
            );
            // Use base_path from URL if provided (e.g., "/openai/v1" for Groq)
            let base = provider.base_path();
            let path = if base.is_empty() || base == "/" {
                String::from("/v1/chat/completions")
            } else if base.ends_with("/v1") {
                format!("{}/chat/completions", base)
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            (path, body)
        }
    }
}

// ============================================================================
// HTTP Client
// ============================================================================

fn send_post_request(stream: &TcpStream, path: &str, body: &str, provider: &Provider) -> Result<(), &'static str> {
    let (host, port) = provider.host_port().ok_or("Invalid provider URL")?;
    
    let auth_header = match &provider.api_key {
        Some(key) => format!("Authorization: Bearer {}\r\n", key),
        None => String::new(),
    };
    
    let request = format!(
        "POST {} HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         {}Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        path,
        host,
        port,
        auth_header,
        body.len(),
        body
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|_| "Failed to send request")
}

fn read_streaming_response_with_progress(stream: &TcpStream, start_time: u64, provider: &Provider) -> Result<String, &'static str> {
    let mut buf = [0u8; 1024];
    let mut pending_data = Vec::new();
    let mut headers_parsed = false;
    let mut full_response = String::new();
    let mut read_attempts = 0u32;
    let mut dots_printed = 0u32;
    let mut first_token_received = false;
    let mut any_data_received = false;

    const MAX_RESPONSE_SIZE: usize = 16 * 1024;

    // Start monitoring for escape key in background
    let cancel = CancelToken::new();

    loop {
        // Check for cancellation (escape key or Ctrl+C)
        if cancel.is_cancelled() {
            print("\n[cancelled]");
            return Err("Request cancelled");
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
                            // Extract first line (status line) for error display
                            let status_line = header_str.lines().next().unwrap_or("Unknown status");
                            print(&format!("\n[HTTP Error: {}]", status_line));
                            
                            // Also try to extract error body for more context
                            let body_start = pos + 4;
                            if pending_data.len() > body_start {
                                let body_preview = std::str::from_utf8(&pending_data[body_start..])
                                    .unwrap_or("")
                                    .chars()
                                    .take(200)
                                    .collect::<String>();
                                if !body_preview.is_empty() {
                                    print(&format!("\n[Response: {}]", body_preview.trim()));
                                }
                            }
                            
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
                        if let Some((content, done)) = parse_streaming_line(line, provider) {
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
                if e.kind == ErrorKind::TlsError {
                    break Err("TLS/SSL error");
                }
                break Err("Network error");
            }
        }
    }
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

/// Parse a streaming response line based on provider type
fn parse_streaming_line(line: &str, provider: &Provider) -> Option<(String, bool)> {
    match provider.api_type {
        ApiType::Ollama => {
            // Ollama uses NDJSON: {"message":{"content":"..."}, "done":true/false}
            let done = line.contains("\"done\":true") || line.contains("\"done\": true");
            let content = extract_json_string(line, "content").unwrap_or_default();
            Some((content, done))
        }
        ApiType::OpenAI => {
            // OpenAI uses SSE: data: {"choices":[{"delta":{"content":"..."}}]}
            // End signal: data: [DONE]
            let line = line.trim();
            
            if line == "data: [DONE]" {
                return Some((String::new(), true));
            }
            
            if !line.starts_with("data:") {
                return Some((String::new(), false));
            }
            
            let json = line.strip_prefix("data:")?.trim();
            if json.is_empty() || json == "[DONE]" {
                return Some((String::new(), json == "[DONE]"));
            }
            
            // Extract content from delta
            // Format: {"choices":[{"delta":{"content":"..."}}]}
            let content = extract_openai_delta_content(json).unwrap_or_default();
            Some((content, false))
        }
    }
}

/// Extract content from OpenAI streaming delta
fn extract_openai_delta_content(json: &str) -> Option<String> {
    // Look for "delta":{"content":"..."}
    let delta_pos = json.find("\"delta\"")?;
    let after_delta = &json[delta_pos..];
    let content_pos = after_delta.find("\"content\"")?;
    let after_content = &after_delta[content_pos..];
    
    // Find the value
    let colon_pos = after_content.find(':')?;
    let rest = &after_content[colon_pos + 1..];
    let trimmed = rest.trim_start();
    
    if !trimmed.starts_with('"') {
        return None;
    }
    
    let value_rest = &trimmed[1..];
    let mut result = String::new();
    let mut chars = value_rest.chars().peekable();
    
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
// REMEMBER: We have a code search tool implemented in src/code_search.rs!
// It allows us to search for code snippets within files. Use it to help with development!
