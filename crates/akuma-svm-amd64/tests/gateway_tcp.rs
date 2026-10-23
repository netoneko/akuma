//! The gateway's TCP against an independent stack: smoltcp (the stack the Akuma guest
//! itself runs) drives the gateway as a client of its echo port and as the listener the
//! gateway connects to. A stack written against the same misreading of RFC 793 would pass
//! its own tests; this one does not share it.

use akuma_svm_amd64::gateway::{Gateway, OutPhase, FWD_PORT_BASE, GATEWAY_IP, GUEST_IP, GUEST_MAC, OUT_SLOTS, STORAGE};
use akuma_svm_amd64::tcp::State;
use akuma_svm_amd64::virtio_net::NetBackend;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use std::collections::VecDeque;

struct Dev {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}
impl Device for Dev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        let f = self.rx.pop_front()?;
        Some((Rx(f), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

struct Net {
    dev: Dev,
    iface: Interface,
    sockets: SocketSet<'static>,
    gw: Gateway<'static>,
    ms: i64,
}

impl Net {
    fn new() -> Self {
        let mut dev = Dev { rx: VecDeque::new(), tx: VecDeque::new() };
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC))),
            &mut dev,
            Instant::from_millis(0),
        );
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::v4(GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], GUEST_IP[3]), 24)).unwrap();
        });
        let storage: &'static mut [u8] = Box::leak(vec![0u8; STORAGE].into_boxed_slice());
        Self { dev, iface, sockets: SocketSet::new(vec![]), gw: Gateway::new(storage).unwrap(), ms: 0 }
    }

    /// A guest whose default route is the gateway, with outbound switched on.
    fn outbound() -> Self {
        let mut n = Self::new();
        n.iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3])).unwrap();
        n.gw.set_outbound(true);
        n
    }

    fn connect(&mut self, ip: [u8; 4], port: u16, local: u16) -> SocketHandle {
        let h = self.socket();
        let cx = self.iface.context();
        self.sockets.get_mut::<tcp::Socket>(h).connect(cx, (IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), port), local).unwrap();
        h
    }

    fn socket(&mut self) -> SocketHandle {
        let buf = || tcp::SocketBuffer::new(vec![0u8; 65_535]);
        self.sockets.add(tcp::Socket::new(buf(), buf()))
    }

    /// One round: the guest's stack runs, its frames go to the gateway, the gateway's come back.
    fn step(&mut self) {
        self.ms += 1;
        let now = Instant::from_millis(self.ms);
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
        while let Some(f) = self.dev.tx.pop_front() {
            self.gw.transmit(&f);
        }
        self.gw.pump();
        let mut buf = [0u8; 2048];
        while let Some(n) = self.gw.receive(&mut buf) {
            if n > 0 {
                self.dev.rx.push_back(buf[..n].to_vec());
            }
        }
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
    }

    fn run_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..200_000 {
            self.step();
            if done(self) {
                return;
            }
        }
        panic!("timed out waiting for: {what}");
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
}

#[test]
fn smoltcp_client_gets_its_bytes_echoed_back_intact() {
    let mut net = Net::new();
    let h = net.socket();
    {
        let cx = net.iface.context();
        net.sockets.get_mut::<tcp::Socket>(h)
            .connect(cx, (IpAddress::v4(GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3]), 7), 49_152)
            .unwrap();
    }
    net.run_until("established", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established);
    assert_eq!(net.gw.stats.tcp_accepts, 1);

    // Far more than either ring: the echo has to flow-control in both directions.
    let data = pattern(300_000);
    let (mut sent, mut got) = (0usize, Vec::new());
    net.run_until("echo of 300 000 bytes", |n| {
        let s = n.sockets.get_mut::<tcp::Socket>(h);
        if sent < data.len() && s.can_send() {
            sent += s.send_slice(&data[sent..]).unwrap();
        }
        let mut chunk = [0u8; 4096];
        while s.can_recv() {
            let k = s.recv_slice(&mut chunk).unwrap();
            got.extend_from_slice(&chunk[..k]);
        }
        got.len() >= data.len()
    });
    assert_eq!(got, data, "the echo is byte-exact");

    // An orderly close from the client ends the server side too.
    net.sockets.get_mut::<tcp::Socket>(h).close();
    net.run_until("closed", |n| matches!(n.sockets.get::<tcp::Socket>(h).state(), tcp::State::Closed | tcp::State::TimeWait));
    for _ in 0..50 {
        net.step();
    }
    assert_eq!(net.gw.fwd(0).state(), State::Closed);
}

#[test]
fn a_second_echo_connection_works_after_the_first_closed() {
    let mut net = Net::new();
    for round in 0..2u16 {
        let h = net.socket();
        {
            let cx = net.iface.context();
            net.sockets.get_mut::<tcp::Socket>(h)
                .connect(cx, (IpAddress::v4(10, 0, 2, 2), 7), 50_000 + round)
                .unwrap();
        }
        net.run_until("established", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established);
        net.sockets.get_mut::<tcp::Socket>(h).send_slice(b"ping").unwrap();
        let mut got = Vec::new();
        net.run_until("echo", |n| {
            let s = n.sockets.get_mut::<tcp::Socket>(h);
            let mut b = [0u8; 16];
            while s.can_recv() {
                let k = s.recv_slice(&mut b).unwrap();
                got.extend_from_slice(&b[..k]);
            }
            got.len() == 4
        });
        assert_eq!(got, b"ping");
        net.sockets.get_mut::<tcp::Socket>(h).close();
        net.run_until("closed", |n| matches!(n.sockets.get::<tcp::Socket>(h).state(), tcp::State::Closed | tcp::State::TimeWait));
        for _ in 0..50 {
            net.step();
        }
    }
    assert_eq!(net.gw.stats.tcp_accepts, 2);
}

#[test]
fn a_connection_to_a_closed_port_is_refused() {
    let mut net = Net::new();
    let h = net.socket();
    {
        let cx = net.iface.context();
        net.sockets.get_mut::<tcp::Socket>(h).connect(cx, (IpAddress::v4(10, 0, 2, 2), 9999), 51_000).unwrap();
    }
    net.run_until("refused", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed);
    assert!(net.gw.stats.tcp_resets >= 1);
}

#[test]
fn the_gateway_connects_to_a_listener_in_the_guest_and_streams_both_ways() {
    let mut net = Net::new();
    let h = net.socket();
    net.sockets.get_mut::<tcp::Socket>(h).listen(22).unwrap();
    assert!(net.gw.fwd_open(0, 22));
    assert!(!net.gw.fwd_open(0, 22), "the slot is in use");
    net.run_until("established", |n| {
        n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established && n.gw.fwd(0).is_established()
    });

    let up = pattern(200_000); // gateway -> guest
    let down: Vec<u8> = pattern(150_000).iter().map(|b| b ^ 0x5A).collect(); // guest -> gateway
    let (mut up_sent, mut down_sent) = (0usize, 0usize);
    let (mut up_got, mut down_got) = (Vec::new(), Vec::new());
    net.run_until("both directions", |n| {
        if up_sent < up.len() {
            up_sent += n.gw.fwd(0).write(&up[up_sent..]);
        }
        let mut chunk = [0u8; 4096];
        let k = n.gw.fwd(0).read(&mut chunk);
        down_got.extend_from_slice(&chunk[..k]);
        let s = n.sockets.get_mut::<tcp::Socket>(h);
        if down_sent < down.len() && s.can_send() {
            down_sent += s.send_slice(&down[down_sent..]).unwrap();
        }
        while s.can_recv() {
            let k = s.recv_slice(&mut chunk).unwrap();
            up_got.extend_from_slice(&chunk[..k]);
        }
        up_got.len() >= up.len() && down_got.len() >= down.len()
    });
    assert_eq!(up_got, up, "gateway -> guest");
    assert_eq!(down_got, down, "guest -> gateway");

    // The gateway closes; the guest sees the end of stream, closes in turn.
    net.gw.fwd(0).close();
    net.run_until("guest sees EOF", |n| !n.sockets.get::<tcp::Socket>(h).may_recv());
    net.sockets.get_mut::<tcp::Socket>(h).close();
    net.run_until("gateway side closed", |n| n.gw.fwd(0).is_closed());
    // The slot is reusable.
    net.gw.fwd_release(0);
    assert!(net.gw.fwd_open(0, 22));
    let _ = FWD_PORT_BASE;
}

#[test]
fn a_guest_that_closes_first_is_seen_as_end_of_stream() {
    let mut net = Net::new();
    let h = net.socket();
    net.sockets.get_mut::<tcp::Socket>(h).listen(2222).unwrap();
    assert!(net.gw.fwd_open(1, 2222));
    net.run_until("established", |n| n.gw.fwd(1).is_established());
    net.sockets.get_mut::<tcp::Socket>(h).send_slice(b"last words").unwrap();
    net.sockets.get_mut::<tcp::Socket>(h).close();
    let mut got = Vec::new();
    net.run_until("peer finished", |n| {
        let mut b = [0u8; 64];
        let k = n.gw.fwd(1).read(&mut b);
        got.extend_from_slice(&b[..k]);
        n.gw.fwd(1).peer_finished()
    });
    assert_eq!(got, b"last words");
}

#[test]
fn a_syn_the_guest_never_saw_is_retried_from_a_new_port_and_the_old_attempt_is_dead() {
    // The 2026-10-22 ryzen wedge: slots stuck `connecting`, `in_flight=1`, for good. The SYN is
    // sent once and nothing retried it; the metal never said where it went.
    let mut net = Net::new();
    let h = net.socket();
    net.sockets.get_mut::<tcp::Socket>(h).listen(22).unwrap();
    assert!(net.gw.fwd_open(0, 22));
    net.gw.pump();
    // The guest never sees it: take the frame off the queue.
    let mut lost = [0u8; 2048];
    let mut dropped = 0;
    while net.gw.receive(&mut lost).is_some() {
        dropped += 1;
    }
    assert_eq!(dropped, 1, "exactly the SYN was queued");
    for _ in 0..2_000 {
        net.step();
    }
    assert!(!net.gw.fwd(0).is_established(), "nothing answers a SYN nobody received");
    assert_eq!(net.gw.fwd(0).state(), State::SynSent);

    assert!(net.gw.fwd_reopen(0));
    net.run_until("established after the retry", |n| {
        n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established && n.gw.fwd(0).is_established()
    });

    // The retry also works with bytes flowing, and an idle slot is not reopened.
    assert_eq!(net.gw.fwd(0).write(b"hello"), 5);
    let mut got = Vec::new();
    net.run_until("bytes arrive", |n| {
        let s = n.sockets.get_mut::<tcp::Socket>(h);
        let mut b = [0u8; 16];
        while s.can_recv() {
            let k = s.recv_slice(&mut b).unwrap();
            got.extend_from_slice(&b[..k]);
        }
        got == b"hello"
    });
    assert!(!net.gw.fwd_reopen(1), "slot 1 is idle: nothing to retry");
}

#[test]
fn every_connection_on_a_slot_uses_a_new_source_port() {
    // A guest holding the previous connection half-open (a lost reset, TIME-WAIT) must not be
    // able to swallow the next SYN: the tuple differs each time.
    let mut net = Net::new();
    let h = net.socket();
    let mut ports = Vec::new();
    for round in 0..10 {
        net.sockets.get_mut::<tcp::Socket>(h).listen(22).unwrap();
        assert!(net.gw.fwd_open(0, 22), "round {round}");
        net.run_until("established", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established);
        ports.push(net.sockets.get::<tcp::Socket>(h).remote_endpoint().unwrap().port);
        net.gw.fwd(0).close();
        net.run_until("guest sees EOF", |n| !n.sockets.get::<tcp::Socket>(h).may_recv());
        net.sockets.get_mut::<tcp::Socket>(h).close();
        net.run_until("closed", |n| n.gw.fwd(0).is_closed());
        net.sockets.get_mut::<tcp::Socket>(h).abort();
        net.gw.fwd_release(0);
        for _ in 0..5 {
            net.step();
        }
    }
    for w in ports.windows(2) {
        assert_ne!(w[0], w[1], "consecutive connections reused a source port: {ports:?}");
    }
    assert!(ports.iter().all(|p| (FWD_PORT_BASE..FWD_PORT_BASE + 16).contains(p)), "{ports:?}");
}

#[test]
fn a_reset_gateway_is_a_fresh_one() {
    // The hypervisor reuses one gateway (and its lent storage) for every guest it runs: after
    // `reset` nothing of the last guest may survive - not its connections, its counters, its
    // queued frames or the source-port generation - and the next guest must connect normally.
    let mut net = Net::new();
    let h = net.socket();
    net.sockets.get_mut::<tcp::Socket>(h).listen(22).unwrap();
    assert!(net.gw.fwd_open(0, 22));
    net.run_until("established", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established);
    let first_port = net.sockets.get::<tcp::Socket>(h).remote_endpoint().unwrap().port;
    assert!(net.gw.stats.frames > 0);
    net.sockets.get_mut::<tcp::Socket>(h).abort();
    net.gw.reset();
    assert_eq!(net.gw.stats.frames, 0);
    assert_eq!(net.gw.queued(), 0);
    assert_eq!(net.gw.fwd(0).state(), State::Closed);
    // The generation starts over, so the first connection of the next guest is the first port again.
    net.step();
    net.sockets.get_mut::<tcp::Socket>(h).listen(22).unwrap();
    assert!(net.gw.fwd_open(0, 22));
    net.run_until("established again", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established);
    assert_eq!(net.sockets.get::<tcp::Socket>(h).remote_endpoint().unwrap().port, first_port);
}


const WEB: [u8; 4] = [93, 184, 216, 34];

#[test]
fn outbound_is_off_by_default_and_nothing_is_held() {
    let mut net = Net::new();
    net.iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).unwrap();
    let h = net.connect(WEB, 80, 40_001);
    for _ in 0..3_000 {
        net.step();
    }
    assert_eq!(net.gw.out_phase(0), OutPhase::Free);
    assert_eq!(net.sockets.get::<tcp::Socket>(h).state(), tcp::State::SynSent, "no answer, no refusal");
    assert!(net.gw.stats.ignored > 0);
}

#[test]
fn an_outbound_connection_is_held_until_the_host_answers_then_streams_both_ways() {
    let mut net = Net::outbound();
    let h = net.connect(WEB, 80, 40_002);
    net.run_until("the gateway holds the SYN", |n| n.gw.out_phase(0) == OutPhase::Asked);
    assert_eq!(net.sockets.get::<tcp::Socket>(h).state(), tcp::State::SynSent);
    assert_eq!(net.gw.out_take_request(0), Some((WEB, 80)));
    assert_eq!(net.gw.out_take_request(0), None, "reported once");
    // The guest's own SYN retransmissions while the host is still connecting change nothing.
    for _ in 0..1_500 {
        net.step();
    }
    assert_eq!(net.gw.out_phase(0), OutPhase::Connecting);
    assert_eq!(net.sockets.get::<tcp::Socket>(h).state(), tcp::State::SynSent);

    assert!(net.gw.out_accept(0));
    net.run_until("established", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established && n.gw.out(0).is_established());
    // The guest believes it reached the real address.
    let ep: IpEndpoint = net.sockets.get::<tcp::Socket>(h).remote_endpoint().unwrap();
    assert_eq!(ep.addr, IpAddress::v4(WEB[0], WEB[1], WEB[2], WEB[3]));
    assert_eq!(ep.port, 80);

    let up = pattern(200_000); // host -> guest
    let down: Vec<u8> = pattern(150_000).iter().map(|b| b ^ 0x33).collect(); // guest -> host
    let (mut up_sent, mut down_sent) = (0usize, 0usize);
    let (mut up_got, mut down_got) = (Vec::new(), Vec::new());
    net.run_until("both directions", |n| {
        if up_sent < up.len() {
            up_sent += n.gw.out(0).write(&up[up_sent..]);
        }
        let mut chunk = [0u8; 4096];
        let k = n.gw.out(0).read(&mut chunk);
        down_got.extend_from_slice(&chunk[..k]);
        let s = n.sockets.get_mut::<tcp::Socket>(h);
        if down_sent < down.len() && s.can_send() {
            down_sent += s.send_slice(&down[down_sent..]).unwrap();
        }
        while s.can_recv() {
            let k = s.recv_slice(&mut chunk).unwrap();
            up_got.extend_from_slice(&chunk[..k]);
        }
        up_got.len() >= up.len() && down_got.len() >= down.len()
    });
    assert_eq!(up_got, up);
    assert_eq!(down_got, down);

    // The guest closes first; the host sees the end, closes; the slot comes back.
    net.sockets.get_mut::<tcp::Socket>(h).close();
    net.run_until("host sees EOF", |n| n.gw.out(0).peer_finished());
    net.gw.out(0).close();
    net.run_until("closed", |n| n.gw.out(0).is_closed());
    net.sockets.get_mut::<tcp::Socket>(h).abort();
    net.gw.out_release(0);
    assert_eq!(net.gw.out_phase(0), OutPhase::Free);
}

#[test]
fn a_host_that_cannot_connect_resets_the_guest_and_frees_the_slot() {
    let mut net = Net::outbound();
    let h = net.connect(WEB, 81, 40_003);
    net.run_until("held", |n| n.gw.out_phase(0) == OutPhase::Asked);
    assert!(net.gw.out_take_request(0).is_some());
    net.gw.out_refuse(0);
    net.run_until("the guest is refused", |n| n.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed);
    assert_eq!(net.gw.out_phase(0), OutPhase::Free);
    assert_eq!(net.gw.stats.out_refused, 1);
}

#[test]
fn the_ninth_connection_is_reset_and_a_stray_segment_is_reset() {
    let mut net = Net::outbound();
    for i in 0..OUT_SLOTS as u16 {
        net.connect(WEB, 80, 41_000 + i);
    }
    net.run_until("all held", |n| (0..OUT_SLOTS).all(|i| n.gw.out_phase(i) == OutPhase::Asked));
    let extra = net.connect(WEB, 80, 41_100);
    net.run_until("the extra one is reset", |n| n.sockets.get::<tcp::Socket>(extra).state() == tcp::State::Closed);
    assert_eq!(net.gw.stats.out_full, 1);
}

#[test]
fn the_guests_own_subnet_loopback_and_multicast_are_never_forwarded() {
    for dst in [[10, 0, 2, 9], [127, 0, 0, 1], [224, 0, 0, 1], [0, 0, 0, 5]] {
        let mut net = Net::outbound();
        net.iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).unwrap();
        let _ = net.connect(dst, 80, 42_000);
        for _ in 0..2_000 {
            net.step();
        }
        assert!((0..OUT_SLOTS).all(|i| net.gw.out_phase(i) == OutPhase::Free), "{dst:?} was held");
    }
}

#[test]
fn dns_queries_to_the_proxy_are_queued_for_the_host_and_answered_from_it() {
    let mut net = Net::outbound();
    let buf = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]);
    let h = net.sockets.add(udp::Socket::new(buf(), buf()));
    net.sockets.get_mut::<udp::Socket>(h).bind(5353).unwrap();
    // A minimal A query for example.com, id 0x1234.
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["example", "com"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    net.sockets.get_mut::<udp::Socket>(h).send_slice(&q, (IpAddress::v4(10, 0, 2, 3), 53)).unwrap();
    net.run_until("queued", |n| n.gw.dns_waiting(0) || n.gw.stats.dns_queries == 1);
    let taken = net.gw.dns_take(0).map(<[u8]>::to_vec);
    assert_eq!(taken.as_deref(), Some(&q[..]), "forwarded verbatim");
    assert!(net.gw.dns_take(0).is_none(), "taken once");
    // The guest's retry of the same query while it is in flight is not queued again.
    net.sockets.get_mut::<udp::Socket>(h).send_slice(&q, (IpAddress::v4(10, 0, 2, 3), 53)).unwrap();
    for _ in 0..20 {
        net.step();
    }
    assert_eq!(net.gw.stats.dns_queries, 1);

    let mut answer = q.clone();
    answer[2] = 0x81;
    answer[3] = 0x80;
    assert!(net.gw.dns_reply(0, &answer));
    let mut got = Vec::new();
    net.run_until("the answer arrives", |n| {
        let s = n.sockets.get_mut::<udp::Socket>(h);
        if let Ok((data, meta)) = s.recv() {
            assert_eq!(meta.endpoint.addr, IpAddress::v4(10, 0, 2, 3));
            assert_eq!(meta.endpoint.port, 53);
            got = data.to_vec();
        }
        !got.is_empty()
    });
    assert_eq!(got, answer);
    assert!(!net.gw.dns_waiting(0), "the slot is free again");
}
