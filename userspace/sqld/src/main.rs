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

use libakuma::{arg, argc, exit, print};

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
    // Initialize SQLite VFS
    vfs::init()?;

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

/// Show the status of a database file (list of tables)
fn cmd_status(path: &str) -> Result<(), &'static str> {
    print("sqld: Opening database: ");
    print(path);
    print("\n");

    // Check if file exists first
    let fd = libakuma::open(path, libakuma::open_flags::O_RDONLY);
    if fd < 0 {
        return Err("Database file not found");
    }
    libakuma::close(fd);

    // Open the database
    let db = vfs::open_db(path)?;

    // Get list of tables
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
