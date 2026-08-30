//! Socket teardown and the connecting-handle bookkeeping.

use super::*;

/// Record that `handle` has just entered `SynSent`, so `poll()` can enforce
/// [`CONNECT_TIMEOUT_US`] on it.
pub fn note_connect_started(handle: SocketHandle) {
    with_network(|net| {
        let now = (runtime().uptime_us)();
        // A redial on the same handle replaces the old deadline rather than
        // stacking a second one.
        if let Some(e) = net.connecting.iter_mut().find(|(h, _)| *h == handle) {
            e.1 = now;
        } else {
            net.connecting.push((handle, now));
        }
    });
}

/// Drop `handle`'s entry from `connecting`, if it has one.
///
/// Split out of [`socket_close`] as a pure function over plain data so the
/// invariant is host-testable without a real smoltcp `SocketSet` — see
/// `smoltcp_close_removes_connecting_entry` in `tests.rs`.
///
/// A close() on a socket still in `SynSent` (non-blocking connect, fd closed
/// before the handshake finished) transitions it straight to `Closed`, which
/// the `pending_removal` sweep in `poll()` frees on its very next pass. If
/// `handle` were left in `connecting`, the sweep right after it in the same
/// `poll()` call would call `sockets.get()` on an already-removed handle and
/// panic with smoltcp's "handle does not refer to a valid socket" — this is
/// what actually happened, not a rare race: any non-blocking connect closed
/// before it establishes hits it deterministically.
pub(crate) fn purge_connecting(connecting: &mut Vec<(SocketHandle, u64)>, handle: SocketHandle) {
    connecting.retain(|(h, _)| *h != handle);
}

pub fn socket_close(handle: SocketHandle) {
    if !is_valid_handle(handle) {
        crate::safe_print!(72, "[NET] CORRUPT HANDLE in socket_close: handle={handle}\n");
        return;
    }
    with_network(|net| {
        let socket = net.sockets.get_mut::<tcp::Socket>(handle);
        socket.close();
        net.pending_removal.push((handle, (runtime().uptime_us)()));
        purge_connecting(&mut net.connecting, handle);
    });
}
