extern crate std;

use super::*;
use core::cell::{Cell, RefCell};
use std::vec::Vec;

const ID: u16 = 0xBEEF;

/// A response to the query `build_query(ID, host, ..)` made: header, the echoed
/// question, then `answers` verbatim.
fn response(host: &str, rcode: u16, answers: &[&[u8]]) -> Vec<u8> {
    let mut q = [0u8; MAX_PACKET];
    let qlen = build_query(ID, host, &mut q).unwrap();
    let mut r = q[..qlen].to_vec();
    r[2..4].copy_from_slice(&(0x8180u16 | rcode).to_be_bytes()); // QR, RD, RA
    r[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
    for a in answers {
        r.extend_from_slice(a);
    }
    r
}

/// An answer RR whose name is a compression pointer to the question (0xC00C).
fn rr(rtype: u16, rdata: &[u8]) -> Vec<u8> {
    let mut v = std::vec![0xC0, 0x0C];
    v.extend_from_slice(&rtype.to_be_bytes());
    v.extend_from_slice(&1u16.to_be_bytes()); // IN
    v.extend_from_slice(&60u32.to_be_bytes()); // TTL
    v.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    v.extend_from_slice(rdata);
    v
}

fn a_record(ip: [u8; 4]) -> Vec<u8> {
    rr(1, &ip)
}

#[test]
fn a_query_is_a_header_a_question_and_nothing_else() {
    let mut buf = [0u8; MAX_PACKET];
    let n = build_query(ID, "pool.ntp.org", &mut buf).unwrap();
    assert_eq!(&buf[0..2], &ID.to_be_bytes());
    assert_eq!(&buf[2..4], &[0x01, 0x00], "recursion desired, standard query");
    assert_eq!(&buf[4..12], &[0, 1, 0, 0, 0, 0, 0, 0], "one question, no other sections");
    assert_eq!(&buf[12..n], b"\x04pool\x03ntp\x03org\x00\x00\x01\x00\x01");
}

#[test]
fn names_that_do_not_encode_are_refused() {
    let mut buf = [0u8; MAX_PACKET];
    for bad in ["", ".", "a..b", "trailing.", ".leading"] {
        assert!(build_query(ID, bad, &mut buf).is_none(), "{bad:?} should not encode");
    }
    let long_label = "a".repeat(64);
    assert!(build_query(ID, &long_label, &mut buf).is_none(), "64-byte label");
    assert!(build_query(ID, &"a".repeat(63), &mut buf).is_some(), "63 is DNS's limit");
    let long_name = std::vec!["abcdefghi"; 40].join(".");
    assert!(build_query(ID, &long_name, &mut buf).is_none(), "does not fit MAX_PACKET");
}

#[test]
fn an_a_record_is_the_answer() {
    let r = response("example.com", 0, &[&a_record([93, 184, 216, 34])]);
    assert_eq!(parse_response(&r, ID), Reply::Answer([93, 184, 216, 34]));
}

#[test]
fn a_cname_is_walked_past_to_the_a_record() {
    // CNAME rdata is a name; the walk must step over its rdlength, not parse it.
    let cname = rr(5, b"\x03www\x07example\x03com\x00");
    let r = response("alias.example.com", 0, &[&cname, &a_record([1, 2, 3, 4])]);
    assert_eq!(parse_response(&r, ID), Reply::Answer([1, 2, 3, 4]));
}

#[test]
fn an_aaaa_only_answer_is_no_record() {
    let r = response("v6.example.com", 0, &[&rr(28, &[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])]);
    assert_eq!(parse_response(&r, ID), Reply::NoRecord);
}

#[test]
fn nxdomain_servfail_and_refused_are_no_record() {
    for rcode in [3, 2, 5] {
        let r = response("example.com", rcode, &[]);
        assert_eq!(parse_response(&r, ID), Reply::NoRecord, "rcode {rcode}");
    }
}

#[test]
fn a_noerror_answer_with_no_records_is_no_record() {
    assert_eq!(parse_response(&response("example.com", 0, &[]), ID), Reply::NoRecord);
}

#[test]
fn a_reply_to_another_query_is_ignored() {
    let r = response("example.com", 0, &[&a_record([9, 9, 9, 9])]);
    assert_eq!(parse_response(&r, ID ^ 1), Reply::Ignore);
}

#[test]
fn a_query_echoed_back_is_not_a_response() {
    let mut q = [0u8; MAX_PACKET];
    let n = build_query(ID, "example.com", &mut q).unwrap();
    assert_eq!(parse_response(&q[..n], ID), Reply::Ignore, "QR bit clear");
}

#[test]
fn truncation_anywhere_is_ignored_never_a_panic() {
    let full = response("example.com", 0, &[&a_record([5, 6, 7, 8])]);
    for cut in 0..full.len() {
        // Any prefix is either ignored or, once the header says "no error" and
        // the answers are cut off, a no-record — never an answer, never a panic.
        let got = parse_response(&full[..cut], ID);
        assert!(!matches!(got, Reply::Answer(_)), "prefix of {cut} bytes produced an answer");
    }
    assert_eq!(parse_response(&full, ID), Reply::Answer([5, 6, 7, 8]));
}

#[test]
fn a_hostile_rdlength_does_not_run_off_the_end() {
    let mut bogus = rr(5, &[]);
    let n = bogus.len();
    bogus[n - 2..].copy_from_slice(&0xFFFFu16.to_be_bytes()); // rdlength 65535
    let r = response("example.com", 0, &[&bogus]);
    // Lands on NoRecord or Ignore; the point is that it returns.
    assert!(!matches!(parse_response(&r, ID), Reply::Answer(_)));
}

// ---- the loop ----------------------------------------------------------------

/// A scripted network. `plan` maps a server to what it does with a query.
#[derive(Clone)]
enum Server {
    /// Answers the first query it sees, after `after_us`.
    Answers { ip: [u8; 4], after_us: u64 },
    /// Answers `NXDOMAIN` at once.
    Nxdomain,
    /// Never answers.
    Silent,
    /// Drops the first `n` queries, then answers at once.
    Lossy { drop: u32, ip: [u8; 4] },
}

struct Net {
    now: Cell<u64>,
    /// Each poll advances the clock, so a loop that never terminates shows up as
    /// the timeout rather than as a hung test.
    step_us: u64,
    servers: Vec<([u8; 4], Server)>,
    /// `(server, sent_at)` for every query sent, in order.
    sent: RefCell<Vec<([u8; 4], u64)>>,
    inbox: RefCell<Vec<(u64, Vec<u8>)>>,
    refuse_send: bool,
    notes: RefCell<Vec<Note>>,
    host: &'static str,
}

impl Net {
    fn new(host: &'static str, servers: Vec<([u8; 4], Server)>) -> Self {
        Self {
            now: Cell::new(1_000),
            step_us: 10_000,
            servers,
            sent: RefCell::default(),
            inbox: RefCell::default(),
            refuse_send: false,
            notes: RefCell::default(),
            host,
        }
    }

    fn run(&self, timeout_us: u64) -> Result<[u8; 4], ResolveError> {
        let resolvers: Vec<[u8; 4]> = self.servers.iter().map(|(ip, _)| *ip).collect();
        let mut send = |to: [u8; 4], _q: &[u8]| {
            if self.refuse_send {
                return false;
            }
            let at = self.now.get();
            let seen = self.sent.borrow().iter().filter(|(s, _)| *s == to).count() as u32;
            self.sent.borrow_mut().push((to, at));
            let behaviour = &self.servers.iter().find(|(ip, _)| *ip == to).unwrap().1;
            let reply = match behaviour {
                Server::Answers { ip, after_us } if seen == 0 => {
                    Some((at + after_us, response(self.host, 0, &[&a_record(*ip)])))
                }
                Server::Nxdomain if seen == 0 => Some((at, response(self.host, 3, &[]))),
                Server::Lossy { drop, ip } if seen >= *drop => {
                    Some((at, response(self.host, 0, &[&a_record(*ip)])))
                }
                _ => None,
            };
            if let Some(r) = reply {
                self.inbox.borrow_mut().push(r);
            }
            true
        };
        let mut recv = |buf: &mut [u8]| {
            let now = self.now.get();
            let mut inbox = self.inbox.borrow_mut();
            let at = inbox.iter().position(|(ready, _)| *ready <= now)?;
            let (_, data) = inbox.remove(at);
            buf[..data.len()].copy_from_slice(&data);
            Some(data.len())
        };
        let mut poll = || self.now.set(self.now.get() + self.step_us);
        let mut uptime = || self.now.get();
        let mut yield_now = || {};
        let mut note = |n: Note| self.notes.borrow_mut().push(n);
        let mut effects = Effects {
            send: &mut send,
            recv: &mut recv,
            poll_network: &mut poll,
            uptime_us: &mut uptime,
            yield_now: &mut yield_now,
            note: &mut note,
        };
        resolve_a(&mut effects, self.host, &resolvers, timeout_us, ID)
    }
}

const A: [u8; 4] = [10, 0, 2, 3];
const B: [u8; 4] = [1, 1, 1, 1];
const C: [u8; 4] = [8, 8, 8, 8];
const ANSWER: [u8; 4] = [93, 184, 216, 34];

#[test]
fn the_first_server_that_answers_wins_and_the_rest_are_never_asked() {
    let net = Net::new(
        "example.com",
        std::vec![(A, Server::Answers { ip: ANSWER, after_us: 30_000 }), (B, Server::Nxdomain)],
    );
    assert_eq!(net.run(10_000_000), Ok(ANSWER));
    assert_eq!(net.sent.borrow().len(), 1);
}

/// The case this crate exists for: the configured resolver `NXDOMAIN`s a name a
/// later one resolves, and the walk must reach it without waiting out a timeout.
#[test]
fn an_nxdomain_moves_to_the_next_resolver_immediately() {
    let net = Net::new(
        "example.com",
        std::vec![(A, Server::Nxdomain), (B, Server::Answers { ip: ANSWER, after_us: 0 })],
    );
    assert_eq!(net.run(10_000_000), Ok(ANSWER));
    let sent = net.sent.borrow();
    assert_eq!(sent.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [A, B]);
    assert!(sent[1].1 - sent[0].1 < 100_000, "walked on at once, not after a timeout");
    assert_eq!(*net.notes.borrow(), [Note::NoRecord(A)]);
}

#[test]
fn a_silent_server_costs_its_share_of_the_budget_then_the_walk_continues() {
    let net = Net::new(
        "example.com",
        std::vec![(A, Server::Silent), (B, Server::Answers { ip: ANSWER, after_us: 0 })],
    );
    assert_eq!(net.run(4_000_000), Ok(ANSWER));
    let sent = net.sent.borrow();
    let second = sent.iter().find(|(s, _)| *s == B).unwrap().1;
    let first = sent[0].1;
    assert!((2_000_000..2_200_000).contains(&(second - first)), "A got half the budget: {}", second - first);
    assert!(net.notes.borrow().contains(&Note::NoReply(A)));
}

#[test]
fn an_unanswered_query_is_retransmitted_to_the_same_server() {
    let net = Net::new("example.com", std::vec![(A, Server::Lossy { drop: 2, ip: ANSWER })]);
    assert_eq!(net.run(10_000_000), Ok(ANSWER));
    let sent = net.sent.borrow();
    assert_eq!(sent.len(), 3, "original + two retransmits");
    assert!(sent.iter().all(|(s, _)| *s == A));
    let gap = sent[1].1 - sent[0].1;
    assert!((RETRANSMIT_US..RETRANSMIT_US + 100_000).contains(&gap), "retransmit gap {gap}");
}

#[test]
fn every_server_saying_no_is_no_record() {
    let net = Net::new("nope.example", std::vec![(A, Server::Nxdomain), (B, Server::Nxdomain)]);
    assert_eq!(net.run(10_000_000), Err(ResolveError::NoRecord));
}

#[test]
fn a_server_that_never_spoke_outranks_one_that_said_no() {
    // "Does not exist" would claim more than was learned.
    let net = Net::new("example.com", std::vec![(A, Server::Nxdomain), (B, Server::Silent)]);
    assert_eq!(net.run(2_000_000), Err(ResolveError::Timeout));
}

#[test]
fn nobody_answering_is_a_timeout_inside_the_budget() {
    let net = Net::new("example.com", std::vec![(A, Server::Silent), (B, Server::Silent), (C, Server::Silent)]);
    let started = net.now.get();
    assert_eq!(net.run(3_000_000), Err(ResolveError::Timeout));
    let spent = net.now.get() - started;
    assert!((3_000_000..3_300_000).contains(&spent), "spent {spent} of a 3 s budget");
}

#[test]
fn a_tiny_budget_still_gives_each_server_its_floor() {
    let net = Net::new("example.com", std::vec![(A, Server::Silent), (B, Server::Silent)]);
    let started = net.now.get();
    assert_eq!(net.run(1), Err(ResolveError::Timeout));
    assert!(net.now.get() - started >= 2 * MIN_PER_SERVER_US, "each server gets {MIN_PER_SERVER_US} us");
}

#[test]
fn when_nothing_can_be_sent_nothing_is_awaited() {
    let mut net = Net::new("example.com", std::vec![(A, Server::Silent), (B, Server::Silent)]);
    net.refuse_send = true;
    let started = net.now.get();
    assert_eq!(net.run(10_000_000), Err(ResolveError::SendFailed));
    assert_eq!(net.now.get(), started, "no poll, no wait");
    assert_eq!(*net.notes.borrow(), [Note::SendFailed(A), Note::SendFailed(B)]);
}

#[test]
fn a_late_answer_from_a_skipped_server_is_still_taken() {
    // Replies are matched on the query id, not the sender: a server we gave up
    // on that answers while we wait on the next one is an answer to our question.
    let net = Net::new(
        "example.com",
        std::vec![(A, Server::Answers { ip: ANSWER, after_us: 600_000 }), (B, Server::Silent)],
    );
    // A's share of a 1 s budget is the 0.5 s floor, so B is being awaited when
    // A's reply lands at 0.6 s.
    assert_eq!(net.run(1_000_000), Ok(ANSWER));
}

#[test]
fn bad_input_is_refused_before_anything_is_sent() {
    let net = Net::new("a..b", std::vec![(A, Server::Silent)]);
    assert_eq!(net.run(1_000_000), Err(ResolveError::BadName));
    assert!(net.sent.borrow().is_empty());
    let none = Net::new("example.com", Vec::new());
    assert_eq!(none.run(1_000_000), Err(ResolveError::NoResolvers));
}
