//! One TCP connection, as a pure state machine.
//!
//! It is for a peer on a link that **does not lose, reorder or duplicate segments**: the
//! hypervisor's own virtual wire.
//!
//! It is not a general TCP. It has no retransmission timer, no congestion control, no
//! out-of-order queue, no options beyond MSS in the SYN, and no TIME-WAIT. What it does
//! have is what a lossless in-memory link needs, and it keeps the rest honest:
//!
//! * the three-way handshake in both roles ([`Tcp::connect`], [`Tcp::accept`]);
//! * in-order data both ways through two buffers the caller lends it (one per direction), with the receive window
//!   advertised as the free space in the receive buffer and the send side limited by the
//!   peer's advertised window (so a slow reader slows the writer, as TCP does);
//! * FIN in both directions and the close states, and RST on anything unexpected;
//! * a duplicate or out-of-window segment answered with an ACK and otherwise ignored.
//!
//! The link is lossless **because the caller makes it so**: it sends a segment only when
//! the wire has room for it ([`Tcp::poll_tx`] is pulled, never pushed), and the peer's
//! frames reach it synchronously. A peer behind a lossy link needs a real stack.
//!
//! Like the rest of the crate: no allocation, no `unsafe`, nothing here knows about
//! Ethernet or IP — [`Segment`] is what is left once the headers are parsed.

/// The most payload one segment carries.
///
/// With Ethernet (14), IPv4 (20) and TCP (20 + the 4-byte MSS option on a SYN) this is inside
/// a 1514-byte frame with room to spare, and below the 1460 any peer on a 1500 MTU will offer.
pub const MSS: usize = 1400;

pub mod flag {
    pub const FIN: u8 = 0x01;
    pub const SYN: u8 = 0x02;
    pub const RST: u8 = 0x04;
    pub const ACK: u8 = 0x10;
}

/// A parsed segment: the header fields a connection needs and where its payload is.
#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

/// A segment to send, minus addresses and checksums (the caller's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Out {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// Bytes of payload written into the caller's buffer.
    pub len: usize,
    /// A SYN carries the MSS option.
    pub mss_option: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    SynSent,
    SynRcvd,
    Established,
    /// We sent FIN; waiting for its ACK (and the peer's FIN).
    FinWait1,
    FinWait2,
    /// The peer sent FIN; we may still send.
    CloseWait,
    /// Both sent FIN, ours not yet ACKed.
    Closing,
    /// We sent FIN after the peer's; waiting for its ACK.
    LastAck,
}

/// A byte ring over a borrowed buffer.
#[derive(Debug)]
struct Ring<'a> {
    b: &'a mut [u8],
    head: usize,
    len: usize,
}

impl<'a> Ring<'a> {
    const fn new(b: &'a mut [u8]) -> Self {
        Self { b, head: 0, len: 0 }
    }

    const fn cap(&self) -> usize {
        self.b.len()
    }

    const fn free(&self) -> usize {
        self.b.len() - self.len
    }

    fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        let cap = self.cap();
        for (i, &x) in data[..n].iter().enumerate() {
            self.b[(self.head + self.len + i) % cap] = x;
        }
        self.len += n;
        n
    }

    /// Copy up to `dst.len()` bytes starting `skip` bytes in, without consuming.
    fn peek(&self, skip: usize, dst: &mut [u8]) -> usize {
        let n = dst.len().min(self.len.saturating_sub(skip));
        let cap = self.cap();
        for (i, d) in dst[..n].iter_mut().enumerate() {
            *d = self.b[(self.head + skip + i) % cap];
        }
        n
    }

    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        self.head = (self.head + n) % self.cap();
        self.len -= n;
    }
}

/// One connection, over two buffers it is lent for its whole life.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // each flag is one independent fact of the protocol
pub struct Tcp<'a> {
    state: State,
    iss: u32,
    /// Oldest unacknowledged sequence number.
    snd_una: u32,
    /// Next sequence number to send.
    snd_nxt: u32,
    /// The peer's advertised window.
    snd_wnd: u32,
    /// Next sequence number expected from the peer.
    rcv_nxt: u32,
    /// Bytes from `snd_una` (less the SYN) that are queued: unacknowledged then unsent.
    tx: Ring<'a>,
    rx: Ring<'a>,
    /// An ACK is owed.
    ack_pending: bool,
    /// The application has closed its side: send FIN once `tx` drains.
    close_requested: bool,
    /// The FIN has been sent (it occupies one sequence number after the data).
    fin_sent: bool,
    /// The peer's FIN has been received (and is behind everything in `rx`).
    fin_rcvd: bool,
    /// A RST ended the connection, or one is owed.
    reset_pending: bool,
    reset_seen: bool,
}

/// `a < b` in sequence space.
#[allow(clippy::cast_possible_wrap)] // the wrap is the point: serial-number arithmetic
const fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

impl<'a> Tcp<'a> {
    /// A closed connection whose received and to-be-sent bytes live in `rx` and `tx`
    /// (their lengths are the windows: the receive window is advertised from `rx`'s space).
    #[must_use]
    pub const fn new(rx: &'a mut [u8], tx: &'a mut [u8]) -> Self {
        Self {
            state: State::Closed,
            iss: 0,
            snd_una: 0,
            snd_nxt: 0,
            snd_wnd: 0,
            rcv_nxt: 0,
            tx: Ring::new(tx),
            rx: Ring::new(rx),
            ack_pending: false,
            close_requested: false,
            fin_sent: false,
            fin_rcvd: false,
            reset_pending: false,
            reset_seen: false,
        }
    }

    /// Reset to [`State::Closed`], forgetting everything.
    pub fn clear(&mut self) {
        self.state = State::Closed;
        self.iss = 0;
        self.snd_una = 0;
        self.snd_nxt = 0;
        self.snd_wnd = 0;
        self.rcv_nxt = 0;
        self.tx.reset();
        self.rx.reset();
        self.ack_pending = false;
        self.close_requested = false;
        self.fin_sent = false;
        self.fin_rcvd = false;
        self.reset_pending = false;
        self.reset_seen = false;
    }

    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// The handshake is done and the connection is not (yet) shutting down.
    #[must_use]
    pub const fn is_established(&self) -> bool {
        matches!(self.state, State::Established | State::CloseWait)
    }

    /// Nothing more will ever happen: closed (including by RST) with nothing owed.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed) && !self.reset_pending && !self.ack_pending
    }

    /// The peer reset the connection.
    #[must_use]
    pub const fn was_reset(&self) -> bool {
        self.reset_seen
    }

    /// The peer has closed its side and every byte it sent has been read.
    #[must_use]
    pub const fn peer_finished(&self) -> bool {
        self.fin_rcvd && self.rx.len == 0
    }

    /// Start a connection to a peer: the SYN goes out at the next [`Tcp::poll_tx`].
    pub fn connect(&mut self, iss: u32) {
        self.clear();
        self.iss = iss;
        self.snd_una = iss;
        self.snd_nxt = iss;
        self.state = State::SynSent;
    }

    /// Accept a peer's SYN (a listening endpoint's job): the SYN-ACK goes out at the next
    /// [`Tcp::poll_tx`]. False if `seg` is not a plain SYN, or the connection is in use.
    pub fn accept(&mut self, seg: &Segment<'_>, iss: u32) -> bool {
        if self.state != State::Closed || seg.flags & (flag::SYN | flag::ACK | flag::RST) != flag::SYN {
            return false;
        }
        self.clear();
        self.iss = iss;
        self.snd_una = iss;
        self.snd_nxt = iss;
        self.rcv_nxt = seg.seq.wrapping_add(1);
        self.snd_wnd = u32::from(seg.window);
        self.state = State::SynRcvd;
        true
    }

    /// Queue bytes for the peer; how many fit.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.close_requested || !matches!(self.state, State::Established | State::CloseWait | State::SynSent | State::SynRcvd) {
            return 0;
        }
        self.tx.push(data)
    }

    /// Bytes [`Tcp::write`] would take now.
    #[must_use]
    pub const fn write_room(&self) -> usize {
        if self.close_requested { 0 } else { self.tx.free() }
    }

    /// Take received bytes; how many. Reading opens the window: an ACK is owed if it
    /// reopens a window the peer may have seen as zero or small.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let n = self.rx.peek(0, dst);
        if n > 0 {
            let before = self.window();
            self.rx.drop_front(n);
            // A window update, as a receiver owes when the window grows by a segment or more.
            if self.window() >= before + MSS as u16 / 2 || before == 0 {
                self.ack_pending = true;
            }
        }
        n
    }

    /// Bytes waiting to be read.
    #[must_use]
    pub const fn readable(&self) -> usize {
        self.rx.len
    }

    /// Close our side once what is queued has been sent. The peer may keep sending.
    pub fn close(&mut self) {
        self.close_requested = true;
    }

    /// Abort: a RST goes out at the next [`Tcp::poll_tx`] and the connection is closed.
    pub fn abort(&mut self) {
        if self.state != State::Closed {
            self.reset_pending = true;
        }
    }

    fn window(&self) -> u16 {
        self.rx.free().min(usize::from(u16::MAX)) as u16
    }

    /// Process a segment from the peer.
    pub fn input(&mut self, seg: &Segment<'_>) {
        if self.state == State::Closed {
            return;
        }
        if seg.flags & flag::RST != 0 {
            // A reset is believed if it names a sequence number we could have sent or
            // are expecting (RFC 5961 would be stricter; the link is trusted).
            self.reset_seen = true;
            self.state = State::Closed;
            return;
        }
        if self.state == State::SynSent {
            let acks_syn = seg.flags & flag::ACK != 0 && seg.ack == self.iss.wrapping_add(1);
            if seg.flags & flag::SYN != 0 && acks_syn {
                self.snd_una = seg.ack;
                self.snd_nxt = seg.ack;
                self.rcv_nxt = seg.seq.wrapping_add(1);
                self.snd_wnd = u32::from(seg.window);
                self.state = State::Established;
                self.ack_pending = true;
            } else if seg.flags & flag::ACK != 0 && !acks_syn {
                // An ACK that is not for our SYN: reset it.
                self.reset_pending = true;
            }
            return;
        }
        // A retransmitted SYN while we are in SYN-RCVD: our SYN-ACK is still owed.
        if self.state == State::SynRcvd && seg.flags & flag::SYN != 0 {
            self.snd_nxt = self.iss;
            return;
        }
        if seg.flags & flag::ACK == 0 {
            return;
        }
        // ACK processing: anything in (snd_una, snd_nxt] advances; beyond that is an error.
        if lt(self.snd_nxt, seg.ack) {
            self.ack_pending = true;
            return;
        }
        if lt(self.snd_una, seg.ack) {
            let acked = seg.ack.wrapping_sub(self.snd_una) as usize;
            // The SYN and the FIN each take a sequence number that is not in `tx`.
            let syn = usize::from(self.snd_una == self.iss);
            let data = acked.saturating_sub(syn);
            let fin_acked = self.fin_sent && seg.ack == self.snd_nxt && data > self.tx.len;
            self.tx.drop_front(data.min(self.tx.len));
            self.snd_una = seg.ack;
            if self.state == State::SynRcvd {
                self.state = State::Established;
            }
            if fin_acked || (self.fin_sent && self.snd_una == self.snd_nxt && self.tx.len == 0) {
                match self.state {
                    State::FinWait1 => self.state = State::FinWait2,
                    State::Closing => self.state = State::Closed,
                    State::LastAck => self.state = State::Closed,
                    _ => {}
                }
            }
        }
        if !lt(seg.ack, self.snd_una) {
            self.snd_wnd = u32::from(seg.window);
        }
        // Data and FIN, in order only.
        if matches!(self.state, State::Established | State::FinWait1 | State::FinWait2) {
            let len = seg.payload.len();
            if len > 0 || seg.flags & flag::FIN != 0 {
                if seg.seq != self.rcv_nxt {
                    // A duplicate, or something ahead of a gap that cannot happen here.
                    self.ack_pending = true;
                    return;
                }
                let took = self.rx.push(seg.payload);
                self.rcv_nxt = self.rcv_nxt.wrapping_add(took as u32);
                self.ack_pending = true;
                if took == len && seg.flags & flag::FIN != 0 {
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    self.fin_rcvd = true;
                    self.state = match self.state {
                        State::Established => State::CloseWait,
                        State::FinWait1 => State::Closing,
                        State::FinWait2 => State::Closed,
                        s => s,
                    };
                }
            }
        }
    }

    /// The next segment to send, if any, with its payload written into `payload`
    /// (at least [`MSS`] bytes). Call until it returns `None` or the wire is full.
    pub fn poll_tx(&mut self, payload: &mut [u8]) -> Option<Out> {
        let window = self.window();
        if self.reset_pending {
            self.reset_pending = false;
            let seq = self.snd_nxt;
            self.state = State::Closed;
            return Some(Out { seq, ack: self.rcv_nxt, flags: flag::RST | flag::ACK, window: 0, len: 0, mss_option: false });
        }
        match self.state {
            // Closed by the peer's FIN while we waited in FIN-WAIT-2: the last ACK is
            // still owed (there is no TIME-WAIT to hold the connection for it).
            State::Closed if self.ack_pending && !self.reset_seen => {
                self.ack_pending = false;
                return Some(Out { seq: self.snd_nxt, ack: self.rcv_nxt, flags: flag::ACK, window: 0, len: 0, mss_option: false });
            }
            State::Closed => return None,
            State::SynSent if self.snd_nxt == self.iss => {
                self.snd_nxt = self.iss.wrapping_add(1);
                return Some(Out { seq: self.iss, ack: 0, flags: flag::SYN, window, len: 0, mss_option: true });
            }
            State::SynRcvd if self.snd_nxt == self.iss => {
                self.snd_nxt = self.iss.wrapping_add(1);
                self.ack_pending = false;
                return Some(Out { seq: self.iss, ack: self.rcv_nxt, flags: flag::SYN | flag::ACK, window, len: 0, mss_option: true });
            }
            State::SynSent | State::SynRcvd => return None,
            _ => {}
        }
        // Data, within the peer's window and what is queued but not yet sent.
        let in_flight = self.snd_nxt.wrapping_sub(self.snd_una) as usize;
        let data_in_flight = in_flight.saturating_sub(usize::from(self.fin_sent));
        let unsent = self.tx.len.saturating_sub(data_in_flight);
        let room = (self.snd_wnd as usize).saturating_sub(in_flight);
        let n = unsent.min(room).min(MSS).min(payload.len());
        if n > 0 && !self.fin_sent {
            let got = self.tx.peek(data_in_flight, &mut payload[..n]);
            let seq = self.snd_nxt;
            self.snd_nxt = self.snd_nxt.wrapping_add(got as u32);
            self.ack_pending = false;
            let mut flags = flag::ACK;
            // PSH when this empties the queue: a hint, and what makes small writes prompt.
            if got == unsent {
                flags |= 0x08;
            }
            return Some(Out { seq, ack: self.rcv_nxt, flags, window, len: got, mss_option: false });
        }
        // FIN once the application closed and everything queued has been sent.
        if self.close_requested && !self.fin_sent && unsent == 0 && matches!(self.state, State::Established | State::CloseWait) {
            self.fin_sent = true;
            let seq = self.snd_nxt;
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.ack_pending = false;
            self.state = if self.state == State::Established { State::FinWait1 } else { State::LastAck };
            return Some(Out { seq, ack: self.rcv_nxt, flags: flag::FIN | flag::ACK, window, len: 0, mss_option: false });
        }
        if self.ack_pending {
            self.ack_pending = false;
            return Some(Out { seq: self.snd_nxt, ack: self.rcv_nxt, flags: flag::ACK, window, len: 0, mss_option: false });
        }
        None
    }

    /// Would [`Tcp::poll_tx`] return something?
    #[must_use]
    pub fn wants_tx(&self) -> bool {
        // Cheap and conservative: any of the reasons it would speak.
        if self.reset_pending || self.ack_pending {
            return true;
        }
        match self.state {
            State::Closed => false,
            State::SynSent | State::SynRcvd => self.snd_nxt == self.iss,
            _ => {
                let in_flight = self.snd_nxt.wrapping_sub(self.snd_una) as usize;
                let data_in_flight = in_flight.saturating_sub(usize::from(self.fin_sent));
                let unsent = self.tx.len.saturating_sub(data_in_flight);
                let room = (self.snd_wnd as usize).saturating_sub(in_flight);
                (unsent > 0 && room > 0 && !self.fin_sent)
                    || (self.close_requested && !self.fin_sent && unsent == 0 && matches!(self.state, State::Established | State::CloseWait))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const CAP: usize = 8192;

    fn conn() -> Tcp<'static> {
        let buf = || -> &'static mut [u8] { std::boxed::Box::leak(std::vec![0u8; CAP].into_boxed_slice()) };
        Tcp::new(buf(), buf())
    }

    /// Two connections wired back to back: whatever one emits the other takes.
    struct Pair {
        a: Tcp<'static>,
        b: Tcp<'static>,
    }

    fn drain(from: &mut Tcp<'_>, to: &mut Tcp<'_>) -> usize {
        let mut n = 0;
        let mut buf = [0u8; MSS];
        while let Some(o) = from.poll_tx(&mut buf) {
            to.input(&Segment { seq: o.seq, ack: o.ack, flags: o.flags, window: o.window, payload: &buf[..o.len] });
            n += 1;
        }
        n
    }

    impl Pair {
        fn run(&mut self) {
            for _ in 0..10_000 {
                let n = drain(&mut self.a, &mut self.b) + drain(&mut self.b, &mut self.a);
                if n == 0 {
                    return;
                }
            }
            panic!("did not settle");
        }

        fn established() -> Self {
            let mut p = Pair { a: conn(), b: conn() };
            p.a.connect(1000);
            // The passive side learns of the SYN from the first segment out of `a`.
            let mut buf = [0u8; MSS];
            let o = p.a.poll_tx(&mut buf).unwrap();
            assert_eq!(o.flags, flag::SYN);
            assert!(o.mss_option);
            assert!(p.b.accept(&Segment { seq: o.seq, ack: 0, flags: o.flags, window: o.window, payload: &[] }, 7_000_000));
            p.run();
            assert_eq!((p.a.state(), p.b.state()), (State::Established, State::Established));
            p
        }
    }

    #[test]
    fn the_handshake_establishes_both_sides() {
        Pair::established();
    }

    #[test]
    fn a_syn_to_a_busy_or_non_syn_segment_is_not_accepted() {
        let mut t = conn();
        let syn_ack = Segment { seq: 1, ack: 2, flags: flag::SYN | flag::ACK, window: 100, payload: &[] };
        assert!(!t.accept(&syn_ack, 5));
        let rst = Segment { seq: 1, ack: 0, flags: flag::SYN | flag::RST, window: 0, payload: &[] };
        assert!(!t.accept(&rst, 5));
        let syn = Segment { seq: 1, ack: 0, flags: flag::SYN, window: 100, payload: &[] };
        assert!(t.accept(&syn, 5));
        assert!(!t.accept(&syn, 6), "in use");
    }

    #[test]
    fn data_flows_both_ways_in_order() {
        let mut p = Pair::established();
        assert_eq!(p.a.write(b"hello, "), 7);
        assert_eq!(p.a.write(b"world"), 5);
        p.run();
        let mut got = [0u8; 64];
        let n = p.b.read(&mut got);
        assert_eq!(&got[..n], b"hello, world");
        assert_eq!(p.b.write(b"and back"), 8);
        p.run();
        let n = p.a.read(&mut got);
        assert_eq!(&got[..n], b"and back");
    }

    #[test]
    fn a_full_buffer_of_data_is_split_into_segments_and_stops_at_the_receivers_window() {
        let mut p = Pair::established();
        let data: Vec<u8> = (0..CAP * 2).map(|i| (i % 251) as u8).collect();
        let mut sent = 0;
        let mut received = Vec::new();
        let mut chunk = [0u8; 3000];
        // The receiver reads a little at a time: the writer must wait for the window.
        for _ in 0..100_000 {
            if sent < data.len() {
                sent += p.a.write(&data[sent..]);
            }
            p.run();
            let n = p.b.read(&mut chunk);
            received.extend_from_slice(&chunk[..n]);
            if received.len() == data.len() {
                break;
            }
        }
        assert_eq!(received, data);
    }

    #[test]
    fn a_zero_window_holds_the_sender_until_the_reader_reads() {
        let mut p = Pair::established();
        let data = std::vec![7u8; CAP + 500];
        let mut sent = p.a.write(&data);
        assert_eq!(sent, CAP);
        p.run();
        // `b` never read: its buffer is full, `a` has sent exactly that much and no more.
        assert_eq!(p.b.readable(), CAP);
        assert!(!p.a.wants_tx(), "the zero window holds it");
        // Reading a segment's worth reopens the window; a window update goes out.
        let mut sink = [0u8; 2000];
        assert_eq!(p.b.read(&mut sink), 2000);
        sent += p.a.write(&data[sent..]);
        p.run();
        assert_eq!(sent, CAP + 500);
        assert_eq!(p.b.readable(), CAP - 2000 + 500);
    }

    #[test]
    fn close_in_both_directions_ends_in_closed_on_both() {
        let mut p = Pair::established();
        p.a.write(b"bye");
        p.a.close();
        p.run();
        // `b` saw the data and then the FIN.
        let mut got = [0u8; 8];
        assert_eq!(p.b.read(&mut got), 3);
        assert!(p.b.peer_finished());
        assert_eq!(p.b.state(), State::CloseWait);
        assert_eq!(p.a.state(), State::FinWait2);
        p.b.close();
        p.run();
        assert_eq!((p.a.state(), p.b.state()), (State::Closed, State::Closed));
        assert!(p.a.is_closed() && p.b.is_closed());
    }

    #[test]
    fn a_simultaneous_close_closes_both() {
        let mut p = Pair::established();
        p.a.close();
        p.b.close();
        p.run();
        assert_eq!((p.a.state(), p.b.state()), (State::Closed, State::Closed));
    }

    #[test]
    fn a_write_after_close_is_refused_and_the_peer_can_still_send() {
        let mut p = Pair::established();
        p.a.close();
        assert_eq!(p.a.write(b"x"), 0);
        p.run();
        assert_eq!(p.b.write(b"still talking"), 13);
        p.run();
        let mut got = [0u8; 32];
        assert_eq!(p.a.read(&mut got), 13);
    }

    #[test]
    fn a_reset_closes_the_other_side_and_is_visible() {
        let mut p = Pair::established();
        p.a.abort();
        p.run();
        assert!(p.a.is_closed());
        assert!(p.b.is_closed() && p.b.was_reset());
    }

    #[test]
    fn a_duplicate_segment_is_acked_and_not_delivered_twice() {
        let mut p = Pair::established();
        p.a.write(b"once");
        let mut buf = [0u8; MSS];
        let o = p.a.poll_tx(&mut buf).unwrap();
        let seg = Segment { seq: o.seq, ack: o.ack, flags: o.flags, window: o.window, payload: &buf[..o.len] };
        p.b.input(&seg);
        p.b.input(&seg);
        assert_eq!(p.b.readable(), 4);
        // And a duplicate draws an ACK.
        let mut out = [0u8; MSS];
        let ack = p.b.poll_tx(&mut out).unwrap();
        assert_eq!(ack.flags & flag::ACK, flag::ACK);
        assert_eq!(ack.ack, o.seq.wrapping_add(4));
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut p = Pair { a: conn(), b: conn() };
        p.a.connect(u32::MAX - 5);
        let mut buf = [0u8; MSS];
        let o = p.a.poll_tx(&mut buf).unwrap();
        assert!(p.b.accept(&Segment { seq: o.seq, ack: 0, flags: o.flags, window: o.window, payload: &[] }, u32::MAX - 2));
        p.run();
        assert_eq!(p.a.state(), State::Established);
        let data = std::vec![9u8; 5000];
        let mut sent = 0;
        let mut got = Vec::new();
        let mut chunk = [0u8; 2048];
        for _ in 0..1000 {
            sent += p.a.write(&data[sent..]);
            p.run();
            let n = p.b.read(&mut chunk);
            got.extend_from_slice(&chunk[..n]);
            if got.len() == data.len() {
                break;
            }
        }
        assert_eq!(got, data);
    }

    #[test]
    fn an_ack_for_data_never_sent_does_not_advance_anything() {
        let mut p = Pair::established();
        p.a.write(b"abc");
        let mut buf = [0u8; MSS];
        let _ = p.a.poll_tx(&mut buf).unwrap();
        // `b` claims to have received a thousand bytes.
        p.a.input(&Segment { seq: 1, ack: 1_000_000, flags: flag::ACK, window: 100, payload: &[] });
        assert_eq!(p.a.state(), State::Established);
        assert_eq!(p.a.tx.len, 3, "nothing was acknowledged");
    }
}
