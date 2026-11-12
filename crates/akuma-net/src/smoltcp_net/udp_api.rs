//! The UDP socket API.

use super::*;

// UDP Socket API
// ============================================================================

const UDP_PACKET_COUNT: usize = 8;
/// DNS responses can exceed 512 bytes when there are many records (CNAME chains,
/// multiple A/AAAA records, TXT in additional section). 1500 bytes handles most cases
/// without exceeding typical MTU.
const UDP_PAYLOAD_SIZE: usize = 1500;

#[must_use] 
pub fn udp_socket_create() -> Option<SocketHandle> {
    with_network(|net| {
        if sockets_live() >= MAX_SOCKETS {
            return None;
        }
        let rx_meta = udp::PacketMetadata::EMPTY;
        let tx_meta = udp::PacketMetadata::EMPTY;
        let rx_buffer = udp::PacketBuffer::new(
            vec![rx_meta; UDP_PACKET_COUNT],
            vec![0u8; UDP_PACKET_COUNT * UDP_PAYLOAD_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![tx_meta; UDP_PACKET_COUNT],
            vec![0u8; UDP_PACKET_COUNT * UDP_PAYLOAD_SIZE],
        );
        let socket = udp::Socket::new(rx_buffer, tx_buffer);
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(net.sockets.add(socket))
    }).flatten()
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_bind(handle: SocketHandle, ip: [u8; 4], port: u16) -> Result<(), ()> {
    // `ip` narrows the socket to datagrams addressed to it, exactly as a TCP
    // listener's endpoint does — see `crate::socket::listen_endpoint`.
    let endpoint = crate::socket::listen_endpoint(ip, port);
    with_network(|net| {
        udp_get_mut(&mut net.sockets, handle, |socket| socket.bind(endpoint).map_err(|_| ()))
    }).flatten().unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_send(handle: SocketHandle, buf: &[u8], remote: smoltcp::wire::IpEndpoint) -> Result<usize, ()> {
    with_network(|net| {
        udp_get_mut(&mut net.sockets, handle, |socket| {
            socket.send_slice(buf, remote).map(|()| buf.len()).map_err(|_| ())
        })
    }).flatten().unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_recv(handle: SocketHandle, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpEndpoint), ()> {
    with_network(|net| {
        udp_get_mut(&mut net.sockets, handle, |socket| {
            match socket.recv_slice(buf) {
                Ok((len, meta)) => Ok((len, meta.endpoint)),
                Err(_) => Err(()),
            }
        })
    }).flatten().unwrap_or(Err(()))
}

#[must_use]
pub fn udp_can_recv(handle: SocketHandle) -> bool {
    with_network(|net| {
        udp_get(&net.sockets, handle, smoltcp::socket::udp::Socket::can_recv)
    }).flatten().unwrap_or(false)
}

#[must_use]
pub fn udp_can_send(handle: SocketHandle) -> bool {
    with_network(|net| {
        udp_get(&net.sockets, handle, smoltcp::socket::udp::Socket::can_send)
    }).flatten().unwrap_or(false)
}

pub fn udp_socket_close(handle: SocketHandle) {
    with_network(|net| {
        // Already gone — a sibling's close won the race. Removing a removed
        // handle panics inside smoltcp, so this is a guard, not paranoia.
        if !is_valid_handle(&net.sockets, handle) {
            return;
        }
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.close();
        net.sockets.remove(handle);
        SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
    });
}
