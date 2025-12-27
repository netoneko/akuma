//! SSH Transport Layer
//!
//! Handles the TCP socket for SSH connections.

use alloc::vec::Vec;

use smoltcp::socket::tcp::{Socket as TcpSocket, State as TcpState};

use crate::network;

// ============================================================================
// Constants
// ============================================================================

const SSH_PORT: u16 = 22;

// ============================================================================
// SSH Socket Polling
// ============================================================================

/// SSH connection event returned by poll
pub enum SshEvent {
    /// No event
    None,
    /// New connection established
    Connected,
    /// Data received (contains the data)
    Data(Vec<u8>),
    /// Connection closed
    Disconnected,
}

/// Poll the SSH socket and return any events
/// This is called by the SSH module to handle the transport layer
pub fn poll() -> SshEvent {
    network::with_netstack(|stack| {
        // Process network interface
        let timestamp = crate::timer::uptime_us();
        let timestamp = smoltcp::time::Instant::from_micros(timestamp as i64);
        stack
            .iface
            .poll(timestamp, &mut stack.device, &mut stack.sockets);

        // Handle SSH socket
        let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);

        // Check for new connection
        if socket.state() == TcpState::Established && !stack.ssh_was_connected {
            stack.ssh_was_connected = true;
            network::increment_connections();
            return SshEvent::Connected;
        }

        // Check for disconnect - handle all non-established states when we were connected
        let state = socket.state();
        if stack.ssh_was_connected && state != TcpState::Established {
            stack.ssh_was_connected = false;
            // Abort and re-listen to ensure socket is ready for new connections
            socket.abort();
            let _ = socket.listen(SSH_PORT);
            return SshEvent::Disconnected;
        }

        // Check for received data
        if socket.can_recv() {
            let mut buf = [0u8; 512];
            match socket.recv_slice(&mut buf) {
                Ok(len) if len > 0 => {
                    network::add_bytes_rx(len as u64);
                    return SshEvent::Data(buf[..len].to_vec());
                }
                _ => {}
            }
        }

        SshEvent::None
    })
    .unwrap_or(SshEvent::None)
}

/// Send data on the SSH socket
pub fn send(data: &[u8]) -> bool {
    network::with_netstack(|stack| {
        let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);
        if socket.can_send() {
            match socket.send_slice(data) {
                Ok(len) => {
                    network::add_bytes_tx(len as u64);
                    true
                }
                Err(_) => false,
            }
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// Close the SSH connection and prepare for new connections
pub fn close() {
    network::with_netstack(|stack| {
        let socket = stack.sockets.get_mut::<TcpSocket>(stack.ssh_handle);

        // Abort the connection to immediately reset the socket state
        // This avoids getting stuck in TIME_WAIT or other intermediate states
        socket.abort();
        stack.ssh_was_connected = false;

        // Re-listen immediately
        let _ = socket.listen(SSH_PORT);
    });
}
