//! sqld client - connects to sqld server and executes SQL
//!
//! Usage:
//!   sqld run              # Read SQL from stdin
//!   sqld run "SELECT 1"   # Execute SQL from argument

use alloc::string::String;
use alloc::vec::Vec;
use libakuma::net::TcpStream;
use libakuma::print;

use crate::server::{STATUS_OK_ROWS, STATUS_OK_AFFECTED, STATUS_ERROR};

/// Run SQL query against the server
pub fn run(sql: &str) -> Result<(), &'static str> {
    run_with_addr("127.0.0.1:4321", sql)
}

/// Run SQL query against a server at a specific address
pub fn run_with_addr(addr: &str, sql: &str) -> Result<(), &'static str> {
    // Connect to server
    print("sqld: Connecting to ");
    print(addr);
    print("...\n");
    
    let stream = TcpStream::connect(addr)
        .map_err(|_| "Failed to connect to sqld server")?;
    
    print("sqld: Connected\n");
    
    // Send request: [u32 length][SQL bytes]
    let sql_bytes = sql.as_bytes();
    let len = sql_bytes.len() as u32;
    
    stream.write_all(&len.to_be_bytes())
        .map_err(|_| "Failed to send length")?;
    stream.write_all(sql_bytes)
        .map_err(|_| "Failed to send SQL")?;
    
    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)
        .map_err(|_| "Failed to read response length")?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    
    if resp_len == 0 {
        return Err("Empty response from server");
    }
    
    // Read response payload
    let mut payload = alloc::vec![0u8; resp_len];
    stream.read_exact(&mut payload)
        .map_err(|_| "Failed to read response")?;
    
    // Parse response
    let status = payload[0];
    let data = &payload[1..];
    
    match status {
        STATUS_OK_ROWS => {
            display_rows(data)?;
        }
        STATUS_OK_AFFECTED => {
            if data.len() >= 4 {
                let affected = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                print("OK, ");
                print_num(affected as usize);
                print(" row(s) affected\n");
            }
        }
        STATUS_ERROR => {
            print("ERROR: ");
            // Error message is null-terminated
            if let Some(end) = data.iter().position(|&b| b == 0) {
                if let Ok(msg) = core::str::from_utf8(&data[..end]) {
                    print(msg);
                }
            }
            print("\n");
            return Err("SQL error");
        }
        _ => {
            return Err("Unknown response status");
        }
    }
    
    Ok(())
}

fn display_rows(data: &[u8]) -> Result<(), &'static str> {
    let mut offset = 0;
    
    // Read column count
    if data.len() < 4 {
        return Err("Invalid response: missing column count");
    }
    let col_count = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += 4;
    
    // Read column names
    let mut columns = Vec::new();
    for _ in 0..col_count {
        let (name, new_offset) = read_null_string(data, offset)?;
        columns.push(name);
        offset = new_offset;
    }
    
    // Read row count
    if data.len() < offset + 4 {
        return Err("Invalid response: missing row count");
    }
    let row_count = u32::from_be_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3]
    ]) as usize;
    offset += 4;
    
    // Print header
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            print("\t");
        }
        print(col);
    }
    print("\n");
    
    // Print separator
    for (i, _) in columns.iter().enumerate() {
        if i > 0 {
            print("\t");
        }
        print("---");
    }
    print("\n");
    
    // Read and print rows
    for _ in 0..row_count {
        for i in 0..col_count {
            if i > 0 {
                print("\t");
            }
            let (val, new_offset) = read_null_string(data, offset)?;
            print(&val);
            offset = new_offset;
        }
        print("\n");
    }
    
    // Print row count summary
    print_num(row_count);
    print(" row(s)\n");
    
    Ok(())
}

fn read_null_string(data: &[u8], start: usize) -> Result<(String, usize), &'static str> {
    let slice = &data[start..];
    let end = slice.iter().position(|&b| b == 0)
        .ok_or("Invalid response: unterminated string")?;
    
    let s = core::str::from_utf8(&slice[..end])
        .map_err(|_| "Invalid UTF-8 in response")?;
    
    Ok((String::from(s), start + end + 1))
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
        if let Ok(ch) = core::str::from_utf8(&s) {
            print(ch);
        }
    }
}
