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
    /// Cached socket index for corruption detection. Must always be < `MAX_SOCKETS`.
    handle_index: usize,
}

/// Extract the internal index from a `SocketHandle`.
///
/// smoltcp 0.12 declares `pub struct SocketHandle(usize)` with a **private**
/// field and no accessor beyond `Display`, so there is no safe route to the
/// index — and the index is load-bearing: [`is_valid_handle`] guards five real
/// paths against a corrupted handle reaching the socket set.
///
/// # Why the assertions below are not enough on their own
///
/// `size_of` proves nothing about field *offset*, and nothing at all about a
/// future smoltcp adding a second field or changing `repr`. Both would keep
/// this compiling and silently return garbage. The assumption is therefore
/// checked against the real type at test time by
/// `tests::socket_handle_layout_tests` — which builds an actual `SocketSet`,
/// adds sockets to it, and asserts this function agrees with the handle smoltcp
/// itself handed back. That test is what fails on the next smoltcp bump; keep
/// it in step with any change here.
///
/// `pub(crate)` only so that test can reach it.
pub(crate) fn socket_handle_index(handle: SocketHandle) -> usize {
    // A single-field struct has its field at offset 0, so with equal size and
    // alignment the transmute is a no-op reinterpretation. Both halves are
    // asserted because a `repr` change upstream could break either.
    const _: () = assert!(
        core::mem::size_of::<SocketHandle>() == core::mem::size_of::<usize>()
    );
    const _: () = assert!(
        core::mem::align_of::<SocketHandle>() == core::mem::align_of::<usize>()
    );
    // SAFETY: layout-compatible per the assertions above and the test named in
    // the doc comment. `SocketHandle` is `Copy` with no `Drop`, so no ownership
    // is duplicated or lost.
    unsafe { core::mem::transmute::<SocketHandle, usize>(handle) }
}

/// Check if a `SocketHandle` index is within the valid range for our socket set.
pub(crate) fn is_valid_handle(handle: SocketHandle) -> bool {
    socket_handle_index(handle) < MAX_SOCKETS
}

/// Build a `SocketHandle` from a raw index for host tests, which have no real
/// `SocketSet` to call `add()` on. The inverse of [`socket_handle_index`]; see
/// that function's safety note.
#[cfg(test)]
pub(crate) fn test_socket_handle(idx: usize) -> SocketHandle {
    unsafe { core::mem::transmute::<usize, SocketHandle>(idx) }
}

impl TcpStream {
    #[must_use] 
    pub fn new(handle: SocketHandle) -> Self {
        Self {
            handle,
            handle_index: socket_handle_index(handle),
        }
    }
}

impl embedded_io_async::ErrorType for TcpStream {
    type Error = TcpError;
}

impl embedded_io_async::Read for TcpStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(|cx| {
            // Validate handle before accessing the socket set. A corrupted
            // async state machine could overwrite handle_index with garbage;
            // catch it here instead of panicking inside smoltcp's get_mut.
            if self.handle_index >= MAX_SOCKETS {
                crate::safe_print!(
                    96,
                    "[NET] CORRUPT HANDLE in TcpStream::read: index={}, handle={}\n",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::ReadError));
            }
            with_network(|net| {
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
            if self.handle_index >= MAX_SOCKETS {
                crate::safe_print!(
                    96,
                    "[NET] CORRUPT HANDLE in TcpStream::write: index={}, handle={}\n",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::WriteError));
            }
            with_network(|net| {
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
            if self.handle_index >= MAX_SOCKETS {
                crate::safe_print!(
                    96,
                    "[NET] CORRUPT HANDLE in TcpStream::flush: index={}, handle={}\n",
                    self.handle_index,
                    self.handle
                );
                return Poll::Ready(Err(TcpError::WriteError));
            }
            with_network(|net| {
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
