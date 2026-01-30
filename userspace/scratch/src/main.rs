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
    print("  scratch branch               List branches\n");
    print("  scratch branch <name>        Create a branch\n");
    print("  scratch branch -d <name>     Delete a branch\n");
    print("  scratch tag                  List tags\n");
    print("  scratch tag <name>           Create a tag\n");
    print("  scratch tag -d <name>        Delete a tag\n");
    print("  scratch status               Show current HEAD\n");
    print("  scratch push                 Push to remote (NOT YET IMPLEMENTED)\n");
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

fn cmd_push() -> i32 {
    // SAFETY: Force push is permanently disabled
    // Check all arguments for force push indicators
    let mut i = 2;
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
        }
        i += 1;
    }

    // Normal push (not yet implemented)
    print("scratch: push is not yet implemented\n");
    print("scratch: (force push will never be implemented - it is permanently disabled)\n");
    1
}
