//! TCP Server for sqld
//!
//! Implements a binary protocol for executing SQL queries over TCP.
//!
//! Protocol:
//!   Request:  [u32 length][SQL bytes]
//!   Response: [u32 length][u8 status][payload]
//!
//!   Status 0x00: OK with rows
//!     Payload: [u32 col_count][col names \0-separated][u32 row_count][rows, each col \0-separated]
//!   Status 0x01: OK, rows affected
//!     Payload: [u32 affected_count]
//!   Status 0xFF: Error
//!     Payload: [error message \0-terminated]

use alloc::vec::Vec;
use libakuma::net::{TcpListener, TcpStream};
use libakuma::print;

use sqld::vfs;

const SERVER_PORT: u16 = 4321;

// Response status codes
pub const STATUS_OK_ROWS: u8 = 0x00;
pub const STATUS_OK_AFFECTED: u8 = 0x01;
pub const STATUS_ERROR: u8 = 0xFF;

/// Run the TCP server
pub fn run(db_path: &str) -> Result<(), &'static str> {
    let addr = "127.0.0.1:4321";

    print("sqld: Starting server on port ");
    print_port(SERVER_PORT);
    print("\n");

    // Open database
    let db = vfs::open_db(db_path)?;
    print("sqld: Database opened\n");

    let listener = TcpListener::bind(addr).map_err(|_| "Failed to bind to address")?;
    
    print("sqld: Listening for connections...\n");

    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                print("sqld: Connection from ");
                print_ip(&addr.ip);
                print(":");
                print_port(addr.port);
                print("\n");
                
                handle_connection(stream, db);
            }
            Err(e) => {
                if e.kind != libakuma::net::ErrorKind::WouldBlock {
                    print("sqld: Accept error\n");
                }
                // Yield before retrying
                libakuma::sleep_ms(1);
            }
        }
    }
}

fn handle_connection(stream: TcpStream, db: *mut vfs::sqlite3) {
    // Read message length (4 bytes, big-endian)
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).is_err() {
        print("sqld: Failed to read message length\n");
        return;
    }
    
    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len == 0 || msg_len > 64 * 1024 {
        print("sqld: Invalid message length\n");
        send_error(&stream, "Invalid message length");
        return;
    }
    
    // Read SQL query
    let mut sql_buf = alloc::vec![0u8; msg_len];
    if stream.read_exact(&mut sql_buf).is_err() {
        print("sqld: Failed to read SQL\n");
        return;
    }
    
    let sql = match core::str::from_utf8(&sql_buf) {
        Ok(s) => s.trim(),
        Err(_) => {
            send_error(&stream, "Invalid UTF-8 in SQL");
            return;
        }
    };
    
    print("sqld: Executing (");
    print_num(sql.len());
    print(" bytes): [");
    print(sql);
    print("]\n");
    
    // Execute SQL
    match vfs::execute_sql(db, sql) {
        Ok(result) => {
            if result.columns.is_empty() {
                // Non-SELECT statement (INSERT, UPDATE, DELETE, etc.)
                send_affected(&stream, result.changes);
            } else {
                // SELECT statement - send rows
                send_rows(&stream, &result);
            }
        }
        Err(e) => {
            print("sqld: SQL error: ");
            print(&e);
            print("\n");
            send_error(&stream, &e);
        }
    }
    
    print("sqld: Request complete\n");
}

fn send_error(stream: &TcpStream, message: &str) {
    let msg_bytes = message.as_bytes();
    // Length = 1 (status) + message + 1 (null terminator)
    let payload_len = 1 + msg_bytes.len() + 1;
    
    let mut response = Vec::with_capacity(4 + payload_len);
    response.extend_from_slice(&(payload_len as u32).to_be_bytes());
    response.push(STATUS_ERROR);
    response.extend_from_slice(msg_bytes);
    response.push(0); // null terminator
    
    let _ = stream.write_all(&response);
}

fn send_affected(stream: &TcpStream, count: u32) {
    // Length = 1 (status) + 4 (count)
    let payload_len: u32 = 5;
    
    let mut response = Vec::with_capacity(9);
    response.extend_from_slice(&payload_len.to_be_bytes());
    response.push(STATUS_OK_AFFECTED);
    response.extend_from_slice(&count.to_be_bytes());
    
    let _ = stream.write_all(&response);
}

fn send_rows(stream: &TcpStream, result: &vfs::QueryResult) {
    let mut payload = Vec::new();
    
    // Status byte
    payload.push(STATUS_OK_ROWS);
    
    // Column count
    payload.extend_from_slice(&(result.columns.len() as u32).to_be_bytes());
    
    // Column names (null-separated)
    for col in &result.columns {
        payload.extend_from_slice(col.as_bytes());
        payload.push(0);
    }
    
    // Row count
    payload.extend_from_slice(&(result.rows.len() as u32).to_be_bytes());
    
    // Row data (each column null-separated)
    for row in &result.rows {
        for val in row {
            payload.extend_from_slice(val.as_bytes());
            payload.push(0);
        }
    }
    
    // Send with length prefix
    let mut response = Vec::with_capacity(4 + payload.len());
    response.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    response.extend_from_slice(&payload);
    
    let _ = stream.write_all(&response);
}

// Helper functions for printing without format!

fn print_port(port: u16) {
    print_num(port as usize);
}

fn print_ip(ip: &[u8; 4]) {
    print_num(ip[0] as usize);
    print(".");
    print_num(ip[1] as usize);
    print(".");
    print_num(ip[2] as usize);
    print(".");
    print_num(ip[3] as usize);
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
    
    // Reverse and print
    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        if let Ok(ch) = core::str::from_utf8(&s) {
            print(ch);
        }
    }
}
