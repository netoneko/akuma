//! TCP Server stub for sqld
//!
//! This is a placeholder implementation that accepts connections on port 4321,
//! prints whatever input it receives, and closes the connection.

use libakuma::net::{TcpListener, TcpStream};
use libakuma::print;

const SERVER_PORT: u16 = 4321;

/// Run the TCP server stub
///
/// This is a placeholder that:
/// 1. Binds to 0.0.0.0:4321
/// 2. Accepts connections in a loop
/// 3. Reads input and prints it
/// 4. Closes the connection
pub fn run(_db_path: &str) -> Result<(), &'static str> {
    let addr = "0.0.0.0:4321";
    
    print("sqld: Starting server on port ");
    print_port(SERVER_PORT);
    print("\n");

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
                
                handle_connection(stream);
            }
            Err(e) => {
                if e.kind != libakuma::net::ErrorKind::WouldBlock {
                    print("sqld: Accept error\n");
                }
                // Yield before retrying
                libakuma::sleep_ms(10);
            }
        }
    }
}

fn handle_connection(stream: TcpStream) {
    let mut buf = [0u8; 1024];
    
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            print("sqld: Received ");
            print_num(n);
            print(" bytes: ");
            
            // Print the received data (as UTF-8 if valid)
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                print(s);
            } else {
                print("[binary data]");
            }
            print("\n");
            
            // Send a response
            let response = b"sqld: Command received (stub - not implemented yet)\n";
            let _ = stream.write(response);
        }
        Ok(_) => {
            print("sqld: Connection closed by client\n");
        }
        Err(_) => {
            print("sqld: Read error\n");
        }
    }
    
    // Connection closes when stream is dropped
    print("sqld: Connection closed\n");
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
