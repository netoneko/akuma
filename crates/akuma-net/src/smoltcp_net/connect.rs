//! The async `tcp_connect` used by the in-kernel HTTP client.

use super::*;

// ============================================================================

/// Async TCP connect - creates a socket, connects to the remote, and returns a `TcpStream`.
/// Suitable for use from async shell commands running in `block_on` contexts.
pub async fn tcp_connect(addr: IpAddress, port: u16) -> Result<(TcpStream, SocketHandle), TcpError> {
    let handle = socket_create().ok_or(TcpError::WriteError)?;
    let local_port = alloc_ephemeral_port();

    let connected = with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        let cx = net.iface.context();
        socket.connect(cx, (addr, port), local_port).is_ok()
    }).unwrap_or(false);

    if !connected {
        socket_close(handle);
        return Err(TcpError::WriteError);
    }

    // Wait for connection to be established
    core::future::poll_fn(|cx| {
        if !is_valid_handle(handle) {
            return Poll::Ready(Err(TcpError::WriteError));
        }
        // Drive the network stack forward
        poll();
        with_network(|net| {
            let socket = net.sockets.get_mut::<tcp::Socket>(handle);
            match socket.state() {
                tcp::State::Established => Poll::Ready(Ok(())),
                tcp::State::Closed | tcp::State::Closing | tcp::State::TimeWait => {
                    Poll::Ready(Err(TcpError::WriteError))
                }
                _ => {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }
        }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
    }).await?;

    Ok((TcpStream::new(handle), handle))
}
