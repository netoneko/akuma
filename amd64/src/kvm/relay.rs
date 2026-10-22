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

/// `EAGAIN`, as the host socket layer reports it (a positive errno).
const EAGAIN: i32 = 11;
/// `shutdown(SHUT_WR)`.
const SHUT_WR: i32 = 1;
/// The port on the guest every connection is forwarded to (its sshd).
const GUEST_PORT: u16 = 22;
/// Bytes moved per direction per poll.
const CHUNK: usize = 1024;
/// How long to wait before retrying a listener the host stack refused (µs).
const RETRY_US: u64 = 1_000_000;

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
}

impl Slot {
    const fn new() -> Self {
        Self { sock: 0, phase: Phase::Idle, host_eof: false, guest_eof_passed: false, up: [0; CHUNK], up_off: 0, up_len: 0 }
    }
}

/// What the relay has done, for the end-of-run summary.
#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub accepted: u64,
    pub to_guest: u64,
    pub to_host: u64,
    pub refused: u64,
}

pub struct Relay {
    port: u16,
    listener: Option<usize>,
    retry_at_us: u64,
    slots: [Slot; FWD_SLOTS],
    pub counts: Counts,
}

impl Relay {
    /// A relay for host port `port`; nothing is bound until the first [`Relay::poll`].
    pub const fn new(port: u16) -> Self {
        Self { port, listener: None, retry_at_us: 0, slots: [const { Slot::new() }; FWD_SLOTS], counts: Counts { accepted: 0, to_guest: 0, to_host: 0, refused: 0 } }
    }

    /// Is the listener up?
    pub const fn listening(&self) -> bool {
        self.listener.is_some()
    }

    fn ensure_listener(&mut self, now_us: u64) {
        if self.listener.is_some() || now_us < self.retry_at_us {
            return;
        }
        self.retry_at_us = now_us + RETRY_US;
        let Some(idx) = socket::alloc_socket(SOCK_STREAM) else { return };
        let up = socket::socket_bind(idx, SocketAddrV4::new([0; 4], self.port)).is_ok() && socket::socket_listen(idx, 4).is_ok();
        if up {
            self.listener = Some(idx);
            super::p("[KVM] relay: listening on host port ");
            super::dec(u64::from(self.port));
            super::p("\n");
        } else {
            socket::remove_socket(idx);
        }
    }

    fn teardown(&mut self, i: usize, gw: &mut Gateway<'_>) {
        let s = &mut self.slots[i];
        socket::remove_socket(s.sock);
        *s = Slot::new();
        gw.fwd_release(i);
    }

    /// Move whatever can move, both ways, on every connection. Cheap when idle.
    pub fn poll(&mut self, gw: &mut Gateway<'_>, now_us: u64) {
        self.ensure_listener(now_us);
        let Some(listener) = self.listener else { return };
        for i in 0..FWD_SLOTS {
            if self.slots[i].phase == Phase::Idle {
                match socket::socket_accept(listener, true) {
                    Ok((sock, _peer)) => {
                        if gw.fwd_open(i, GUEST_PORT) {
                            self.slots[i] = Slot::new();
                            self.slots[i].sock = sock;
                            self.slots[i].phase = Phase::Connecting;
                            self.counts.accepted += 1;
                        } else {
                            socket::remove_socket(sock);
                            self.counts.refused += 1;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        for i in 0..FWD_SLOTS {
            match self.slots[i].phase {
                Phase::Idle => {}
                Phase::Connecting => {
                    if gw.fwd(i).is_established() {
                        self.slots[i].phase = Phase::Open;
                    } else if gw.fwd(i).is_closed() {
                        // The guest refused (nothing listening yet) or reset.
                        self.counts.refused += 1;
                        self.teardown(i, gw);
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
                Err(e) if e == EAGAIN => {}
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
                    Err(e) if e == EAGAIN => {}
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
