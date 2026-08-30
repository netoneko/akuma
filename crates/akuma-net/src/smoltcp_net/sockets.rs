//! Socket-set slot management: allocation, the soft cap, and reclaim.

use super::*;

// Socket API (Wrappers)
// ============================================================================

#[must_use] 
/// Free slots held by sockets that are only waiting out a protocol timer.
///
/// `socket_close` does not drop a socket immediately — it parks the handle in
/// `pending_removal` so smoltcp can finish the teardown handshake, and `poll`'s
/// sweep collects it once the state reaches `Closed` or `SOCKET_GC_TIMEOUT_US`
/// (**30 s**) expires. That is correct for a long-lived connection and badly
/// wrong for connection-per-request traffic: a socket in `TimeWait` holds a slot
/// for smoltcp's full close timeout, so at ~900 HTTP requests a second the
/// 128-slot budget drains in well under a second and every subsequent `accept`
/// fails. Measured 2026-08-19: **26 % of requests reset**
/// (`docs/archive/AKUMA_NET_ISSUES.md` §3.4).
///
/// Under pressure the states below are reclaimed early, oldest first:
///
/// - `TimeWait` — the connection is over. TIME-WAIT exists to absorb delayed
///   duplicates from the *old* incarnation of a 4-tuple; recycling it under slot
///   pressure is what every production stack does (Linux's `tcp_tw_recycle`
///   lineage, and its `tcp_max_tw_buckets` cap, which simply drops the oldest).
/// - `Closed` — already collectable; the periodic sweep just has not run.
///
/// Nothing else is touched. A socket still in `FinWait1`/`FinWait2`/`LastAck` is
/// waiting on the *peer*, and killing it would discard data the peer is still
/// entitled to send, so those keep the 30 s timeout they always had.
///
/// Returns how many slots it freed.
fn reclaim_pending_slots(net: &mut NetworkState, want: usize) -> usize {
    if want == 0 {
        return 0;
    }
    let mut freed = 0;
    // Oldest first: `pending_removal` holds the close timestamp, and the entry
    // that has waited longest is the one whose duplicates are least likely to
    // still be in flight.
    while freed < want {
        let mut oldest: Option<(usize, u64)> = None;
        for (i, &(handle, added_at)) in net.pending_removal.iter().enumerate() {
            if !is_valid_handle(handle) {
                continue;
            }
            let state = net.sockets.get::<tcp::Socket>(handle).state();
            if !matches!(state, tcp::State::TimeWait | tcp::State::Closed) {
                continue;
            }
            if oldest.is_none_or(|(_, t)| added_at < t) {
                oldest = Some((i, added_at));
            }
        }
        let Some((idx, _)) = oldest else { break };
        let (handle, _) = net.pending_removal[idx];
        net.sockets.get_mut::<tcp::Socket>(handle).abort();
        net.sockets.remove(handle);
        SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
        net.pending_removal.swap_remove(idx);
        freed += 1;
        RECLAIMED_SLOTS.fetch_add(1, Ordering::Relaxed);
    }
    freed
}

/// Slots to free per pressure-valve trip. Small next to `MAX_SOCKETS` (128 on
/// the devbox) so a burst never discards a meaningful fraction of the table,
/// large enough that a listener refilling an 8- or 32-deep backlog does not
/// re-scan `pending_removal` on every single `socket_create`.
const RECLAIM_BATCH: usize = 8;

/// The table size actually enforced right now. Starts at
/// [`SOCKET_SOFT_CAP_START`] and grows toward `MAX_SOCKETS` only under genuine
/// pressure — see [`grow_soft_cap`].
#[cfg(feature = "smoltcp")]
pub(crate) static SOCKET_SOFT_CAP: AtomicUsize = AtomicUsize::new(SOCKET_SOFT_CAP_START);

/// Grow the soft cap by 20 % (at least one slot), clamped to `MAX_SOCKETS`.
///
/// Called only when reclamation has already failed to free anything, i.e. the
/// table is full of genuinely live connections rather than `TimeWait` corpses.
/// That distinction is the whole design: growing on `TimeWait` pressure would
/// walk the cap straight up to the ceiling and inherit the 2048-slot result
/// (45 us per poll), whereas growing on *live* pressure trades a slightly more
/// expensive poll for connections that would otherwise be refused.
///
/// Returns the new cap.
#[cfg(feature = "smoltcp")]
fn grow_soft_cap() -> usize {
    let cur = SOCKET_SOFT_CAP.load(Ordering::Relaxed);
    if cur >= MAX_SOCKETS {
        return cur;
    }
    let next = (cur + cur / 5).max(cur + 1).min(MAX_SOCKETS);
    SOCKET_SOFT_CAP.store(next, Ordering::Relaxed);
    next
}

/// Live entries in the smoltcp `SocketSet`, maintained incrementally.
///
/// `SocketSet::iter().count()` is O(set) and there is no cheaper accessor, so
/// every "how full are we" question used to walk the whole table — up to three
/// times per `socket_create`, plus once per `poll()` for the profiler. At 128
/// slots and ~90,000 polls/s that is over 10M iterations/s inside the `NETWORK`
/// lock with IRQs masked; at 2048 slots it distorted the measurement it was
/// taken for. Maintained here instead: `+1` on `add`, `-1` on `remove`, read as
/// a single atomic load.
#[cfg(feature = "smoltcp")]
pub(crate) static SOCKETS_LIVE: AtomicUsize = AtomicUsize::new(0);

/// Live socket count. O(1) — see [`SOCKETS_LIVE`].
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn sockets_live() -> usize {
    SOCKETS_LIVE.load(Ordering::Relaxed)
}

/// The enforced table size right now. Diagnostic + the `[NICSTAT]` dump.
#[cfg(feature = "smoltcp")]
#[must_use]
pub fn socket_soft_cap() -> usize {
    SOCKET_SOFT_CAP.load(Ordering::Relaxed)
}

/// Sockets reclaimed early by [`reclaim_pending_slots`]. A steadily climbing
/// value means the socket budget is genuinely too small for the workload rather
/// than merely churning; a flat zero under connection-per-request load means the
/// valve is not being reached.
static RECLAIMED_SLOTS: AtomicU64 = AtomicU64::new(0);

/// How many socket slots have been reclaimed under pressure since boot.
#[must_use]
pub fn reclaimed_slot_count() -> u64 {
    RECLAIMED_SLOTS.load(Ordering::Relaxed)
}

#[must_use]
pub fn socket_create() -> Option<SocketHandle> {
    with_network(|net| {
        let mut cap = SOCKET_SOFT_CAP.load(Ordering::Relaxed);
        if sockets_live() >= cap {
            // Pressure valve before giving up: a full table under
            // connection-per-request traffic is usually full of `TimeWait`,
            // not of live connections. Reclaim a small batch rather than the
            // one slot needed — the scan is O(pending) and the caller here is
            // the listener refilling its backlog, so it is about to ask again
            // several more times.
            let freed = reclaim_pending_slots(net, RECLAIM_BATCH);
            if sockets_live() >= cap {
                // Reclamation could not help: the table is full of LIVE
                // connections, not corpses. Widen rather than refuse — see
                // `grow_soft_cap` for why this is gated on `freed == 0` and not
                // simply on being full.
                if freed == 0 {
                    cap = grow_soft_cap();
                }
                if sockets_live() >= cap {
                    return None;
                }
            }
        }
        let rx_buffer = tcp::SocketBuffer::new(vec![0; TCP_RX_BUFFER_SIZE]);
        let tx_buffer = tcp::SocketBuffer::new(vec![0; TCP_TX_BUFFER_SIZE]);
        let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
        socket.set_nagle_enabled(false);
        // Delayed ACK, so a request/response exchange costs ONE transmit
        // instead of two.
        //
        // With `None`, smoltcp ACKs the request the moment it lands — before
        // the server has produced a reply — so every round trip emits a bare
        // ACK *and* the response. Measured on redis PING at every core count:
        // exactly 1.97-2.00 tx packets per rx packet. Each transmit costs
        // ~14.9 us of `add_notify_wait_pop` spin inside the `NETWORK` lock
        // (`virtio_rings.rs` header), so the duplicate is ~15 us of the 69 us
        // serialized budget that sets the throughput ceiling.
        //
        // The old comment here justified `None` as avoiding a ~65KB/10ms
        // throttle on receive-heavy workloads. That does not apply to smoltcp
        // 0.12: `immediate_ack_to_transmit()` forces an immediate ACK once one
        // full MSS of unacked data has arrived (the Linux rule), and
        // `window_to_update()` forces one whenever the receive window doubles.
        // Bulk receive therefore still ACKs per segment; only the sub-MSS
        // request/response case waits, which is exactly the case that wants to
        // piggyback. The timer never actually expires here — redis replies in
        // ~100 us, far inside the delay — so this costs no latency.
        socket.set_ack_delay(Some(smoltcp::time::Duration::from_millis(10)));
        SOCKETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Some(net.sockets.add(socket))
    }).flatten()
}
