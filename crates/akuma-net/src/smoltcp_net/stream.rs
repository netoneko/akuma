//! `TcpStream` — the `embedded-io-async` half, and `SocketHandle` indexing.

use super::*;

// Async TCP Stream (embedded-io-async)
// ============================================================================


#[derive(Debug, Clone, Copy)]
pub enum TcpError {
    ReadError,
    WriteError,
}

impl embedded_io_async::Error for TcpError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
}

pub struct TcpStream {
    handle: SocketHandle,
}


/// Is `handle` still a live socket in `sockets`?
///
/// **Safe since 2026-08-30.** This used to `transmute` the handle to a `usize`
/// and bounds-check it against `MAX_SOCKETS`, because smoltcp keeps
/// `SocketHandle(usize)`'s field private and offers no accessor. Asking the set
/// whether it still holds the handle needs no such trick, and it is a *stronger*
/// guard: an in-range handle whose socket has been removed passed the old check
/// and then panicked inside smoltcp's `get_mut`.
///
/// O(live sockets). Every caller is a per-operation or per-connect-sweep path,
/// never per-packet — `net.connecting` and `net.pending_removal` hold a handful
/// of entries in practice.
pub(crate) fn is_valid_handle(sockets: &SocketSet<'static>, handle: SocketHandle) -> bool {
    sockets.iter().any(|(h, _)| h == handle)
}

impl TcpStream {
    #[must_use] 
    pub fn new(handle: SocketHandle) -> Self {
        Self { handle }
    }
}

impl embedded_io_async::ErrorType for TcpStream {
    type Error = TcpError;
}

impl embedded_io_async::Read for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                // The handle must still be in the set. A membership test,
                // not a bounds check on a cached index: it also catches a
                // handle whose socket was removed, which is the case the
                // old `index < MAX_SOCKETS` guard let through into
                // smoltcp's panicking `get_mut`.
                if !is_valid_handle(&net.sockets, self.handle) {
                    crate::safe_print!(72,
                        "[NET] CORRUPT HANDLE in TcpStream::read: handle={}\n",
                        self.handle);
                    return Poll::Ready(Err(TcpError::ReadError));
                }
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_recv() {
                    socket
                        .recv(|data| {
                            let len = data.len().min(buf.len());
                            buf[..len].copy_from_slice(&data[..len]);
                            (len, len)
                        })
                        .map_or(Poll::Ready(Err(TcpError::ReadError)), |n| Poll::Ready(Ok(n)))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Ok(0)) // EOF
                } else {
                    socket.register_recv_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::ReadError)))
        }).await
    }
}

impl embedded_io_async::Write for TcpStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                // Membership, not a bounds check on a cached index — see
                // `is_valid_handle`.
                if !is_valid_handle(&net.sockets, self.handle) {
                    crate::safe_print!(72,
                        "[NET] CORRUPT HANDLE in TcpStream::write: handle={}\n",
                        self.handle);
                    return Poll::Ready(Err(TcpError::WriteError));
                }
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_send() {
                    socket
                        .send_slice(buf)
                        .map_or(Poll::Ready(Err(TcpError::WriteError)), |n| Poll::Ready(Ok(n)))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError)) // Broken pipe
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
    
    async fn flush(&mut self) -> Result<(), Self::Error> {
        core::future::poll_fn(|cx| {
            with_network(|net| {
                // Membership, not a bounds check on a cached index — see
                // `is_valid_handle`.
                if !is_valid_handle(&net.sockets, self.handle) {
                    crate::safe_print!(72,
                        "[NET] CORRUPT HANDLE in TcpStream::flush: handle={}\n",
                        self.handle);
                    return Poll::Ready(Err(TcpError::WriteError));
                }
                let socket = net.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.send_queue() == 0 {
                    Poll::Ready(Ok(()))
                } else if socket.state() == tcp::State::Closed || socket.state() == tcp::State::CloseWait {
                    Poll::Ready(Err(TcpError::WriteError))
                } else {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                }
            }).unwrap_or(Poll::Ready(Err(TcpError::WriteError)))
        }).await
    }
}
