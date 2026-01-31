//! scratch - Minimal Git client for Akuma OS
//!
//! A lightweight Git implementation that supports cloning from GitHub
//! and basic repository operations.
//!
//! Usage:
//!   scratch clone <url>          # Clone a repository
//!   scratch fetch                # Fetch updates from remote
//!   scratch branch               # List branches
//!   scratch branch <name>        # Create a branch
//!   scratch branch -d <name>     # Delete a branch
//!   scratch tag                  # List tags
//!   scratch tag <name>           # Create a tag
//!   scratch tag -d <name>        # Delete a tag
//!   scratch status               # Show current HEAD
//!   scratch push                  # Push to remote (force push DISABLED)

#![no_std]
#![no_main]

extern crate alloc;

mod error;
mod sha1;
mod zlib;
mod object;
mod store;
mod refs;
mod pack;
mod pktline;
mod protocol;
mod http;
mod stream;
mod pack_stream;
mod repository;
mod commit;
mod base64;
mod pack_write;
mod config;

use alloc::format;
use alloc::string::String;
use libakuma::{arg, argc, exit, print};

// ============================================================================
// Global Repository Path
// ============================================================================

/// The repository root directory (set via -C option)
static mut REPO_ROOT: Option<String> = None;

/// Get the .git directory path
pub fn git_dir() -> String {
    unsafe {
        match &REPO_ROOT {
            Some(root) => format!("{}/.git", root),
            None => String::from(".git"),
        }
    }
}

/// Get a path relative to the repo root
pub fn repo_path(relative: &str) -> String {
    unsafe {
        match &REPO_ROOT {
            Some(root) => {
                if relative == "." {
                    root.clone()
                } else {
                    format!("{}/{}", root, relative)
                }
            }
            None => String::from(relative),
        }
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let code = main();
    exit(code);
}

fn main() -> i32 {
    if argc() < 2 {
        print_usage();
        return 1;
    }

    // Check for -C option first
    let mut arg_offset: u32 = 1;
    if let Some(first) = arg(1) {
        if first == "-C" {
            if let Some(path) = arg(2) {
                unsafe {
                    REPO_ROOT = Some(String::from(path));
                }
                arg_offset = 3;
            } else {
                print("scratch: -C requires a directory path\n");
                return 1;
            }
        }
    }

    if argc() <= arg_offset {
        print_usage();
        return 1;
    }

    let command = match arg(arg_offset) {
        Some(cmd) => cmd,
        None => {
            print("scratch: missing command\n");
            return 1;
        }
    };

    // Adjust arg() calls in commands by storing offset
    unsafe {
        ARG_OFFSET = arg_offset;
    }

    match command {
        "clone" => cmd_clone(),
        "fetch" => cmd_fetch(),
        "push" => cmd_push(),
        "commit" => cmd_commit(),
        "checkout" => cmd_checkout(),
        "config" => cmd_config(),
        "branch" => cmd_branch(),
        "tag" => cmd_tag(),
        "status" => cmd_status(),
        "help" | "--help" | "-h" => {
            print_usage();
            0
        }
        _ => {
            print("scratch: unknown command '");
            print(command);
            print("'\n");
            print_usage();
            1
        }
    }
}

/// Argument offset (for -C handling)
static mut ARG_OFFSET: u32 = 1;

/// Get command argument (adjusted for -C offset)
fn cmd_arg(n: u32) -> Option<&'static str> {
    unsafe { arg(ARG_OFFSET + n) }
}

fn print_usage() {
    print("scratch - Minimal Git client for Akuma OS\n\n");
    print("Usage:\n");
    print("  scratch [-C <path>] <command> [args]\n\n");
    print("Options:\n");
    print("  -C <path>                    Run as if started in <path>\n\n");
    print("Commands:\n");
    print("  clone <url>          Clone a repository\n");
    print("  fetch                Fetch updates from remote\n");
    print("  commit -m <msg>      Commit all changes\n");
    print("  checkout <branch>    Switch to a branch\n");
    print("  config <key> [val]   Get or set config value\n");
    print("  branch [name]        List or create branches\n");
    print("  branch -d <name>     Delete a branch\n");
    print("  tag [name]           List or create tags\n");
    print("  status               Show current HEAD\n");
    print("  push [--token <t>]   Push to remote\n");
    print("  help                 Show this help\n");
    print("\n");
    print("Config keys: user.name, user.email, credential.token\n");
    print("NOTE: Force push is permanently disabled for safety.\n");
}

// ============================================================================
// Commands
// ============================================================================

fn cmd_clone() -> i32 {
    let url = match cmd_arg(1) {
        Some(u) => u,
        None => {
            print("scratch: clone requires a URL\n");
            print("Usage: scratch clone <url>\n");
            return 1;
        }
    };

    print("scratch: cloning ");
    print(url);
    print("\n");

    // TODO: Implement clone
    match repository::clone(url) {
        Ok(()) => {
            print("scratch: clone complete\n");
            0
        }
        Err(e) => {
            print("scratch: clone failed: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_fetch() -> i32 {
    print("scratch: fetching from origin\n");

    match repository::fetch() {
        Ok(()) => {
            print("scratch: fetch complete\n");
            0
        }
        Err(e) => {
            print("scratch: fetch failed: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_branch() -> i32 {
    // Check for arguments
    match cmd_arg(1) {
        None => {
            // List branches
            match refs::list_branches() {
                Ok(branches) => {
                    for (name, _sha) in branches {
                        print("  ");
                        print(&name);
                        print("\n");
                    }
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
        Some("-d") => {
            // Delete branch
            let name = match cmd_arg(2) {
                Some(n) => n,
                None => {
                    print("scratch: branch -d requires a name\n");
                    return 1;
                }
            };
            match refs::delete_branch(name) {
                Ok(()) => {
                    print("Deleted branch ");
                    print(name);
                    print("\n");
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
        Some(name) => {
            // Create branch
            match refs::create_branch(name) {
                Ok(()) => {
                    print("Created branch ");
                    print(name);
                    print("\n");
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
    }
}

fn cmd_tag() -> i32 {
    match cmd_arg(1) {
        None => {
            // List tags
            match refs::list_tags() {
                Ok(tags) => {
                    for (name, _sha) in tags {
                        print("  ");
                        print(&name);
                        print("\n");
                    }
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
        Some("-d") => {
            // Delete tag
            let name = match cmd_arg(2) {
                Some(n) => n,
                None => {
                    print("scratch: tag -d requires a name\n");
                    return 1;
                }
            };
            match refs::delete_tag(name) {
                Ok(()) => {
                    print("Deleted tag ");
                    print(name);
                    print("\n");
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
        Some(name) => {
            // Create tag
            match refs::create_tag(name) {
                Ok(()) => {
                    print("Created tag ");
                    print(name);
                    print("\n");
                    0
                }
                Err(e) => {
                    print("scratch: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
    }
}

fn cmd_status() -> i32 {
    match refs::read_head() {
        Ok(head) => {
            // Show branch name if on a branch
            if let Ok(Some(branch)) = commit::current_branch() {
                print("On branch ");
                print(&branch);
                print("\n");
            }
            print("HEAD: ");
            print(&head);
            print("\n");
            0
        }
        Err(e) => {
            print("scratch: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_commit() -> i32 {
    // Parse arguments: scratch commit -m "message"
    let mut message: Option<&str> = None;
    let mut i = 1;
    
    while let Some(a) = cmd_arg(i) {
        match a {
            "-m" => {
                i += 1;
                message = cmd_arg(i);
            }
            _ => {}
        }
        i += 1;
    }

    let message = match message {
        Some(m) => m,
        None => {
            print("scratch: commit requires -m <message>\n");
            print("Usage: scratch commit -m \"commit message\"\n");
            return 1;
        }
    };

    print("scratch: committing changes...\n");

    match commit::create_commit(message, None, None) {
        Ok(sha) => {
            print("scratch: created commit ");
            print(&crate::sha1::to_hex(&sha));
            print("\n");
            0
        }
        Err(e) => {
            print("scratch: commit failed: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_checkout() -> i32 {
    let target = match cmd_arg(1) {
        Some(t) => t,
        None => {
            print("scratch: checkout requires a branch name\n");
            print("Usage: scratch checkout <branch>\n");
            return 1;
        }
    };

    print("scratch: switching to branch ");
    print(target);
    print("\n");

    match repository::checkout(target) {
        Ok(()) => {
            print("scratch: switched to branch ");
            print(target);
            print("\n");
            0
        }
        Err(e) => {
            print("scratch: checkout failed: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_config() -> i32 {
    let first_arg = match cmd_arg(1) {
        Some(a) => a,
        None => {
            print("scratch: config requires a key\n");
            print("Usage: scratch config <key> [value]\n");
            print("       scratch config set <key> <value>\n");
            print("       scratch config get <key>\n");
            print("\n");
            print("Keys:\n");
            print("  user.name         Your name for commits\n");
            print("  user.email        Your email for commits\n");
            print("  credential.token  Auth token for push\n");
            return 1;
        }
    };

    // Support both syntaxes:
    // scratch config user.name "value"
    // scratch config set user.name "value"
    // scratch config get user.name
    let (key, value) = match first_arg {
        "set" => {
            // scratch config set <key> <value>
            let k = match cmd_arg(2) {
                Some(k) => k,
                None => {
                    print("scratch: config set requires <key> <value>\n");
                    return 1;
                }
            };
            let v = cmd_arg(3);
            (k, v)
        }
        "get" => {
            // scratch config get <key>
            let k = match cmd_arg(2) {
                Some(k) => k,
                None => {
                    print("scratch: config get requires <key>\n");
                    return 1;
                }
            };
            (k, None)
        }
        key => {
            // scratch config <key> [value]
            (key, cmd_arg(2))
        }
    };

    match value {
        Some(val) => {
            // Set value
            match config::GitConfig::set(key, val) {
                Ok(()) => {
                    print("scratch: set ");
                    print(key);
                    print(" = ");
                    print(val);
                    print("\n");
                    0
                }
                Err(e) => {
                    print("scratch: config failed: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
        None => {
            // Get value
            match config::GitConfig::get(key) {
                Ok(Some(val)) => {
                    print(&val);
                    print("\n");
                    0
                }
                Ok(None) => {
                    print("scratch: ");
                    print(key);
                    print(" is not set\n");
                    1
                }
                Err(e) => {
                    print("scratch: config failed: ");
                    print(e.message());
                    print("\n");
                    1
                }
            }
        }
    }
}

fn cmd_push() -> i32 {
    // SAFETY: Force push is permanently disabled
    // Check all arguments for force push indicators
    let mut i = 1;
    let mut token: Option<&str> = None;
    
    while let Some(arg_str) = cmd_arg(i) {
        // Check for force push flags
        if arg_str == "--force" || arg_str == "-f" || arg_str == "--force-with-lease" {
            print("scratch: FATAL: force push is disabled\n");
            print("scratch: This safety measure cannot be bypassed.\n");
            return -1;
        }
        // Check for +refspec syntax (e.g., +HEAD:refs/heads/main)
        if arg_str.starts_with('+') {
            print("scratch: FATAL: force push via +refspec is disabled\n");
            print("scratch: This safety measure cannot be bypassed.\n");
            return -1;
        }
        // Check for --token
        if arg_str == "--token" {
            i += 1;
            token = cmd_arg(i);
        }
        i += 1;
    }

    print("scratch: pushing to origin\n");

    match repository::push(token) {
        Ok(()) => {
            print("scratch: push complete\n");
            0
        }
        Err(e) => {
            print("scratch: push failed: ");
            print(e.message());
            print("\n");
            if e.message().contains("authentication") {
                print("scratch: hint: use --token <your-token> for authentication\n");
            }
            1
        }
    }
}
