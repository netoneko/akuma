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

use libakuma::{arg, argc, exit, print};

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
        "commit" => cmd_commit(),
        "checkout" => cmd_checkout(),
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

fn print_usage() {
    print("scratch - Minimal Git client for Akuma OS\n\n");
    print("Usage:\n");
    print("  scratch clone <url>          Clone a repository\n");
    print("  scratch fetch                Fetch updates from remote\n");
    print("  scratch commit -m <msg>      Commit all changes\n");
    print("  scratch checkout <branch>    Switch to a branch\n");
    print("  scratch branch               List branches\n");
    print("  scratch branch <name>        Create a branch\n");
    print("  scratch branch -d <name>     Delete a branch\n");
    print("  scratch tag                  List tags\n");
    print("  scratch tag <name>           Create a tag\n");
    print("  scratch tag -d <name>        Delete a tag\n");
    print("  scratch status               Show current HEAD\n");
    print("  scratch push                 Push to remote\n");
    print("  scratch push --token <tok>   Push with auth token\n");
    print("  scratch help                 Show this help\n");
    print("\n");
    print("NOTE: Force push is permanently disabled for safety.\n");
}

// ============================================================================
// Commands
// ============================================================================

fn cmd_clone() -> i32 {
    let url = match arg(2) {
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
    match arg(2) {
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
            let name = match arg(3) {
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
    match arg(2) {
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
            let name = match arg(3) {
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
    let mut i = 2;
    
    while i < argc() {
        match arg(i) {
            Some("-m") => {
                i += 1;
                message = arg(i);
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
    let target = match arg(2) {
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

fn cmd_push() -> i32 {
    // SAFETY: Force push is permanently disabled
    // Check all arguments for force push indicators
    let mut i = 2;
    let mut token: Option<&str> = None;
    
    while i < argc() {
        if let Some(arg_str) = arg(i) {
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
                token = arg(i);
            }
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
