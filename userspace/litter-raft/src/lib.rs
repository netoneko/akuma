//! Raft's leader-election subprotocol, and nothing else — no log, no log
//! replication, no snapshotting, no persistence. That's deliberate: a meow
//! litter doesn't have replicated state that needs to survive a node dying,
//! it just needs the agents to agree on who's "leader" right now. See
//! `userspace/meow/docs/LITTER_EXPERIMENT.md` for why a full Raft crate was
//! rejected (none of the no_std-tagged ones are both maintained and no_std in
//! practice) and why hand-rolling only this piece is the right size of thing
//! to build.
//!
//! This crate is pure logic and does no I/O: no sockets, no files, no clock.
//! A caller owns the transport and the timer (when to call `start_election`,
//! e.g. "no heartbeat for N ms"); this crate only decides what a state
//! transition does. That split is what makes it `no_std` + `alloc` with zero
//! dependencies, and it's what makes the `tests` module below able to run as
//! an ordinary native `cargo test` on macOS — meow itself can't (it's
//! `#![no_std]` end to end and issues real Linux syscalls, so it only runs
//! under `aarch64-unknown-linux-musl`, in practice inside Docker's Linux VM;
//! see `userspace/meow/README.md`'s "linux-net build" section). This crate
//! has no libakuma dependency at all, so the one thing that would otherwise
//! need Docker to exercise — several agents' election state machines talking
//! over a transport — can be simulated in-process with a mocked transport and
//! run directly on the host.
//!
//! `userspace/meow/src/tools/litter/raft.rs` re-exports this crate rather
//! than defining the types itself — this is the "extract into a crate later"
//! the litter experiment always intended, just done now instead of later,
//! matching how `akuma-cow`'s write-fault decision or `akuma-syscalls-sync`'s
//! futex algebra were built inline first and pulled into their own crate once
//! the seam proved itself (see the root `CLAUDE.md`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

pub struct RequestVote {
    pub term: u64,
    pub candidate: String,
}

pub struct VoteResponse {
    pub term: u64,
    pub voter: String,
    pub granted: bool,
}

pub struct Heartbeat {
    pub term: u64,
    pub leader: String,
}

pub struct ElectionState {
    pub term: u64,
    pub role: Role,
    pub voted_for: Option<String>,
    pub leader: Option<String>,
    /// Voters (deduplicated) who granted this node a vote in the current
    /// term. Only meaningful while `role == Candidate`; cleared on every term
    /// change.
    votes: Vec<String>,
}

/// `peer_count` is the number of voting members INCLUDING self — a 3-node
/// litter passes 3, not 2. Majority of 1 is 1 (a lone node is trivially its
/// own leader); majority of 2 is 2 (no such thing as a majority of one half
/// of two voters — this deliberately can't split-brain a 2-node litter the
/// way real Raft can't either).
fn majority(peer_count: usize) -> usize {
    peer_count / 2 + 1
}

impl ElectionState {
    pub fn new() -> Self {
        ElectionState { term: 0, role: Role::Follower, voted_for: None, leader: None, votes: Vec::new() }
    }

    /// A message claiming a higher term always wins: step down to Follower,
    /// adopt the term, and forget this term's vote/candidacy. Called before
    /// handling any incoming message's term-specific logic.
    fn see_term(&mut self, term: u64) {
        if term > self.term {
            self.term = term;
            self.role = Role::Follower;
            self.voted_for = None;
            self.votes.clear();
        }
    }

    /// Become a candidate for the next term, vote for self, and return the
    /// `RequestVote` to broadcast to every peer. The caller decides *when*
    /// this fires (an election-timeout policy lives outside this crate).
    pub fn start_election(&mut self, self_id: &str) -> RequestVote {
        self.term += 1;
        self.role = Role::Candidate;
        self.voted_for = Some(String::from(self_id));
        self.leader = None;
        self.votes.clear();
        self.votes.push(String::from(self_id));
        RequestVote { term: self.term, candidate: String::from(self_id) }
    }

    /// Decide whether to grant a vote. At most one grant per term (Raft's
    /// core safety property): once `voted_for` is set for this term, only a
    /// repeated request from that same candidate is granted again (handles a
    /// retransmitted request, not a second candidate).
    pub fn on_request_vote(&mut self, req: &RequestVote) -> VoteResponse {
        self.see_term(req.term);

        if req.term < self.term {
            return VoteResponse { term: self.term, voter: String::new(), granted: false };
        }

        let granted = match &self.voted_for {
            None => true,
            Some(existing) => existing == &req.candidate,
        };
        if granted {
            self.voted_for = Some(req.candidate.clone());
        }
        VoteResponse { term: self.term, voter: String::new(), granted }
    }

    /// Fold in a vote response. Returns `true` exactly on the call that
    /// crosses the majority threshold and makes this node the leader — a
    /// one-shot edge, not a "currently leader" query (use `self.role` or
    /// `is_leader()` for that).
    pub fn on_vote_response(&mut self, resp: &VoteResponse, peer_count: usize) -> bool {
        if resp.term != self.term || self.role != Role::Candidate || !resp.granted {
            return false;
        }
        if !self.votes.iter().any(|v| v == &resp.voter) {
            self.votes.push(resp.voter.clone());
        }
        if self.votes.len() >= majority(peer_count) {
            self.role = Role::Leader;
            self.leader = Some(self.voted_for.clone().unwrap_or_default());
            return true;
        }
        false
    }

    /// A leader's heartbeat resets this node to Follower and clears any
    /// candidacy — a stale (lower-term) heartbeat from a leader that lost an
    /// election it doesn't know about yet is ignored rather than accepted.
    pub fn on_heartbeat(&mut self, hb: &Heartbeat) {
        if hb.term < self.term {
            return;
        }
        self.see_term(hb.term);
        self.term = hb.term;
        self.role = Role::Follower;
        self.leader = Some(hb.leader.clone());
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    /// The privilege check the whole crate exists for: is `sender` allowed to
    /// issue a command that only the leader may issue? `root`, when present,
    /// always is — a fixed identity (today: a name; later: a verified pubkey
    /// once `LitterSend` carries signatures) that outranks whatever the
    /// current elected term says. This is not a protocol, just a priority
    /// rule ahead of the leader check.
    pub fn is_authorized(&self, sender: &str, root: Option<&str>) -> bool {
        if let Some(root_id) = root {
            if sender == root_id {
                return true;
            }
        }
        self.leader.as_deref() == Some(sender)
    }
}

impl Default for ElectionState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_election_bumps_term_and_votes_for_self() {
        let mut s = ElectionState::new();
        let req = s.start_election("a");
        assert_eq!(s.term, 1);
        assert_eq!(s.role, Role::Candidate);
        assert_eq!(s.voted_for.as_deref(), Some("a"));
        assert_eq!(req.term, 1);
        assert_eq!(req.candidate, "a");
    }

    #[test]
    fn three_node_majority_elects_a_leader() {
        let mut s = ElectionState::new();
        s.start_election("a");
        let became_leader = s.on_vote_response(&VoteResponse { term: 1, voter: String::from("b"), granted: true }, 3);
        assert!(became_leader);
        assert!(s.is_leader());
        assert_eq!(s.leader.as_deref(), Some("a"));
    }

    #[test]
    fn split_vote_among_four_nodes_does_not_cross_majority() {
        let mut s = ElectionState::new();
        s.start_election("a");
        s.on_vote_response(&VoteResponse { term: 1, voter: String::from("b"), granted: true }, 4);
        let became_leader = s.on_vote_response(&VoteResponse { term: 1, voter: String::from("c"), granted: false }, 4);
        assert!(!became_leader);
        assert_eq!(s.role, Role::Candidate);
    }

    #[test]
    fn one_vote_per_term_refuses_a_second_candidate() {
        let mut s = ElectionState::new();
        let r1 = s.on_request_vote(&RequestVote { term: 1, candidate: String::from("a") });
        let r2 = s.on_request_vote(&RequestVote { term: 1, candidate: String::from("b") });
        assert!(r1.granted);
        assert!(!r2.granted);
    }

    #[test]
    fn retransmitted_request_from_same_candidate_is_granted_again() {
        let mut s = ElectionState::new();
        let r1 = s.on_request_vote(&RequestVote { term: 1, candidate: String::from("a") });
        let r2 = s.on_request_vote(&RequestVote { term: 1, candidate: String::from("a") });
        assert!(r1.granted);
        assert!(r2.granted);
    }

    #[test]
    fn higher_term_forces_step_down_even_from_leader() {
        let mut s = ElectionState::new();
        s.start_election("a");
        s.on_vote_response(&VoteResponse { term: 1, voter: String::from("b"), granted: true }, 3);
        assert!(s.is_leader());
        let resp = s.on_request_vote(&RequestVote { term: 5, candidate: String::from("c") });
        assert_eq!(s.role, Role::Follower);
        assert_eq!(s.term, 5);
        assert!(resp.granted);
    }

    #[test]
    fn stale_heartbeat_is_ignored() {
        let mut s = ElectionState::new();
        s.start_election("a"); // term 1
        s.start_election("a"); // term 2
        s.on_heartbeat(&Heartbeat { term: 1, leader: String::from("stale-leader") });
        assert_eq!(s.term, 2);
        assert_ne!(s.leader.as_deref(), Some("stale-leader"));
    }

    #[test]
    fn current_term_heartbeat_is_adopted() {
        let mut s = ElectionState::new();
        s.start_election("a");
        s.on_heartbeat(&Heartbeat { term: 1, leader: String::from("b") });
        assert_eq!(s.role, Role::Follower);
        assert_eq!(s.leader.as_deref(), Some("b"));
    }

    #[test]
    fn is_authorized_checks_the_elected_leader() {
        let mut s = ElectionState::new();
        s.on_heartbeat(&Heartbeat { term: 1, leader: String::from("b") });
        assert!(s.is_authorized("b", None));
        assert!(!s.is_authorized("c", None));
    }

    #[test]
    fn is_authorized_lets_root_override_a_different_elected_leader() {
        let mut s = ElectionState::new();
        s.on_heartbeat(&Heartbeat { term: 1, leader: String::from("b") });
        assert!(s.is_authorized("root-key", Some("root-key")));
        assert!(!s.is_authorized("root-key", None));
    }

    /// The "processing loop" simulation: three independent `ElectionState`
    /// machines, wired together with nothing but direct method calls playing
    /// the part of a transport — no sockets, no threads, no async runtime.
    /// This is the mock the doc comment above refers to: standing in for
    /// whatever real transport eventually carries these messages (the
    /// planned TCP hub, per `LITTER_EXPERIMENT.md`) so the full
    /// campaign-vote-heartbeat cycle can be driven to convergence and
    /// asserted on, on a host that can't run meow itself.
    #[test]
    fn simulated_election_converges_across_three_nodes_over_mock_transport() {
        struct Node {
            id: &'static str,
            state: ElectionState,
        }
        let mut nodes = [
            Node { id: "a", state: ElectionState::new() },
            Node { id: "b", state: ElectionState::new() },
            Node { id: "c", state: ElectionState::new() },
        ];

        // "a"'s election timer fires first; it campaigns.
        let req = nodes[0].state.start_election(nodes[0].id);

        // Mock transport: broadcast the RequestVote to the other two peers
        // and collect their responses — in a real transport this is N
        // network round-trips, here it's just N function calls.
        let responses: Vec<VoteResponse> = nodes[1..]
            .iter_mut()
            .map(|n| n.state.on_request_vote(&req))
            .collect();

        // Deliver each response back to the candidate over the same mock link.
        let mut became_leader = false;
        for resp in &responses {
            if nodes[0].state.on_vote_response(resp, nodes.len()) {
                became_leader = true;
            }
        }
        assert!(became_leader, "candidate should reach majority with 2 of 3 votes");
        assert!(nodes[0].state.is_leader());

        // The new leader's heartbeat, delivered to the other two over the
        // mock transport, brings them around to recognizing it.
        let hb = Heartbeat { term: nodes[0].state.term, leader: String::from(nodes[0].id) };
        for node in nodes[1..].iter_mut() {
            node.state.on_heartbeat(&hb);
        }
        for node in &nodes {
            assert_eq!(node.state.leader.as_deref(), Some("a"), "node {} did not converge on the leader", node.id);
        }

        // Root override holds on every node regardless of who's leader —
        // this is the property the whole "your key outranks the elected
        // leader" design depends on.
        for node in &nodes {
            assert!(node.state.is_authorized("root-key", Some("root-key")), "node {} should honor root override", node.id);
        }

        // And a non-root, non-leader sender is authorized on none of them.
        for node in &nodes {
            assert!(!node.state.is_authorized("mallory", None), "node {} should not authorize a non-leader, non-root sender", node.id);
        }
    }

    /// A leader that goes silent (no more heartbeats over the mock
    /// transport) lets a follower time out and start a new campaign in the
    /// next term, which the old leader itself must accept once it sees the
    /// higher term — this is what actually recovers from a leader dying,
    /// not anything the transport layer does.
    #[test]
    fn simulated_reelection_after_leader_goes_silent() {
        struct Node {
            id: &'static str,
            state: ElectionState,
        }
        let mut nodes = [
            Node { id: "a", state: ElectionState::new() },
            Node { id: "b", state: ElectionState::new() },
            Node { id: "c", state: ElectionState::new() },
        ];

        let req = nodes[0].state.start_election(nodes[0].id);
        let responses: Vec<VoteResponse> = nodes[1..].iter_mut().map(|n| n.state.on_request_vote(&req)).collect();
        for resp in &responses {
            nodes[0].state.on_vote_response(resp, 3);
        }
        assert!(nodes[0].state.is_leader());

        // "a" goes silent (crashed, network partitioned, whatever) — no
        // heartbeat ever arrives. "b" times out and campaigns for the next term.
        let req2 = nodes[1].state.start_election(nodes[1].id);

        // "c" grants it (higher term, hasn't voted this term).
        let resp_c = nodes[2].state.on_request_vote(&req2);
        assert!(resp_c.granted);

        // The old leader "a" also eventually sees the higher-term request
        // (e.g. once the partition heals) and must step down and grant it too
        // — Raft's safety property doesn't make an exception for "I used to
        // be leader".
        let resp_a = nodes[0].state.on_request_vote(&req2);
        assert!(resp_a.granted);
        assert!(!nodes[0].state.is_leader());

        let became_leader = nodes[1].state.on_vote_response(&resp_c, 3) || nodes[1].state.on_vote_response(&resp_a, 3);
        assert!(became_leader);
        assert_eq!(nodes[1].state.leader.as_deref(), Some("b"));
    }
}
