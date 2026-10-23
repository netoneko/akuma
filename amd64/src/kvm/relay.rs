//! The host end of a guest's port forward: a listening socket on the host's own network
//! stack whose clients are connected through to a port on the guest, over the hypervisor's
//! virtual wire (`akuma_svm_amd64::gateway`'s forward slots).
//!
//! `ssh -p 2223 root@<host>` reaches the guest's sshd this way, from the network or from
//! the host itself. Every call here is non-blocking: it is polled from the guest's run
//! loop, between `VMRUN`s, and a blocked relay would be a stopped guest.
//!
//! Each accepted connection takes a forward slot ([`FWD_SLOTS`] of them) and gets bytes
//! moved in both directions a chunk at a time; an orderly close either way is passed on
//! as a `shutdown(SHUT_WR)` / FIN, an error or reset tears both ends down. The host's
//! `socket_*` calls drive the host stack themselves (`socket_recv` polls it), so this needs
//! no daemon of its own.
//!
//! **Outbound** (the spec's `vm_outbound`): the other direction. A connection the *guest* opens
//! to an address outside its own subnet is held by the gateway ([`Gateway::out_take_request`]);
//! the relay makes the same connection from the host's stack (a non-blocking `connect`) and
//! answers the guest through [`Gateway::out_accept`] or [`Gateway::out_refuse`], after which the
//! bytes move exactly as for a forward. DNS queries the gateway queues for its proxy are sent,
//! verbatim, to the host's own resolver over one UDP socket and the answers matched back by id.
//! The guest's connections are the host's connections: they leave from the host's address, so
//! anything the host can reach, a guest with `vm_outbound` can reach (loopback excepted).
//!
//! No allocation on the forward path: a chunk buffer per direction on the stack, one for each
//! slot's guest-to-host leftovers. The outbound slots (eight more of them) are allocated once,
//! fallibly, when outbound is first polled: the relay is built by value on a small task stack.

use akuma_net::socket::{self, socket_const::{SOCK_DGRAM, SOCK_STREAM}, SocketAddrV4};
use akuma_svm_amd64::gateway::{Gateway, OutPhase, DNS_SLOTS, FWD_SLOTS, OUT_SLOTS};
use akuma_svm_amd64::tcp::Tcp;
use alloc::vec::Vec;
use akuma_vm_spec::{spec::MAX_FORWARDS, Forward};

/// `EAGAIN`, as the host socket layer reports it (a positive errno).
const EAGAIN: i32 = 11;
/// `shutdown(SHUT_WR)`.
const SHUT_WR: i32 = 1;
/// Bytes moved per direction per round.
const CHUNK: usize = 2048;
/// Rounds per [`Relay::poll`] while bytes keep moving. A poll is rare (the run loop sleeps
/// between host interrupts), so one chunk per poll capped the relay at `CHUNK` x (wakes per
/// second): ~100 KB/s on a 100 Hz tick.
const ROUNDS: usize = 16;
/// How long to wait before retrying a listener the host stack refused (µs).
const RETRY_US: u64 = 1_000_000;
/// How long a SYN may go unanswered before it is sent again, from a new source port (µs).
/// The gateway's TCP sends it once and has no timer. Measured on the ryzen (2026-10-22): every SYN
/// reaches the guest and is answered, but a busy one-vCPU guest can take seconds (its netpoll waits
/// behind a key exchange), so this is generous: a retry costs the guest a half-open socket. What
/// wedged for good was a reused 4-tuple, which the source-port rotation removes.
const SYN_RETRY_US: u64 = 4_000_000;
/// SYNs sent per connection before the client is refused (reset) and the slot freed.
const SYN_TRIES: u8 = 4;
/// `connect(2)` reports a non-blocking dial in flight as `EINPROGRESS`.
const EINPROGRESS: i32 = 115;
/// How long the host may take to make an outbound connection before the guest is refused (µs).
const OUT_CONNECT_US: u64 = 15_000_000;
/// How long a forwarded DNS query waits for its answer before it is dropped (the guest retries) (µs).
const DNS_WAIT_US: u64 = 5_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// The gateway has sent its SYN to the guest.
    Connecting,
    Open,
}

/// The bytes in flight between one guest connection and its host socket.
struct Pipe {
    /// The host peer closed its side (we saw EOF): the guest has been told with a FIN.
    host_eof: bool,
    /// The guest closed its side and everything it sent has been delivered: the host
    /// peer has been told with `shutdown(SHUT_WR)`.
    guest_eof_passed: bool,
    /// Guest-to-host bytes read from the guest but not yet accepted by the host socket.
    up: [u8; CHUNK],
    up_off: usize,
    up_len: usize,
}

impl Pipe {
    const fn new() -> Self {
        Self { host_eof: false, guest_eof_passed: false, up: [0; CHUNK], up_off: 0, up_len: 0 }
    }
}

struct Slot {
    sock: usize,
    phase: Phase,
    pipe: Pipe,
    /// When the SYN last went out, and how many have.
    syn_at_us: u64,
    syn_tries: u8,
}

impl Slot {
    const fn new() -> Self {
        Self { sock: 0, phase: Phase::Idle, pipe: Pipe::new(), syn_at_us: 0, syn_tries: 0 }
    }
}

/// One connection the guest opened outward.
struct OutSlot {
    sock: usize,
    phase: Phase,
    pipe: Pipe,
    /// When the host's dial began, and to where (for the log).
    started_us: u64,
    dst: [u8; 4],
    dport: u16,
}

impl OutSlot {
    const fn new() -> Self {
        Self { sock: 0, phase: Phase::Idle, pipe: Pipe::new(), started_us: 0, dst: [0; 4], dport: 0 }
    }
}


/// What the relay has done, for the end-of-run summary.
#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub accepted: u64,
    pub to_guest: u64,
    pub to_host: u64,
    pub refused: u64,
    /// Times the host socket had no room (or no data) when the relay asked.
    pub send_blocked: u64,
    pub recv_idle: u64,
    /// SYNs sent again after [`SYN_RETRY_US`] of silence, and connections given up on.
    pub syn_retries: u64,
    pub syn_gave_up: u64,
    /// The slowest SYN-to-established time seen (µs): slow answers or lost SYNs?
    pub connect_us_max: u64,
    /// Outbound: connections the host made for the guest, ones it could not (refused, timed out,
    /// no socket), ones denied by policy, and the bytes moved each way.
    pub out_opened: u64,
    pub out_failed: u64,
    pub out_denied: u64,
    pub out_to_guest: u64,
    pub out_to_host: u64,
    /// DNS queries forwarded to the host's resolver, answers handed back, queries that timed out.
    pub dns_fwd: u64,
    pub dns_answered: u64,
    pub dns_timeouts: u64,
}

pub struct Relay {
    /// The forwards, in the order the spec gave them; each has its own host listener, and
    /// all of them share the gateway's [`FWD_SLOTS`] connection slots.
    fwds: [Forward; MAX_FORWARDS],
    n_fwds: usize,
    listeners: [Option<usize>; MAX_FORWARDS],
    retry_at_us: u64,
    slots: [Slot; FWD_SLOTS],
    /// The guest port each live slot's connection goes to.
    slot_guest: [u16; FWD_SLOTS],
    /// The spec allows the guest out (`vm_outbound`); the slots below exist once it is polled.
    outbound: bool,
    out: Vec<OutSlot>,
    /// The UDP socket DNS queries leave from, and what each gateway DNS slot is waiting on:
    /// the query's id and when it went out.
    dns_sock: Option<usize>,
    dns_wait: [Option<(u16, u64)>; DNS_SLOTS],
    pub counts: Counts,
}

impl Relay {
    /// A relay for `forwards` (at most [`MAX_FORWARDS`]); nothing is bound until the first
    /// [`Relay::poll`].
    pub const fn new(forwards: &[Forward], outbound: bool) -> Self {
        let mut fwds = [Forward { host: 0, guest: 0 }; MAX_FORWARDS];
        let mut i = 0;
        while i < forwards.len() && i < MAX_FORWARDS {
            fwds[i] = forwards[i];
            i += 1;
        }
        Self { fwds, n_fwds: i, listeners: [None; MAX_FORWARDS], retry_at_us: 0, slots: [const { Slot::new() }; FWD_SLOTS], slot_guest: [0; FWD_SLOTS], outbound, out: Vec::new(), dns_sock: None, dns_wait: [None; DNS_SLOTS], counts: Counts { accepted: 0, to_guest: 0, to_host: 0, refused: 0, send_blocked: 0, recv_idle: 0, syn_retries: 0, syn_gave_up: 0, connect_us_max: 0, out_opened: 0, out_failed: 0, out_denied: 0, out_to_guest: 0, out_to_host: 0, dns_fwd: 0, dns_answered: 0, dns_timeouts: 0 } }
    }

    /// Is every forward's listener up?
    pub fn listening(&self) -> bool {
        self.listeners[..self.n_fwds].iter().all(Option::is_some)
    }

    fn ensure_listeners(&mut self, now_us: u64) {
        if self.listening() || now_us < self.retry_at_us {
            return;
        }
        self.retry_at_us = now_us + RETRY_US;
        for k in 0..self.n_fwds {
            if self.listeners[k].is_some() {
                continue;
            }
            let Some(idx) = socket::alloc_socket(SOCK_STREAM) else { return };
            let up = socket::socket_bind(idx, SocketAddrV4::new([0; 4], self.fwds[k].host)).is_ok() && socket::socket_listen(idx, 4).is_ok();
            if up {
                self.listeners[k] = Some(idx);
                super::p("[KVM] relay: listening on host port ");
                super::dec(u64::from(self.fwds[k].host));
                super::p("\n");
            } else {
                socket::remove_socket(idx);
            }
        }
    }

    /// Release every host socket (listeners and live connections) and the gateway's slots, so
    /// a restarted guest can bind the same ports.
    pub fn close(&mut self, gw: &mut Gateway<'_>) {
        for i in 0..FWD_SLOTS {
            if self.slots[i].phase != Phase::Idle {
                self.teardown(i, gw);
            }
        }
        for i in 0..self.out.len() {
            if self.out[i].phase != Phase::Idle {
                self.out_teardown(i, gw);
            }
        }
        for i in 0..DNS_SLOTS {
            if self.dns_wait[i].take().is_some() {
                gw.dns_drop(i);
            }
        }
        if let Some(s) = self.dns_sock.take() {
            socket::remove_socket(s);
        }
        for l in &mut self.listeners {
            if let Some(idx) = l.take() {
                socket::remove_socket(idx);
            }
        }
    }

    /// One line per active connection, for the log: where bytes are sitting.
    pub fn status(&self, gw: &mut Gateway<'_>) {
        if self.outbound {
            let c = &self.counts;
            super::p("[KVM] relay: outbound opened=");
            super::dec(c.out_opened);
            super::p(" failed=");
            super::dec(c.out_failed);
            super::p(" denied=");
            super::dec(c.out_denied);
            super::p(" to_guest=");
            super::dec(c.out_to_guest);
            super::p(" to_host=");
            super::dec(c.out_to_host);
            super::p(" dns_fwd=");
            super::dec(c.dns_fwd);
            super::p(" dns_answered=");
            super::dec(c.dns_answered);
            super::p(" dns_timeouts=");
            super::dec(c.dns_timeouts);
            super::p(" gw_syns=");
            super::dec(gw.stats.out_syns);
            super::p(" gw_full=");
            super::dec(gw.stats.out_full);
            super::p(" gw_refused=");
            super::dec(gw.stats.out_refused);
            super::p(" resets=");
            super::dec(gw.stats.tcp_resets);
            super::p("\n");
            for i in 0..OUT_SLOTS {
                let ph = gw.out_phase(i);
                if ph == OutPhase::Free && self.out.get(i).is_none_or(|o| o.phase == Phase::Idle) {
                    continue;
                }
                super::p("[KVM] relay: out slot ");
                super::dec(i as u64);
                super::p(match ph {
                    OutPhase::Free => " gw=free",
                    OutPhase::Asked => " gw=asked",
                    OutPhase::Connecting => " gw=connecting",
                    OutPhase::Open => " gw=open",
                    OutPhase::Refusing => " gw=refusing",
                });
                super::p(match self.out.get(i).map(|o| o.phase) {
                    Some(Phase::Connecting) => " relay=dialing",
                    Some(Phase::Open) => " relay=open",
                    _ => " relay=idle",
                });
                super::p(" tcp_state=");
                super::p(match gw.out(i).state() {
                    akuma_svm_amd64::tcp::State::Closed => "closed",
                    akuma_svm_amd64::tcp::State::Established => "established",
                    akuma_svm_amd64::tcp::State::CloseWait => "close_wait",
                    akuma_svm_amd64::tcp::State::FinWait1 => "fin_wait1",
                    akuma_svm_amd64::tcp::State::FinWait2 => "fin_wait2",
                    akuma_svm_amd64::tcp::State::LastAck => "last_ack",
                    akuma_svm_amd64::tcp::State::Closing => "closing",
                    akuma_svm_amd64::tcp::State::SynRcvd => "syn_rcvd",
                    akuma_svm_amd64::tcp::State::SynSent => "syn_sent",
                });
                super::p("\n");
            }
        }
        for i in 0..FWD_SLOTS {
            if self.slots[i].phase == Phase::Idle {
                continue;
            }
            let t = gw.fwd(i).snapshot();
            super::p("[KVM] relay: slot ");
            super::dec(i as u64);
            super::p(if self.slots[i].phase == Phase::Open { " open" } else { " connecting" });
            super::p(" guest_port=");
            super::dec(u64::from(self.slot_guest[i]));
            super::p(" to_guest=");
            super::dec(self.counts.to_guest);
            super::p(" to_host=");
            super::dec(self.counts.to_host);
            super::p(" up_pending=");
            super::dec(self.slots[i].pipe.up_len as u64);
            super::p(" gw_tx=");
            super::dec(t.tx_queued as u64);
            super::p(" gw_rx=");
            super::dec(t.rx_queued as u64);
            super::p(" in_flight=");
            super::dec(u64::from(t.in_flight));
            super::p(" peer_wnd=");
            super::dec(u64::from(t.peer_window));
            super::p(" host_send_blocked=");
            super::dec(self.counts.send_blocked);
            super::p(" host_recv_idle=");
            super::dec(self.counts.recv_idle);
            super::p(" syn_tries=");
            super::dec(u64::from(self.slots[i].syn_tries));
            super::p(" syn_retries=");
            super::dec(self.counts.syn_retries);
            super::p(" gave_up=");
            super::dec(self.counts.syn_gave_up);
            super::p(" connect_us_max=");
            super::dec(self.counts.connect_us_max);
            super::p("\n");
        }
    }

    fn teardown(&mut self, i: usize, gw: &mut Gateway<'_>) {
        let s = &mut self.slots[i];
        socket::remove_socket(s.sock);
        *s = Slot::new();
        self.slot_guest[i] = 0;
        gw.fwd_release(i);
    }

    /// Is any connection open or being opened? (The run loop sleeps less while one is.)
    pub fn active(&self) -> bool {
        self.slots.iter().any(|s| s.phase != Phase::Idle) || self.out.iter().any(|s| s.phase != Phase::Idle) || self.dns_wait.iter().any(Option::is_some)
    }

    /// Move whatever can move, both ways, on every connection, until nothing moves or
    /// [`ROUNDS`] rounds have. Cheap when idle.
    pub fn poll(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        for _ in 0..ROUNDS {
            let moved = |c: &Counts| c.to_guest + c.to_host + c.accepted + c.out_to_guest + c.out_to_host + c.out_opened + c.dns_answered;
            let before = moved(&self.counts);
            self.poll_once(gw, now_us);
            // Frames the gateway queued for the guest are the caller's to deliver; once the
            // queue is full there is nothing more to do this wake.
            if moved(&self.counts) == before {
                break;
            }
        }
    }

    fn poll_once(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        self.ensure_listeners(now_us);
        for i in 0..FWD_SLOTS {
            if self.slots[i].phase != Phase::Idle {
                continue;
            }
            // The first forward with a client waiting takes the slot.
            let accepted = (0..self.n_fwds).find_map(|k| {
                let l = self.listeners[k]?;
                socket::socket_accept(l, true).ok().map(|(sock, _peer)| (sock, self.fwds[k].guest))
            });
            let Some((sock, guest_port)) = accepted else { break };
            if gw.fwd_open(i, guest_port) {
                self.slots[i] = Slot::new();
                self.slots[i].sock = sock;
                self.slots[i].phase = Phase::Connecting;
                self.slots[i].syn_at_us = now_us;
                self.slots[i].syn_tries = 1;
                self.slot_guest[i] = guest_port;
                self.counts.accepted += 1;
            } else {
                socket::remove_socket(sock);
                self.counts.refused += 1;
            }
        }
        for i in 0..FWD_SLOTS {
            match self.slots[i].phase {
                Phase::Idle => {}
                Phase::Connecting => {
                    if gw.fwd(i).is_established() {
                        let took = now_us.saturating_sub(self.slots[i].syn_at_us);
                        self.counts.connect_us_max = self.counts.connect_us_max.max(took);
                        self.slots[i].phase = Phase::Open;
                    } else if gw.fwd(i).is_closed() {
                        // The guest refused (nothing listening yet) or reset.
                        self.counts.refused += 1;
                        self.teardown(i, gw);
                    } else if now_us.saturating_sub(self.slots[i].syn_at_us) >= SYN_RETRY_US {
                        if self.slots[i].syn_tries >= SYN_TRIES {
                            // The guest never answered: reset the client rather than hold the
                            // slot (and, with every slot held, the listen backlog) for good.
                            self.counts.syn_gave_up += 1;
                            self.teardown(i, gw);
                        } else if gw.fwd_reopen(i) {
                            self.counts.syn_retries += 1;
                            self.slots[i].syn_tries += 1;
                            self.slots[i].syn_at_us = now_us;
                        }
                    }
                }
                Phase::Open => self.pump_open(i, gw),
            }
        }
        if self.outbound {
            self.poll_out(gw, now_us);
            self.poll_dns(gw, now_us);
        }
        gw.pump();
    }

    fn pump_open(&mut self, i: usize, gw: &mut Gateway<'_>) {
        let done = shuttle(gw.fwd(i), self.slots[i].sock, &mut self.slots[i].pipe, &mut self.counts.to_host, &mut self.counts.to_guest, &mut self.counts.send_blocked, &mut self.counts.recv_idle);
        if done {
            self.teardown(i, gw);
        }
    }

    /// Outbound slot storage, allocated once and fallibly (the relay is built on a small stack).
    fn ensure_out(&mut self) -> bool {
        if self.out.len() == OUT_SLOTS {
            return true;
        }
        if self.out.try_reserve_exact(OUT_SLOTS).is_err() {
            return false;
        }
        while self.out.len() < OUT_SLOTS {
            self.out.push(OutSlot::new());
        }
        true
    }

    fn out_teardown(&mut self, i: usize, gw: &mut Gateway<'_>) {
        socket::remove_socket(self.out[i].sock);
        self.out[i].phase = Phase::Idle;
        self.out[i].pipe = Pipe::new();
        gw.out_release(i);
    }

    /// The guest's outward connections: dial what it asked for, answer it, move the bytes.
    fn poll_out(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        if !self.ensure_out() {
            return;
        }
        for i in 0..OUT_SLOTS {
            match self.out[i].phase {
                Phase::Idle => {
                    let Some((ip, port)) = gw.out_take_request(i) else { continue };
                    if !outbound_allowed(ip) {
                        self.counts.out_denied += 1;
                        gw.out_refuse(i);
                        continue;
                    }
                    let Some(sock) = socket::alloc_socket(SOCK_STREAM) else {
                        self.counts.out_failed += 1;
                        gw.out_refuse(i);
                        continue;
                    };
                    match socket::socket_connect(sock, SocketAddrV4::new(ip, port), true) {
                        Ok(()) | Err(EINPROGRESS) => {
                            self.out[i].sock = sock;
                            self.out[i].phase = Phase::Connecting;
                            self.out[i].started_us = now_us;
                            self.out[i].dst = ip;
                            self.out[i].dport = port;
                        }
                        Err(_) => {
                            socket::remove_socket(sock);
                            self.counts.out_failed += 1;
                            gw.out_refuse(i);
                        }
                    }
                }
                Phase::Connecting => {
                    let sock = self.out[i].sock;
                    // The guest gave up (it reset its held SYN): drop the dial.
                    if gw.out_phase(i) != OutPhase::Connecting {
                        self.out_teardown(i, gw);
                        continue;
                    }
                    let _ = socket::socket_tcp_ready(sock); // polls the host stack
                    let state = socket::tcp_recv_snapshot(sock).map_or("CLOSED", |s| s.state);
                    if state == "ESTABLISHED" {
                        if gw.out_accept(i) {
                            self.out[i].phase = Phase::Open;
                            self.counts.out_opened += 1;
                        } else {
                            self.out_teardown(i, gw);
                        }
                    } else if state != "SYN_SENT" || now_us.saturating_sub(self.out[i].started_us) >= OUT_CONNECT_US {
                        // Refused, reset, or nothing answered in time.
                        if self.counts.out_failed < 6 {
                            let d = self.out[i].dst;
                            super::p("[KVM] relay: dial to ");
                            for (k, b) in d.iter().enumerate() {
                                if k > 0 {
                                    super::p(".");
                                }
                                super::dec(u64::from(*b));
                            }
                            super::p(":");
                            super::dec(u64::from(self.out[i].dport));
                            super::p(" ended in state ");
                            super::p(state);
                            super::p(" after ");
                            super::dec(now_us.saturating_sub(self.out[i].started_us) / 1000);
                            super::p(" ms\n");
                        }
                        self.counts.out_failed += 1;
                        gw.out_refuse(i);
                        socket::remove_socket(sock);
                        self.out[i].phase = Phase::Idle;
                    }
                }
                Phase::Open => {
                    let sock = self.out[i].sock;
                    let c = &mut self.counts;
                    let done = shuttle(gw.out(i), sock, &mut self.out[i].pipe, &mut c.out_to_host, &mut c.out_to_guest, &mut c.send_blocked, &mut c.recv_idle);
                    if done {
                        self.out_teardown(i, gw);
                    }
                }
            }
        }
    }

    /// Forward the guest's DNS queries to the host's resolver and hand the answers back.
    fn poll_dns(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        for i in 0..DNS_SLOTS {
            if self.dns_wait[i].is_some() {
                continue;
            }
            let mut q = [0u8; akuma_svm_amd64::gateway::DNS_MAX];
            let n = match gw.dns_take(i) {
                Some(b) => {
                    q[..b.len()].copy_from_slice(b);
                    b.len()
                }
                None => continue,
            };
            let sock = match self.dns_sock {
                Some(s) => Some(s),
                None => {
                    let s = socket::alloc_socket(SOCK_DGRAM);
                    self.dns_sock = s;
                    s
                }
            };
            let sent = match (sock, akuma_net::dns::upstream()) {
                (Some(s), Some(up)) => {
                    let ok = socket::socket_send_udp(s, &q[..n], SocketAddrV4::new(up, 53)).is_ok();
                    if self.counts.dns_fwd < 3 {
                        super::p("[KVM] relay: dns query ");
                        super::dec(n as u64);
                        super::p(" bytes to ");
                        super::dec(u64::from(up[0]));
                        super::p(".");
                        super::dec(u64::from(up[1]));
                        super::p(".");
                        super::dec(u64::from(up[2]));
                        super::p(".");
                        super::dec(u64::from(up[3]));
                        super::p(if ok { " sent\n" } else { " FAILED\n" });
                    }
                    ok
                }
                _ => {
                    super::p("[KVM] relay: dns query dropped: no UDP socket or no resolver\n");
                    false
                }
            };
            if sent {
                self.dns_wait[i] = Some((u16::from_be_bytes([q[0], q[1]]), now_us));
                self.counts.dns_fwd += 1;
            } else {
                gw.dns_drop(i);
            }
        }
        if let Some(sock) = self.dns_sock {
            let mut buf = [0u8; 1500];
            while let Ok((n, _from)) = socket::socket_recv_udp(sock, &mut buf, true) {
                if n < 12 {
                    continue;
                }
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                if self.counts.dns_answered < 3 {
                    super::p("[KVM] relay: dns reply ");
                    super::dec(n as u64);
                    super::p(" bytes rcode=");
                    super::dec(u64::from(buf[3] & 0xF));
                    super::p("\n");
                }
                let hit = (0..DNS_SLOTS).find(|&i| self.dns_wait[i].is_some_and(|(w, _)| w == id) && gw.dns_waiting(i));
                if let Some(i) = hit {
                    self.dns_wait[i] = None;
                    if gw.dns_reply(i, &buf[..n]) {
                        self.counts.dns_answered += 1;
                    } else {
                        // The wire was full: the guest's retry asks again.
                        gw.dns_drop(i);
                    }
                }
            }
        }
        for i in 0..DNS_SLOTS {
            if let Some((_, t)) = self.dns_wait[i]
                && now_us.saturating_sub(t) >= DNS_WAIT_US
            {
                self.dns_wait[i] = None;
                gw.dns_drop(i);
                self.counts.dns_timeouts += 1;
            }
        }
    }
}

/// May the host dial `ip` for a guest? The gateway has already kept the guest's own subnet,
/// loopback, "this host" and multicast from getting this far; this is the host's own say.
fn outbound_allowed(ip: [u8; 4]) -> bool {
    !(ip[0] == 0 || ip[0] == 127 || ip[0] >= 224)
}

/// Say, for the first few, why a host socket broke a connection (an errno from the host stack).
fn note_err(what: &str, errno: i32) {
    static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 6 {
        super::p("[KVM] relay: host socket ");
        super::p(what);
        super::p(" failed, errno ");
        super::dec(errno.unsigned_abs().into());
        super::p("\n");
    }
}

/// Move bytes between a guest connection and its host socket, both ways, one chunk each.
/// True when the connection is finished (or broken) and both ends should be torn down.
fn shuttle(conn: &mut Tcp<'_>, sock: usize, p: &mut Pipe, to_host: &mut u64, to_guest: &mut u64, send_blocked: &mut u64, recv_idle: &mut u64) -> bool {
    if conn.was_reset() || (conn.is_closed() && p.up_len == 0) {
        return true;
    }
    // Guest to host: read a chunk if the last one is gone, offer it to the host socket.
    if p.up_len == 0 {
        let n = conn.read(&mut p.up);
        p.up_off = 0;
        p.up_len = n;
    }
    if p.up_len > 0 {
        match socket::socket_send(sock, &p.up[p.up_off..p.up_len], true) {
            Ok(n) => {
                p.up_off += n;
                *to_host += n as u64;
                if p.up_off >= p.up_len {
                    p.up_len = 0;
                    p.up_off = 0;
                }
            }
            Err(e) if e == EAGAIN => *send_blocked += 1,
            Err(e) => {
                note_err("send", e);
                conn.abort();
                return true;
            }
        }
    }
    if conn.peer_finished() && p.up_len == 0 && !p.guest_eof_passed {
        let _ = socket::socket_shutdown(sock, SHUT_WR);
        p.guest_eof_passed = true;
    }
    // Host to guest.
    if !p.host_eof {
        let room = conn.write_room().min(CHUNK);
        if room > 0 {
            let mut buf = [0u8; CHUNK];
            match socket::socket_recv(sock, &mut buf[..room], true) {
                Ok(0) => {
                    p.host_eof = true;
                    conn.close();
                }
                Ok(n) => {
                    let took = conn.write(&buf[..n]);
                    *to_guest += took as u64;
                }
                Err(e) if e == EAGAIN => *recv_idle += 1,
                Err(e) => {
                    note_err("recv", e);
                    conn.abort();
                    return true;
                }
            }
        }
    }
    p.host_eof && p.guest_eof_passed && conn.is_closed()
}
