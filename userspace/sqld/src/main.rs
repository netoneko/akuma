//! sqld - SQLite daemon for Akuma
//!
//! A userspace SQLite CLI tool that provides:
//! - `sqld status <file>` - Show list of tables in a database
//! - `sqld <file>` - Start a TCP server on port 4321 (stub)

#![no_std]
#![no_main]

extern crate alloc;

mod server;
mod vfs;

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
    // Debug: show all arguments
    print("sqld: argc=");
    print_num(argc());
    print("\n");
    for i in 0..argc() {
        print("sqld: arg[");
        print_num(i);
        print("]=");
        if let Some(a) = arg(i) {
            print(a);
        } else {
            print("(none)");
        }
        print("\n");
    }

    print("sqld: Initializing SQLite...\n");
    
    // Initialize SQLite VFS
    vfs::init()?;
    
    print("sqld: SQLite initialized\n");

    // arg(0) = program name, arg(1) = first argument, etc.
    if argc() < 2 {
        print_usage();
        return Err("missing arguments");
    }

    let first_arg = arg(1).ok_or("missing command")?;

    match first_arg {
        "status" => {
            let path = arg(2).ok_or("missing file path")?;
            cmd_status(path)
        }
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        path => {
            // Treat as file path for serve mode
            cmd_serve(path)
        }
    }
}

fn print_usage() {
    print("sqld - SQLite daemon for Akuma\n");
    print("\n");
    print("Usage:\n");
    print("  sqld status <file>  Show tables in database\n");
    print("  sqld <file>         Start TCP server on port 4321\n");
    print("  sqld help           Show this help\n");
}

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
    print("sqld: Checking file: ");
    print(path);
    print("\n");

    // Check if file exists first
    let fd = libakuma::open(path, libakuma::open_flags::O_RDONLY);
    if fd < 0 {
        return Err("Database file not found");
    }
    print("sqld: File exists, fd=");
    print_num(fd as u32);
    print("\n");
    libakuma::close(fd);

    // Open the database
    print("sqld: Opening SQLite database...\n");
    let db = vfs::open_db(path)?;
    print("sqld: Database opened successfully\n");

    // Get list of tables
    print("sqld: Querying tables...\n");
    match vfs::list_tables(db) {
        Ok(tables) => {
            print("\nTables in ");
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

/// Start the TCP server (stub implementation)
fn cmd_serve(path: &str) -> Result<(), &'static str> {
    print("sqld: Database path: ");
    print(path);
    print("\n");

    // Check if database file exists
    let fd = libakuma::open(path, libakuma::open_flags::O_RDONLY);
    if fd < 0 {
        print("sqld: Warning - database file does not exist, will be created on first write\n");
    } else {
        libakuma::close(fd);
    }

    // Run the TCP server stub
    server::run(path)
}
