#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::task::{Context, Poll};
use core::pin::Pin;

use libakuma::*;
use libakuma::net::{TcpListener, TcpStream, Error as NetError};
use embedded_io_async::{Read, Write, ErrorType};

mod crypto;
mod auth;
mod keys;
mod config;
mod protocol;
mod shell;

// ============================================================================
// TcpStream Wrapper for embedded-io-async
// ============================================================================

pub struct SshStream {
    inner: TcpStream,
}

impl SshStream {
    pub fn new(inner: TcpStream) -> Self {
        Self { inner }
    }
}

impl ErrorType for SshStream {
    type Error = NetError;
}

impl Read for SshStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.inner.read(buf)
    }
}

impl Write for SshStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.inner.write(buf)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    println("[SSHD] Starting userspace SSH server...");

    // 1. Load config from file first
    block_on(config::load_config());
    block_on(keys::load_or_generate_host_key());
    
    let mut ssh_config = config::get_config();
    let mut cli_port: Option<u16> = None;

    // 2. Parse CLI arguments (overrides config file)
    let mut args = args();
    args.next(); // Skip program name
    
    while let Some(arg) = args.next() {
        match arg {
            "--shell" => {
                if let Some(shell_path) = args.next() {
                    ssh_config.shell = Some(alloc::string::String::from(shell_path));
                    println(&format!("[SSHD] Shell override from CLI: {}", shell_path));
                }
            }
            "--port" => {
                if let Some(port_str) = args.next() {
                    if let Ok(p) = port_str.parse::<u16>() {
                        cli_port = Some(p);
                        println(&format!("[SSHD] Port override from CLI: {}", p));
                    }
                }
            }
            _ => {
                println(&format!("[SSHD] Unknown argument: {}", arg));
            }
        }
    }

    // Determine final port: CLI > Config > Default(2222)
    let final_port = cli_port.or(ssh_config.port).unwrap_or(2222);

    let addr = format!("0.0.0.0:{}", final_port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln(&format!("[SSHD] Failed to bind to {}: {:?}", addr, e));
            exit(1);
        }
    };

    println(&format!("[SSHD] Listening on {}...", addr));
    if let Some(ref shell) = ssh_config.shell {
        println(&format!("[SSHD] Default shell: {}", shell));
    } else {
        println("[SSHD] Default shell: built-in");
    }

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                println("[SSHD] Accepted connection");
                handle_connection(stream, ssh_config.clone());
            }
            Err(e) => {
                eprintln(&format!("[SSHD] Accept error: {:?}", e));
            }
        }
    }
}

fn handle_connection(stream: TcpStream, config: config::SshdConfig) {
    let ssh_stream = SshStream::new(stream);
    block_on(protocol::handle_connection(ssh_stream, config));
}

fn block_on<F: core::future::Future>(mut future: F) -> F::Output {
    let mut future = unsafe { Pin::new_unchecked(&mut future) };
    
    static VTABLE: core::task::RawWakerVTable = core::task::RawWakerVTable::new(
        |_| core::task::RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    
    loop {
        let raw_waker = core::task::RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { core::task::Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);
        
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                sleep_ms(1);
            }
        }
    }
}
