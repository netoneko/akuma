//! chainlink - Issue tracker CLI for Akuma
//!
//! A no_std userspace application that wraps the chainlink issue tracker library.

#![no_std]
#![no_main]

extern crate alloc;

mod backend;

use alloc::string::String;
use libakuma::{arg, argc, exit, print};

use chainlink::db::Database;
use backend::SqldBackend;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    match run() {
        Ok(_) => exit(0),
        Err(e) => {
            print("chainlink: error: ");
            print(&e);
            print("\n");
            exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    if argc() < 2 {
        print_usage();
        return Err(String::from("missing command"));
    }

    let cmd = arg(1).ok_or_else(|| String::from("missing command"))?;

    match cmd {
        "init" => cmd_init(),
        "create" => cmd_create(),
        "list" => cmd_list(),
        "show" => cmd_show(),
        "close" => cmd_close(),
        "reopen" => cmd_reopen(),
        "comment" => cmd_comment(),
        "label" => cmd_label(),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => {
            print("chainlink: unknown command: ");
            print(cmd);
            print("\n");
            print_usage();
            Err(String::from("unknown command"))
        }
    }
}

fn print_usage() {
    print("chainlink - Issue tracker CLI for Akuma\n");
    print("\n");
    print("Usage:\n");
    print("  chainlink init                      Initialize database\n");
    print("  chainlink create <title> [-d desc] [-p priority]\n");
    print("                                      Create a new issue\n");
    print("  chainlink list [-s status]          List issues (open/closed/all)\n");
    print("  chainlink show <id>                 Show issue details\n");
    print("  chainlink close <id>                Close an issue\n");
    print("  chainlink reopen <id>               Reopen an issue\n");
    print("  chainlink comment <id> <text>       Add a comment\n");
    print("  chainlink label <id> <label>        Add a label\n");
    print("  chainlink help                      Show this help\n");
}

const DB_DIR: &str = ".chainlink";
const DB_PATH: &str = ".chainlink/issues.db";

fn ensure_db_dir() {
    // Create the .chainlink directory if it doesn't exist
    let result = libakuma::mkdir(DB_DIR);
    if result < 0 && result != -17 {
        // -17 is EEXIST, which is fine
        print("chainlink: Warning: could not create ");
        print(DB_DIR);
        print(" directory\n");
    }
}

fn get_db() -> Result<Database<SqldBackend>, String> {
    // Ensure directory exists
    ensure_db_dir();
    
    // Initialize SQLite VFS
    sqld::vfs::init().map_err(|e| String::from(e))?;
    
    Database::open(DB_PATH).map_err(|e| alloc::format!("{}", e))
}

fn cmd_init() -> Result<(), String> {
    print("chainlink: Initializing database...\n");
    
    // Create the .chainlink directory
    ensure_db_dir();
    
    // Initialize SQLite VFS
    sqld::vfs::init().map_err(|e| String::from(e))?;
    
    // Open/create the database (the database layer will create the schema)
    let _db = Database::<SqldBackend>::open(DB_PATH)
        .map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Initialized in .chainlink/\n");
    Ok(())
}

fn cmd_create() -> Result<(), String> {
    if argc() < 3 {
        return Err(String::from("usage: chainlink create <title> [-d desc] [-p priority]"));
    }
    
    let title = arg(2).ok_or_else(|| String::from("missing title"))?;
    
    // Parse optional arguments
    let mut description: Option<&str> = None;
    let mut priority = "medium";
    
    let mut i = 3;
    while i < argc() as usize {
        match arg(i as u32) {
            Some("-d") => {
                i += 1;
                description = arg(i as u32);
            }
            Some("-p") => {
                i += 1;
                if let Some(p) = arg(i as u32) {
                    priority = p;
                }
            }
            _ => {}
        }
        i += 1;
    }
    
    let db = get_db()?;
    let id = db.create_issue(title, description, priority)
        .map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Created issue #");
    print_num(id as usize);
    print(": ");
    print(title);
    print("\n");
    
    Ok(())
}

fn cmd_list() -> Result<(), String> {
    // Parse optional status filter
    let mut status_filter = Some("open");
    
    let mut i = 2;
    while i < argc() as usize {
        match arg(i as u32) {
            Some("-s") => {
                i += 1;
                status_filter = arg(i as u32);
            }
            _ => {}
        }
        i += 1;
    }
    
    let db = get_db()?;
    let issues = db.list_issues(status_filter, None, None)
        .map_err(|e| alloc::format!("{}", e))?;
    
    if issues.is_empty() {
        print("No issues found.\n");
        return Ok(());
    }
    
    for issue in issues {
        print("#");
        print_num(issue.id as usize);
        print(" [");
        print(&issue.status);
        print("] ");
        print(&issue.title);
        print(" (");
        print(&issue.priority);
        print(")\n");
    }
    
    Ok(())
}

fn cmd_show() -> Result<(), String> {
    if argc() < 3 {
        return Err(String::from("usage: chainlink show <id>"));
    }
    
    let id_str = arg(2).ok_or_else(|| String::from("missing issue id"))?;
    let id: i64 = id_str.parse().map_err(|_| String::from("invalid issue id"))?;
    
    let db = get_db()?;
    let issue = db.get_issue(id)
        .map_err(|e| alloc::format!("{}", e))?
        .ok_or_else(|| String::from("issue not found"))?;
    
    print("Issue #");
    print_num(issue.id as usize);
    print("\n");
    print("Title: ");
    print(&issue.title);
    print("\n");
    print("Status: ");
    print(&issue.status);
    print("\n");
    print("Priority: ");
    print(&issue.priority);
    print("\n");
    
    if let Some(desc) = &issue.description {
        print("Description: ");
        print(desc);
        print("\n");
    }
    
    // Show labels
    let labels = db.get_labels(id).map_err(|e| alloc::format!("{}", e))?;
    if !labels.is_empty() {
        print("Labels: ");
        for (i, label) in labels.iter().enumerate() {
            if i > 0 {
                print(", ");
            }
            print(label);
        }
        print("\n");
    }
    
    // Show comments
    let comments = db.get_comments(id).map_err(|e| alloc::format!("{}", e))?;
    if !comments.is_empty() {
        print("\nComments:\n");
        for comment in comments {
            print("  - ");
            print(&comment.content);
            print("\n");
        }
    }
    
    Ok(())
}

fn cmd_close() -> Result<(), String> {
    if argc() < 3 {
        return Err(String::from("usage: chainlink close <id>"));
    }
    
    let id_str = arg(2).ok_or_else(|| String::from("missing issue id"))?;
    let id: i64 = id_str.parse().map_err(|_| String::from("invalid issue id"))?;
    
    let db = get_db()?;
    db.close_issue(id).map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Closed issue #");
    print_num(id as usize);
    print("\n");
    
    Ok(())
}

fn cmd_reopen() -> Result<(), String> {
    if argc() < 3 {
        return Err(String::from("usage: chainlink reopen <id>"));
    }
    
    let id_str = arg(2).ok_or_else(|| String::from("missing issue id"))?;
    let id: i64 = id_str.parse().map_err(|_| String::from("invalid issue id"))?;
    
    let db = get_db()?;
    db.reopen_issue(id).map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Reopened issue #");
    print_num(id as usize);
    print("\n");
    
    Ok(())
}

fn cmd_comment() -> Result<(), String> {
    if argc() < 4 {
        return Err(String::from("usage: chainlink comment <id> <text>"));
    }
    
    let id_str = arg(2).ok_or_else(|| String::from("missing issue id"))?;
    let id: i64 = id_str.parse().map_err(|_| String::from("invalid issue id"))?;
    let text = arg(3).ok_or_else(|| String::from("missing comment text"))?;
    
    let db = get_db()?;
    db.add_comment(id, text).map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Added comment to issue #");
    print_num(id as usize);
    print("\n");
    
    Ok(())
}

fn cmd_label() -> Result<(), String> {
    if argc() < 4 {
        return Err(String::from("usage: chainlink label <id> <label>"));
    }
    
    let id_str = arg(2).ok_or_else(|| String::from("missing issue id"))?;
    let id: i64 = id_str.parse().map_err(|_| String::from("invalid issue id"))?;
    let label = arg(3).ok_or_else(|| String::from("missing label"))?;
    
    let db = get_db()?;
    db.add_label(id, label).map_err(|e| alloc::format!("{}", e))?;
    
    print("chainlink: Added label '");
    print(label);
    print("' to issue #");
    print_num(id as usize);
    print("\n");
    
    Ok(())
}

fn print_num(n: usize) {
    let mut buf = [0u8; 20];
    let mut i = 19;
    let mut v = n;

    if v == 0 {
        print("0");
        return;
    }

    while v > 0 && i > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i -= 1;
    }

    if let Ok(s) = core::str::from_utf8(&buf[i + 1..]) {
        print(s);
    }
}
