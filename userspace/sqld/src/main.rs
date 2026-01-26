//! sqld - SQLite daemon for Akuma
//!
//! A userspace SQLite CLI tool that provides:
//! - `sqld status <file>` - Show list of tables in a database
//! - `sqld <file>` - Start a TCP server on port 4321
//! - `sqld run [sql]` - Execute SQL via server (reads from stdin if no sql arg)

#![no_std]
#![no_main]

extern crate alloc;

mod client;
mod server;
mod vfs;

use alloc::string::String;
use libakuma::{arg, argc, exit, print, write, fd};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    match run() {
        Ok(_) => exit(0),
        Err(e) => {
            print("sqld: error: ");
            print(e);
            print("\n");
            exit(1);
        }
    }
}

fn run() -> Result<(), &'static str> {
    // arg(0) = program name, arg(1) = first argument, etc.
    if argc() < 2 {
        print_usage();
        return Err("missing arguments");
    }

    let first_arg = arg(1).ok_or("missing command")?;

    match first_arg {
        "status" => {
            // Initialize SQLite for local operations
            vfs::init()?;
            let path = arg(2).ok_or("missing file path")?;
            cmd_status(path)
        }
        "run" => {
            // Client mode - connect to server
            cmd_run()
        }
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        path => {
            // Initialize SQLite for server mode
            vfs::init()?;
            // Treat as file path for serve mode
            cmd_serve(path)
        }
    }
}

fn print_usage() {
    print("sqld - SQLite daemon for Akuma\n");
    print("\n");
    print("Usage:\n");
    print("  sqld <file>                  Start TCP server on port 4321\n");
    print("  sqld status <file>           Show tables in database\n");
    print("  sqld run [sql]               Execute SQL via server (127.0.0.1)\n");
    print("  sqld run -h host:port [sql]  Execute SQL via specific server\n");
    print("  sqld help                    Show this help\n");
    print("\n");
    print("Examples:\n");
    print("  sqld local.sqlite                          # Start server\n");
    print("  sqld run \"SELECT 1\"                        # Query localhost\n");
    print("  sqld run -h 10.0.2.15:4321 \"SELECT 1\"      # Query specific host\n");
}

#[allow(dead_code)]
fn print_num(n: u32) {
    if n == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 12];
    let mut i = 0;
    let mut num = n;
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        write(fd::STDOUT, &buf[i..i+1]);
    }
}

/// Show the status of a database file (list of tables)
fn cmd_status(path: &str) -> Result<(), &'static str> {
    // Check if file exists first
    let file_fd = libakuma::open(path, libakuma::open_flags::O_RDONLY);
    if file_fd < 0 {
        return Err("Database file not found");
    }
    libakuma::close(file_fd);

    // Open the database
    let db = vfs::open_db(path)?;

    // Get list of tables
    match vfs::list_tables(db) {
        Ok(tables) => {
            print("Tables in ");
            print(path);
            print(":\n");

            if tables.is_empty() {
                print("  (no tables)\n");
            } else {
                for table in tables {
                    print("  - ");
                    print(&table);
                    print("\n");
                }
            }
        }
        Err(e) => {
            vfs::close_db(db);
            return Err(e);
        }
    }

    vfs::close_db(db);
    Ok(())
}

/// Start the TCP server
fn cmd_serve(path: &str) -> Result<(), &'static str> {
    print("sqld: Starting server for ");
    print(path);
    print("\n");

    // Run the TCP server
    server::run(path)
}

/// Execute SQL via server connection
fn cmd_run() -> Result<(), &'static str> {
    // Check for -h host:port option
    let (server_addr, sql_arg_idx) = if argc() > 3 && arg(2) == Some("-h") {
        // sqld run -h 10.0.2.15:4321 "SELECT 1"
        let addr = arg(3).ok_or("missing server address after -h")?;
        (addr, 4)
    } else {
        ("127.0.0.1:4321", 2)
    };
    
    // Get SQL from argument or stdin
    let sql = if argc() > sql_arg_idx as u32 {
        // SQL provided as argument
        String::from(arg(sql_arg_idx as u32).ok_or("missing SQL")?)
    } else {
        // Read from stdin
        read_stdin()?
    };
    
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty SQL query");
    }
    
    client::run_with_addr(server_addr, sql_trimmed)
}

/// Read all available data from stdin
fn read_stdin() -> Result<String, &'static str> {
    let mut data = alloc::vec::Vec::new();
    let mut buf = [0u8; 256];
    
    loop {
        let n = libakuma::read(fd::STDIN, &mut buf);
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
        
        // Limit stdin to 64KB
        if data.len() > 64 * 1024 {
            return Err("stdin too large");
        }
    }
    
    String::from_utf8(data).map_err(|_| "invalid UTF-8 in stdin")
}
