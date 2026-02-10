//! Network Self-Tests
//!
//! Verifies loopback connectivity and basic TCP state transitions.

use crate::smoltcp_net::{self, with_network};
use smoltcp::socket::tcp;
use smoltcp::wire::IpAddress;
use crate::console;

fn log(msg: &str) {
    console::print(msg);
}

/// Run a suite of network tests. Panics on failure.
pub fn run_tests() {
    log("[NetTest] Starting network self-tests...\n");

    test_loopback_connection();

    log("[NetTest] All network tests passed!\n");
}

fn test_loopback_connection() {
    log("[NetTest] Testing loopback connection (127.0.0.1:9999)...\n");

    const TEST_PORT: u16 = 9999;
    const LOCAL_PORT: u16 = 40000;
    const TEST_DATA: &[u8] = b"Akuma Network Test";

    // 1. Create Listener
    let listen_handle = smoltcp_net::socket_create().expect("Failed to create listen socket");
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(listen_handle);
        socket.listen(TEST_PORT).expect("Failed to listen");
    });

    // 2. Create Client
    let client_handle = smoltcp_net::socket_create().expect("Failed to create client socket");
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(client_handle);
        let cx = net.iface.context();
        // Use a valid local port (non-zero)
        socket.connect(cx, (IpAddress::v4(127, 0, 0, 1), TEST_PORT), LOCAL_PORT)
            .expect("Connect call failed");
    });

    // 3. Drive stack until established
    let mut success = false;
    for _ in 0..1000 {
        smoltcp_net::poll();
        
        let client_state = with_network(|net| net.sockets.get::<tcp::Socket>(client_handle).state());
        let server_state = with_network(|net| net.sockets.get::<tcp::Socket>(listen_handle).state());

        if client_state == Some(tcp::State::Established) && server_state == Some(tcp::State::Established) {
            success = true;
            break;
        }
        crate::threading::yield_now();
    }

    if !success {
        let client_state = with_network(|net| net.sockets.get::<tcp::Socket>(client_handle).state());
        let server_state = with_network(|net| net.sockets.get::<tcp::Socket>(listen_handle).state());
        crate::safe_print!(128, "[NetTest] Connection failed. Client: {:?}, Server: {:?}\n", client_state, server_state);
        panic!("Network Test Failed: Loopback connection timeout");
    }

    log("[NetTest] Connection established. Sending data...\n");

    // 4. Send Data
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(client_handle);
        socket.send_slice(TEST_DATA).expect("Failed to send");
    });

    // 5. Receive Data
    let mut received = false;
    let mut buf = [0u8; 64];
    
    for _ in 0..1000 {
        smoltcp_net::poll();
        
        let n = with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(listen_handle);
            if socket.can_recv() {
                socket.recv(|data| {
                    let len = data.len().min(buf.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    (len, len)
                }).unwrap_or(0)
            } else {
                0
            }
        }).unwrap_or(0);

        if n == TEST_DATA.len() && &buf[..n] == TEST_DATA {
            received = true;
            break;
        }
        crate::threading::yield_now();
    }

    if !received {
        panic!("Network Test Failed: Loopback data transfer failed");
    }

    log("[NetTest] Data transfer verified. Closing...\n");

    // 6. Close
    smoltcp_net::socket_close(client_handle);
    smoltcp_net::socket_close(listen_handle);

    log("[NetTest] Loopback test passed.\n");
}