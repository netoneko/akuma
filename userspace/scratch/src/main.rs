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
//!   scratch push [branch]          # Push branch to remote (force push DISABLED)

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
mod index;
mod log;

use alloc::format;
use alloc::string::String;
use libakuma::{arg, argc, exit, getcwd, print};

// ============================================================================
// Working Directory Support
// ============================================================================

/// Get the .git directory path (relative to cwd)
pub fn git_dir() -> String {
    let cwd = getcwd();
    if cwd == "/" {
        String::from("/.git")
    } else {
        format!("{}/.git", cwd)
    }
}

/// Get a path relative to the repo root (cwd)
pub fn repo_path(relative: &str) -> String {
    let cwd = getcwd();
    if relative == "." {
        String::from(cwd)
    } else if cwd == "/" {
        format!("/{}", relative)
    } else {
        format!("{}/{}", cwd, relative)
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

    let command = match arg(1) {
        Some(cmd) => cmd,
        None => {
            print("scratch: missing command\n");
            return 1;
        }
    };

    match command {
        "clone" => cmd_clone(),
        "fetch" => cmd_fetch(),
        "push" => cmd_push(),
        "add" => cmd_add(),
        "commit" => cmd_commit(),
        "checkout" => cmd_checkout(),
        "config" => cmd_config(),
        "branch" => cmd_branch(),
        "tag" => cmd_tag(),
        "status" => cmd_status(),
        "log" => cmd_log(),
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

/// Get command argument (argument index relative to command)
fn cmd_arg(n: u32) -> Option<&'static str> {
    arg(1 + n)
}

fn print_usage() {
    print("scratch - Minimal Git client for Akuma OS\n\n");
    print("Usage: scratch <command> [args]\n\n");
    print("Commands:\n");
    print("  clone <url>          Clone a repository\n");
    print("  fetch                Fetch updates from remote\n");
    print("  add <path>           Stage files for commit\n");
    print("  commit -m <msg>      Commit staged changes\n");
    print("  commit --amend       Amend the last commit\n");
    print("  log [-n N]           Show commit history\n");
    print("  checkout <branch>    Switch to a branch\n");
    print("  config <key> [val]   Get or set config value\n");
    print("  branch [name]        List or create branches\n");
    print("  branch -d <name>     Delete a branch\n");
    print("  tag [name]           List or create tags\n");
    print("  status               Show current HEAD\n");
    print("  push [--token <t>]   Push to remote\n");
    print("  help                 Show this help\n");
    print("\n");
    print("Uses current working directory (inherited from parent process).\n");
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
    // Show branch and HEAD
    match refs::read_head() {
        Ok(head) => {
            if let Ok(Some(branch)) = commit::current_branch() {
                print("On branch ");
                print(&branch);
                print("\n");
            }
            print("HEAD: ");
            print(&head);
            print("\n");
        }
        Err(e) => {
            print("scratch: ");
            print(e.message());
            print("\n");
            return 1;
        }
    }
    
    // Show staged files from index
    let git_dir = format!("{}/.git", getcwd());
    match index::Index::load(&git_dir) {
        Ok(idx) => {
            if idx.is_empty() {
                print("\nNo changes staged for commit.\n");
            } else {
                print("\nChanges staged for commit:\n");
                for entry in idx.entries() {
                    let mode_str = if entry.mode == 0o100755 {
                        "(executable)"
                    } else {
                        ""
                    };
                    print("  ");
                    print(&entry.path);
                    if !mode_str.is_empty() {
                        print(" ");
                        print(mode_str);
                    }
                    print("\n");
                }
                print(&format!("\n{} file(s) staged\n", idx.len()));
            }
        }
        Err(_) => {
            // No index or error reading it - that's fine
            print("\nNo changes staged for commit.\n");
        }
    }
    
    0
}

fn cmd_commit() -> i32 {
    // Parse arguments: scratch commit -m "message" [--amend]
    let mut message: Option<&str> = None;
    let mut amend = false;
    let mut i = 1;
    
    while let Some(a) = cmd_arg(i) {
        match a {
            "-m" => {
                i += 1;
                message = cmd_arg(i);
            }
            "--amend" => {
                amend = true;
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
            print("       scratch commit --amend -m \"new message\"\n");
            return 1;
        }
    };

    if amend {
        print("scratch: amending last commit...\n");
    } else {
        print("scratch: committing changes...\n");
    }

    match commit::create_commit(message, None, None, amend) {
        Ok(sha) => {
            if amend {
                print("scratch: amended commit ");
            } else {
                print("scratch: created commit ");
            }
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
    let mut branch: Option<&str> = None;
    
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
        } else if !arg_str.starts_with('-') && branch.is_none() {
            // First non-flag argument is the branch name
            branch = Some(arg_str);
        }
        i += 1;
    }

    print("scratch: pushing to origin\n");

    match repository::push(token, branch) {
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

fn cmd_add() -> i32 {
    let path = match cmd_arg(1) {
        Some(p) => p,
        None => {
            print("scratch: add requires a path\n");
            print("Usage: scratch add <file|dir>\n");
            print("       scratch add .     (stage all)\n");
            return 1;
        }
    };

    // Handle -A flag as alias for .
    let path = if path == "-A" { "." } else { path };

    let git_dir = git_dir();
    let store = store::ObjectStore::new(&git_dir);

    // Load existing index or create new one
    let mut idx = match index::Index::load(&git_dir) {
        Ok(idx) => idx,
        Err(e) => {
            print("scratch: failed to load index: ");
            print(e.message());
            print("\n");
            return 1;
        }
    };

    // Add path to index
    let added_count = match idx.add_path(path, &store) {
        Ok(count) => count,
        Err(e) => {
            print("scratch: add failed: ");
            print(e.message());
            print("\n");
            return 1;
        }
    };

    // Save index
    match idx.save(&git_dir) {
        Ok(()) => {
            print("scratch: staged ");
            print_num(added_count);
            print(" file(s)\n");
            0
        }
        Err(e) => {
            print("scratch: failed to save index: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn cmd_log() -> i32 {
    // Parse arguments
    let mut max_commits: Option<usize> = None;
    let mut oneline = false;
    let mut i = 1;

    while let Some(a) = cmd_arg(i) {
        match a {
            "-n" => {
                i += 1;
                if let Some(n_str) = cmd_arg(i) {
                    if let Ok(n) = n_str.parse::<usize>() {
                        max_commits = Some(n);
                    }
                }
            }
            "--oneline" => {
                oneline = true;
            }
            _ => {}
        }
        i += 1;
    }

    match log::show_log(max_commits, oneline) {
        Ok(()) => 0,
        Err(e) => {
            print("scratch: log failed: ");
            print(e.message());
            print("\n");
            1
        }
    }
}

fn print_num(n: usize) {
    if n == 0 {
        print("0");
        return;
    }

    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut num = n;

    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        if let Ok(s) = core::str::from_utf8(&s) {
            print(s);
        }
    }
}
