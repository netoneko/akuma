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
//! No allocation: a chunk buffer per direction on the stack, one for each slot's
//! guest-to-host leftovers.

use akuma_net::socket::{self, socket_const::SOCK_STREAM, SocketAddrV4};
use akuma_svm_amd64::gateway::{Gateway, FWD_SLOTS};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// The gateway has sent its SYN to the guest.
    Connecting,
    Open,
}

struct Slot {
    sock: usize,
    phase: Phase,
    /// The host client closed its side (we saw EOF): the guest has been told with a FIN.
    host_eof: bool,
    /// The guest closed its side and everything it sent has been delivered: the host
    /// client has been told with `shutdown(SHUT_WR)`.
    guest_eof_passed: bool,
    /// Guest-to-host bytes read from the guest but not yet accepted by the host socket.
    up: [u8; CHUNK],
    up_off: usize,
    up_len: usize,
    /// When the SYN last went out, and how many have.
    syn_at_us: u64,
    syn_tries: u8,
}

impl Slot {
    const fn new() -> Self {
        Self { sock: 0, phase: Phase::Idle, host_eof: false, guest_eof_passed: false, up: [0; CHUNK], up_off: 0, up_len: 0, syn_at_us: 0, syn_tries: 0 }
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
    pub counts: Counts,
}

impl Relay {
    /// A relay for `forwards` (at most [`MAX_FORWARDS`]); nothing is bound until the first
    /// [`Relay::poll`].
    pub const fn new(forwards: &[Forward]) -> Self {
        let mut fwds = [Forward { host: 0, guest: 0 }; MAX_FORWARDS];
        let mut i = 0;
        while i < forwards.len() && i < MAX_FORWARDS {
            fwds[i] = forwards[i];
            i += 1;
        }
        Self { fwds, n_fwds: i, listeners: [None; MAX_FORWARDS], retry_at_us: 0, slots: [const { Slot::new() }; FWD_SLOTS], slot_guest: [0; FWD_SLOTS], counts: Counts { accepted: 0, to_guest: 0, to_host: 0, refused: 0, send_blocked: 0, recv_idle: 0, syn_retries: 0, syn_gave_up: 0, connect_us_max: 0 } }
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
        for l in &mut self.listeners {
            if let Some(idx) = l.take() {
                socket::remove_socket(idx);
            }
        }
    }

    /// One line per active connection, for the log: where bytes are sitting.
    pub fn status(&self, gw: &mut Gateway<'_>) {
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
            super::dec(self.slots[i].up_len as u64);
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
        self.slots.iter().any(|s| s.phase != Phase::Idle)
    }

    /// Move whatever can move, both ways, on every connection, until nothing moves or
    /// [`ROUNDS`] rounds have. Cheap when idle.
    pub fn poll(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        for _ in 0..ROUNDS {
            let before = self.counts.to_guest + self.counts.to_host + self.counts.accepted;
            self.poll_once(gw, now_us);
            // Frames the gateway queued for the guest are the caller's to deliver; once the
            // queue is full there is nothing more to do this wake.
            if self.counts.to_guest + self.counts.to_host + self.counts.accepted == before {
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
        gw.pump();
    }

    fn pump_open(&mut self, i: usize, gw: &mut Gateway<'_>) {
        if gw.fwd(i).was_reset() || (gw.fwd(i).is_closed() && self.slots[i].up_len == 0) {
            self.teardown(i, gw);
            return;
        }
        let sock = self.slots[i].sock;
        // Guest to host: read a chunk if the last one is gone, offer it to the host socket.
        if self.slots[i].up_len == 0 {
            let s = &mut self.slots[i];
            let n = gw.fwd(i).read(&mut s.up);
            s.up_off = 0;
            s.up_len = n;
        }
        if self.slots[i].up_len > 0 {
            let s = &mut self.slots[i];
            match socket::socket_send(sock, &s.up[s.up_off..s.up_len], true) {
                Ok(n) => {
                    s.up_off += n;
                    self.counts.to_host += n as u64;
                    if s.up_off >= s.up_len {
                        s.up_len = 0;
                        s.up_off = 0;
                    }
                }
                Err(e) if e == EAGAIN => self.counts.send_blocked += 1,
                Err(_) => {
                    gw.fwd(i).abort();
                    self.teardown(i, gw);
                    return;
                }
            }
        }
        if gw.fwd(i).peer_finished() && self.slots[i].up_len == 0 && !self.slots[i].guest_eof_passed {
            let _ = socket::socket_shutdown(sock, SHUT_WR);
            self.slots[i].guest_eof_passed = true;
        }
        // Host to guest.
        if !self.slots[i].host_eof {
            let room = gw.fwd(i).write_room().min(CHUNK);
            if room > 0 {
                let mut buf = [0u8; CHUNK];
                match socket::socket_recv(sock, &mut buf[..room], true) {
                    Ok(0) => {
                        self.slots[i].host_eof = true;
                        gw.fwd(i).close();
                    }
                    Ok(n) => {
                        let took = gw.fwd(i).write(&buf[..n]);
                        self.counts.to_guest += took as u64;
                    }
                    Err(e) if e == EAGAIN => self.counts.recv_idle += 1,
                    Err(_) => {
                        gw.fwd(i).abort();
                        self.teardown(i, gw);
                        return;
                    }
                }
            }
        }
        if self.slots[i].host_eof && self.slots[i].guest_eof_passed && gw.fwd(i).is_closed() {
            self.teardown(i, gw);
        }
    }
}
