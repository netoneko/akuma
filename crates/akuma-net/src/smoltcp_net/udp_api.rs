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
pub fn udp_socket_bind(handle: SocketHandle, port: u16) -> Result<(), ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.bind(port).map_err(|_| ())
    }).unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_send(handle: SocketHandle, buf: &[u8], remote: smoltcp::wire::IpEndpoint) -> Result<usize, ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.send_slice(buf, remote).map(|()| buf.len()).map_err(|_| ())
    }).unwrap_or(Err(()))
}

#[allow(clippy::result_unit_err)]
pub fn udp_socket_recv(handle: SocketHandle, buf: &mut [u8]) -> Result<(usize, smoltcp::wire::IpEndpoint), ()> {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        match socket.recv_slice(buf) {
            Ok((len, meta)) => Ok((len, meta.endpoint)),
            Err(_) => Err(()),
        }
    }).unwrap_or(Err(()))
}

/// Register `waker` on the **smoltcp socket itself**, so a state change wakes
/// it from inside `process_tcp` rather than from `wake_all`'s list walk after
/// `poll()` has released `NETWORK`.
///
/// # Why this exists
///
/// `net-waker-park`'s measurement (root `Cargo.toml`) found the targeted park
/// *slower* than the promiscuous halt, and named the reason: `wake_all` drains,
/// so a waiter's registration survives exactly one wake and anything arriving
/// during its 64-poll drain lands on the 3 ms backstop. smoltcp's own
/// registration is one-shot too — `WakerRegistration::wake` also `take()`s —
/// but it fires at the state transition (`tcp.rs:848`, `:1327`, `:2072`), so the
/// window it can be lost in is a few instructions rather than a whole lap.
///
/// # Lock discipline
///
/// This runs under `NETWORK` via [`with_network`], and the waker it stores is
/// later fired from *inside* `iface.poll()` — i.e. with `NETWORK` held and IRQs
/// masked. That is only safe because `ThreadWaker::wake`
/// (`akuma-exec/src/threading/mod.rs:3569`) is lock-free: a generation gate, a
/// sticky store, a `WAITING`→`READY` CAS, then an SGI. It touches neither
/// `SOCKET_TABLE` nor the console, so it cannot recreate the AB-BA that
/// [`poll`] defers `wake_all` past `drop(guard)` to avoid. **Do not register a
/// waker here whose `wake` takes any lock.**
///
/// Both halves are registered: a waiter may be blocked on either direction and
/// the caller's `condition` closure is opaque to us.
///
/// Returns `false` when the stack is not up, so nothing was registered. The
/// caller still parks — its backstop is exactly the fallback for a wake that
/// cannot arrive — but it is not a silent no-op.
#[must_use]
pub fn register_socket_waker(handle: SocketHandle, is_udp: bool, waker: &core::task::Waker) -> bool {
    with_network(|net| {
        if is_udp {
            let s = net.sockets.get_mut::<udp::Socket>(handle);
            s.register_recv_waker(waker);
            s.register_send_waker(waker);
        } else {
            let s = net.sockets.get_mut::<tcp::Socket>(handle);
            s.register_recv_waker(waker);
            s.register_send_waker(waker);
        }
    })
    .is_some()
}

#[must_use]
pub fn udp_can_recv(handle: SocketHandle) -> bool {
    with_network(|net| {
        net.sockets.get::<udp::Socket>(handle).can_recv()
    }).unwrap_or(false)
}

#[must_use] 
pub fn udp_can_send(handle: SocketHandle) -> bool {
    with_network(|net| {
        net.sockets.get::<udp::Socket>(handle).can_send()
    }).unwrap_or(false)
}

pub fn udp_socket_close(handle: SocketHandle) {
    with_network(|net| {
        let socket = net.sockets.get_mut::<udp::Socket>(handle);
        socket.close();
        net.sockets.remove(handle);
        SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
    });
}
