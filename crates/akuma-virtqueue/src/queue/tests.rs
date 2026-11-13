//! The split queue against a simulated device.
//!
//! [`Dev`] plays the device's half of §2.7: it consumes the available ring from
//! its own index, walks each chain by the `next` fields *in shared memory* (as a
//! real device does), and completes requests in whatever order a test asks.

// q = the queue, m = its memory, d = the device: the same three in every test.
#![allow(clippy::many_single_char_names)]

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

const Q: usize = 8;
type Sq = SplitQueue<Q>;

struct Mem(RefCell<Vec<u16>>);

impl Mem {
    fn new() -> Self {
        Self(RefCell::new(vec![0xdead; Sq::LAYOUT.words()]))
    }
}

impl QueueMem for Mem {
    fn read(&self, w: usize) -> u16 {
        self.0.borrow()[w]
    }
    fn write(&self, w: usize, v: u16) {
        self.0.borrow_mut()[w] = v;
    }
}

/// One descriptor as the device reads it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

struct Dev {
    last_avail: u16,
    used_idx: u16,
    /// Heads taken from the available ring, not yet completed.
    pending: VecDeque<u16>,
}

impl Dev {
    fn new() -> Self {
        Self { last_avail: 0, used_idx: 0, pending: VecDeque::new() }
    }

    fn desc(m: &Mem, i: u16) -> Desc {
        let w = Sq::LAYOUT.desc / 2 + 8 * i as usize;
        let r = |k| u64::from(m.read(w + k));
        Desc {
            addr: r(0) | r(1) << 16 | r(2) << 32 | r(3) << 48,
            len: (r(4) | r(5) << 16) as u32,
            flags: m.read(w + 6),
            next: m.read(w + 7),
        }
    }

    /// The chain headed at `head`, following the table's `next` fields.
    fn chain(m: &Mem, head: u16) -> Vec<Desc> {
        let mut out = vec![Self::desc(m, head)];
        while out.last().unwrap().flags & DESC_F_NEXT != 0 {
            assert!(out.len() <= Q, "descriptor loop");
            out.push(Self::desc(m, out.last().unwrap().next));
        }
        out
    }

    /// Take everything newly available.
    fn take(&mut self, m: &Mem) {
        let avail_idx = m.read(Sq::LAYOUT.avail / 2 + 1);
        while self.last_avail != avail_idx {
            let slot = Sq::LAYOUT.avail / 2 + 2 + self.last_avail as usize % Q;
            self.pending.push_back(m.read(slot));
            self.last_avail = self.last_avail.wrapping_add(1);
        }
    }

    /// Write a used element `{id, len}` and publish it.
    fn complete_raw(&mut self, m: &Mem, id: u32, len: u32) {
        let e = Sq::LAYOUT.used / 2 + 2 + 4 * (self.used_idx as usize % Q);
        m.write(e, id as u16);
        m.write(e + 1, (id >> 16) as u16);
        m.write(e + 2, len as u16);
        m.write(e + 3, (len >> 16) as u16);
        self.used_idx = self.used_idx.wrapping_add(1);
        m.write(Sq::LAYOUT.used / 2 + 1, self.used_idx);
    }

    fn complete(&mut self, m: &Mem, head: u16, len: u32) {
        let at = self.pending.iter().position(|&h| h == head).expect("completing a head the device never took");
        self.pending.remove(at);
        self.complete_raw(m, u32::from(head), len);
    }
}

fn setup() -> (Sq, Mem, Dev) {
    let m = Mem::new();
    let mut q = Sq::new();
    q.reset(&m);
    (q, m, Dev::new())
}

fn rd(addr: u64, len: u32) -> Buf {
    Buf { addr, len, device_writes: false }
}

fn wr(addr: u64, len: u32) -> Buf {
    Buf { addr, len, device_writes: true }
}

#[test]
fn layout_matches_the_spec_arithmetic() {
    // Q = 16: table 256 B, available ring 6 + 32 = 38 B (ends at 294), used ring
    // rounded to 296 and 6 + 128 B long.
    assert_eq!(Layout::new(16), Layout { desc: 0, avail: 256, used: 296, bytes: 430 });
    assert_eq!(Layout::new(16).words(), 215);
    for q in [1usize, 2, 4, 8, 64, 256, 32768] {
        let l = Layout::new(q);
        assert_eq!(l.desc % 16, 0);
        assert_eq!(l.avail % 2, 0);
        assert_eq!(l.used % 4, 0);
        assert!(l.used >= l.avail + 6 + 2 * q);
    }
}

#[test]
fn reset_zeroes_every_cell() {
    let (_, m, _) = setup();
    assert!(m.0.borrow().iter().all(|&w| w == 0));
}

#[test]
fn a_three_buffer_request_is_one_chain_with_the_right_flags() {
    let (mut q, m, mut d) = setup();
    let head = q.add(&m, &[rd(0x1000, 16), wr(0x2000_0000_0000, 4096), wr(0x3000, 1)]).unwrap();
    assert_eq!(q.num_free(), Q - 3);
    d.take(&m);
    assert_eq!(d.pending, [head]);
    let c = Dev::chain(&m, head);
    assert_eq!(c.len(), 3);
    assert_eq!((c[0].addr, c[0].len, c[0].flags), (0x1000, 16, DESC_F_NEXT));
    assert_eq!((c[1].addr, c[1].len, c[1].flags), (0x2000_0000_0000, 4096, DESC_F_NEXT | DESC_F_WRITE));
    assert_eq!((c[2].addr, c[2].len, c[2].flags), (0x3000, 1, DESC_F_WRITE));
    assert!(q.in_flight(head));

    assert!(!q.has_used(&m));
    d.complete(&m, head, 4097);
    assert_eq!(q.peek_used(&m), Some(u32::from(head)));
    assert_eq!(q.pop_used(&m), Some(Ok((head, 4097))));
    assert_eq!(q.num_free(), Q);
    assert!(!q.in_flight(head));
    assert_eq!(q.pop_used(&m), None);
}

#[test]
fn an_empty_request_and_an_overfull_one_are_refused_without_side_effects() {
    let (mut q, m, mut d) = setup();
    assert_eq!(q.add(&m, &[]), Err(AddError::Empty));
    let bufs = vec![rd(0, 1); Q + 1];
    assert_eq!(q.add(&m, &bufs), Err(AddError::Full));
    assert_eq!(q.num_free(), Q);
    d.take(&m);
    assert!(d.pending.is_empty(), "a refused add published nothing");
}

#[test]
fn the_queue_fills_and_drains() {
    let (mut q, m, mut d) = setup();
    let heads: Vec<u16> = (0..Q).map(|i| q.add(&m, &[wr(i as u64, 1)]).unwrap()).collect();
    assert_eq!(q.num_free(), 0);
    assert_eq!(q.add(&m, &[rd(0, 1)]), Err(AddError::Full));
    d.take(&m);
    for &h in &heads {
        d.complete(&m, h, 1);
    }
    let mut got: Vec<u16> = std::iter::from_fn(|| q.pop_used(&m)).map(|r| r.unwrap().0).collect();
    got.sort_unstable();
    let mut want = heads;
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(q.num_free(), Q);
}

#[test]
fn completion_out_of_order_frees_each_chain_whole() {
    let (mut q, m, mut d) = setup();
    let a = q.add(&m, &[rd(1, 1), wr(2, 2)]).unwrap();
    let b = q.add(&m, &[rd(3, 3), wr(4, 4), wr(5, 5)]).unwrap();
    let c = q.add(&m, &[wr(6, 6)]).unwrap();
    assert_eq!(q.num_free(), Q - 6);
    d.take(&m);
    d.complete(&m, b, 9);
    d.complete(&m, c, 6);
    assert_eq!(q.pop_used(&m), Some(Ok((b, 9))));
    assert_eq!(q.num_free(), Q - 3);
    assert_eq!(q.pop_used(&m), Some(Ok((c, 6))));
    // b's three and c's one rejoin the two never used, while a still holds
    // two: a six-buffer request fits exactly, a seventh descriptor does not.
    assert_eq!(q.num_free(), Q - 2);
    assert_eq!(q.add(&m, &[rd(0, 1); 7]), Err(AddError::Full));
    let e = q.add(&m, &[rd(0, 1); 6]).unwrap();
    d.take(&m);
    assert_eq!(Dev::chain(&m, e).len(), 6);
    d.complete(&m, a, 2);
    d.complete(&m, e, 0);
    assert_eq!(q.pop_used(&m), Some(Ok((a, 2))));
    assert_eq!(q.pop_used(&m), Some(Ok((e, 0))));
    assert_eq!(q.num_free(), Q);
}

#[test]
fn indices_wrap_past_u16_without_losing_a_descriptor() {
    let (mut q, m, mut d) = setup();
    // 70,000 requests cross both 16-bit indices' wrap; chain lengths and
    // completion order vary so the free list is reshuffled throughout.
    let mut inflight: VecDeque<u16> = VecDeque::new();
    for i in 0..70_000u32 {
        let n = 1 + (i as usize * 7) % 3;
        while q.num_free() < n {
            d.take(&m);
            // complete the newest first every other round, the oldest otherwise
            let h = if i % 2 == 0 { inflight.pop_back() } else { inflight.pop_front() }.unwrap();
            d.complete(&m, h, i);
            let (got, len) = q.pop_used(&m).unwrap().unwrap();
            assert_eq!((got, len), (h, i));
        }
        let bufs: Vec<Buf> = (0..n).map(|k| wr(u64::from(i) << 8 | k as u64, k as u32 + 1)).collect();
        let h = q.add(&m, &bufs).unwrap();
        d.take(&m);
        assert_eq!(Dev::chain(&m, h).iter().map(|c| c.addr).collect::<Vec<_>>(),
                   bufs.iter().map(|b| b.addr).collect::<Vec<_>>());
        inflight.push_back(h);
    }
    while let Some(h) = inflight.pop_front() {
        d.complete(&m, h, 0);
        assert_eq!(q.pop_used(&m).unwrap().unwrap().0, h);
    }
    assert_eq!(q.num_free(), Q);
    // and the bookkeeping still holds a full queue's worth of distinct chains
    let heads: Vec<u16> = (0..Q).map(|_| q.add(&m, &[rd(0, 1)]).unwrap()).collect();
    let mut sorted = heads;
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), Q);
}

#[test]
fn a_used_element_naming_no_request_is_refused_and_frees_nothing() {
    let (mut q, m, mut d) = setup();
    let a = q.add(&m, &[rd(1, 1), wr(2, 2)]).unwrap();
    d.take(&m);
    let free = q.num_free();
    d.complete_raw(&m, Q as u32 + 3, 0);
    assert_eq!(q.pop_used(&m), Some(Err(UsedError::BadId(Q as u32 + 3))));
    // a descriptor that is free, or inside a chain but not its head
    let idle = (0..Q as u16).find(|&i| !q.in_flight(i) && i != a).unwrap();
    d.complete_raw(&m, u32::from(idle), 0);
    assert_eq!(q.pop_used(&m), Some(Err(UsedError::NotInFlight(idle))));
    assert_eq!(q.num_free(), free);
    // the real completion still lands
    d.complete(&m, a, 2);
    assert_eq!(q.pop_used(&m), Some(Ok((a, 2))));
    // and completing it twice is refused the second time
    d.complete_raw(&m, u32::from(a), 2);
    assert_eq!(q.pop_used(&m), Some(Err(UsedError::NotInFlight(a))));
    assert_eq!(q.num_free(), Q);
}

#[test]
fn pop_used_expect_leaves_another_requests_element_in_place() {
    let (mut q, m, mut d) = setup();
    let a = q.add(&m, &[wr(1, 64)]).unwrap();
    let b = q.add(&m, &[wr(2, 64)]).unwrap();
    d.take(&m);
    d.complete(&m, b, 60);
    assert_eq!(q.pop_used_expect(&m, a), Some(Err(UsedError::WrongToken { expected: a, got: u32::from(b) })));
    assert_eq!(q.pop_used_expect(&m, b), Some(Ok(60)));
    assert_eq!(q.pop_used_expect(&m, a), None);
    d.complete(&m, a, 64);
    assert_eq!(q.pop_used_expect(&m, a), Some(Ok(64)));
}

#[test]
fn notify_is_suppressed_only_when_the_device_says_so() {
    let (mut q, m, _) = setup();
    q.add(&m, &[rd(0, 1)]).unwrap();
    assert!(q.should_notify(&m));
    m.write(Sq::LAYOUT.used / 2, USED_F_NO_NOTIFY);
    assert!(!q.should_notify(&m));
}
