//! SSH Server - Accept Loop and Connection Spawning
//!
//! Manages the SSH server accept loop that listens for connections
//! and spawns per-connection async tasks to handle each session.

use embassy_net::Stack;
use embassy_time::{Duration, Timer};

use crate::async_net::TcpListener;
use crate::console;
use crate::ssh;

// ============================================================================
// Constants
// ============================================================================

const SSH_PORT: u16 = 22;

// ============================================================================
// SSH Server Accept Loop
// ============================================================================

/// Run the SSH server accept loop
/// Listens for incoming connections and handles them in the current task
/// (for simplicity - a full implementation would spawn tasks for each connection)
pub async fn run(stack: Stack<'static>) {
    log("[SSH Server] Starting SSH server on port 22...\n");
    log("[SSH Server] Connect with: ssh -o StrictHostKeyChecking=no user@localhost -p 2222\n");

    // Initialize shared host key
    ssh::init_host_key();

    let listener = TcpListener::new(stack, SSH_PORT);

    loop {
        match listener.accept().await {
            Ok(stream) => {
                log("[SSH Server] Accepted new connection\n");
                // Handle connection inline (single-tasked for simplicity)
                // In a full embassy setup, we'd use spawner.spawn() here
                ssh::handle_connection(stream).await;
                log("[SSH Server] Connection handled, listening again...\n");
            }
            Err(e) => {
                log(&alloc::format!(
                    "[SSH Server] Accept error: {:?}, retrying...\n",
                    e
                ));
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}

