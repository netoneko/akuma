//! The bring-up, replayed against Linux's own register trace of the same card.
//!
//! `golden/w0_up.txt` is every access Linux's `rtw89` made bringing ryzen's
//! RTL8852CE from powered-off to running firmware (stage W0). [`Replay`]
//! serves reads from it in order and requires every write to match it —
//! register, width and value — so [`bring_up`] passes only if it does what
//! Linux did, in the order Linux did it. Two allowances, both forced:
//!
//! - **Ring base addresses** (`*_DESA_L/H`) are DMA addresses: Linux's were
//!   IOMMU addresses, ours are whatever the test's buffers are. Any value is
//!   accepted, and recorded so the test can check they are the buffers given.
//! - **Polls.** A poll that Linux satisfied on its Nth read may be satisfied
//!   on a different read here; runs of identical reads are folded into one line
//!   in the fixture, and a read of the register the previous read consumed
//!   gets that value again rather than advancing.
//!
//! The firmware is synthetic: zeros in the shape of ryzen's real image
//! (`rtw8852c_fw-1.bin`, cut-1 normal: three sections of 315 176, 15 504 and
//! 960 bytes behind a 144-byte header with a dynamic part), so it produces the
//! same 166 data packets as the trace's 166 kicks. What the packets carry is
//! `sim_chip.rs`'s business.

use std::collections::BTreeMap;

use akuma_rtw89::bringup::{self, Dma, RING_BYTES, SLOT};
use akuma_rtw89::{Bus, regs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    R,
    W,
}

#[derive(Clone, Copy, Debug)]
struct Access {
    op: Op,
    width: u8,
    off: u32,
    val: u32,
    line: usize,
}

fn load() -> Vec<Access> {
    let text = include_str!("golden/w0_up.txt");
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if l.starts_with('#') || l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        let (op, width) = f[0].split_at(1);
        out.push(Access {
            op: if op == "R" { Op::R } else { Op::W },
            width: width.parse().unwrap(),
            off: u32::from_str_radix(f[1].trim_start_matches("0x"), 16).unwrap(),
            val: u32::from_str_radix(f[3].trim_start_matches("0x"), 16).unwrap(),
            line: i + 1,
        });
    }
    out
}

struct Replay {
    trace: Vec<Access>,
    at: usize,
    last_read: Option<Access>,
    ring_bases: BTreeMap<u32, u32>,
}

impl Replay {
    fn new() -> Self {
        Self { trace: load(), at: 0, last_read: None, ring_bases: BTreeMap::new() }
    }

    fn read(&mut self, width: u8, off: u32) -> u32 {
        if let Some(a) = self.trace.get(self.at)
            && a.op == Op::R
            && a.width == width
            && a.off == off
        {
            self.at += 1;
            self.last_read = Some(*a);
            return a.val;
        }
        if let Some(a) = self.last_read
            && a.width == width
            && a.off == off
        {
            return a.val;
        }
        panic!(
            "unexpected R{width} 0x{off:05x}; the trace has, at line {}: {:?}",
            self.trace.get(self.at).map_or(0, |a| a.line),
            self.trace.get(self.at)
        );
    }

    fn write(&mut self, width: u8, off: u32, val: u32) {
        let Some(a) = self.trace.get(self.at).copied() else {
            panic!("W{width} 0x{off:05x} = 0x{val:x} after the end of the trace");
        };
        let same_place = a.op == Op::W && a.width == width && a.off == off;
        let same_value = a.val == val || regs::is_ring_base(off);
        assert!(
            same_place && same_value,
            "W{width} 0x{off:05x} = 0x{val:x}, but the trace has, at line {}: {:?} 0x{:05x} = 0x{:x}",
            a.line,
            a.op,
            a.off,
            a.val
        );
        if regs::is_ring_base(off) {
            self.ring_bases.insert(off, val);
        }
        self.at += 1;
        self.last_read = None;
    }
}

impl Bus for Replay {
    fn read8(&mut self, off: u32) -> u8 {
        self.read(8, off) as u8
    }
    fn read16(&mut self, off: u32) -> u16 {
        self.read(16, off) as u16
    }
    fn read32(&mut self, off: u32) -> u32 {
        self.read(32, off)
    }
    fn write8(&mut self, off: u32, v: u8) {
        self.write(8, off, u32::from(v));
    }
    fn write16(&mut self, off: u32, v: u16) {
        self.write(16, off, u32::from(v));
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.write(32, off, v);
    }
    fn delay_us(&mut self, _: u32) {}
}

/// A container shaped like ryzen's `rtw8852c_fw-1.bin`: a cut-0 and a cut-2
/// decoy around the cut-1 normal image, which has the real image's header
/// (dynamic part included) and section sizes.
fn synthetic_firmware() -> Vec<u8> {
    const SECTIONS: [(u32, u32); 3] = [(2, 315_176), (1, 15_504), (9, 960)];
    let base_hdr = 32 + 16 * SECTIONS.len();
    let hdr_total = 144;
    let image_len = hdr_total + SECTIONS.iter().map(|s| s.1 as usize).sum::<usize>();
    let entries = [(0u8, 1u8), (1, 1), (2, 1)];
    let first = 16 + 16 * entries.len();
    let mut f = vec![0u8; first + image_len * entries.len()];
    f[0] = 0xff;
    f[1] = entries.len() as u8;
    for (i, &(cv, ty)) in entries.iter().enumerate() {
        let shift = first + image_len * i;
        let e = 16 + 16 * i;
        f[e] = cv;
        f[e + 1] = ty;
        f[e + 4..e + 8].copy_from_slice(&(shift as u32).to_le_bytes());
        f[e + 8..e + 12].copy_from_slice(&(image_len as u32).to_le_bytes());
        let h = &mut f[shift..shift + hdr_total];
        h[4..8].copy_from_slice(&[0, 27, 122, 0]);
        h[14] = hdr_total as u8; // w3[23:16]: header length incl. dynamic part
        h[25] = SECTIONS.len() as u8; // w6[15:8]
        h[28..32].copy_from_slice(&(1u32 << 16).to_le_bytes()); // w7: DYN_HDR
        for (j, &(ty, len)) in SECTIONS.iter().enumerate() {
            let s = 32 + 16 * j;
            h[s + 4..s + 8].copy_from_slice(&(len | (ty << 24)).to_le_bytes());
        }
        let dyn_len = (hdr_total - base_hdr) as u32;
        h[base_hdr..base_hdr + 4].copy_from_slice(&dyn_len.to_le_bytes());
    }
    f
}

#[test]
fn bring_up_matches_linux_access_for_access() {
    let file = synthetic_firmware();
    let mut src: &[u8] = &file;
    let mut ring = vec![0u8; RING_BYTES];
    let mut slots = vec![0u8; SLOT * 32];
    let mut dma = Dma {
        ring: &mut ring,
        ring_phys: 0x1_0000,
        slots: &mut slots,
        slots_phys: 0x2_0000,
        idle_phys: 0x3_0000,
        rx_phys: [0x4_0000, 0x5_0000],
    };
    let mut bus = Replay::new();
    let mut steps = 0;
    let report = bringup::bring_up(&mut bus, &mut dma, &mut src, &mut |_: &'static str, _: u32| {
        steps += 1;
    })
    .unwrap_or_else(|e| panic!("bring-up failed: {e:?} at trace line {}", bus.trace[bus.at].line));

    assert_eq!(bus.at, bus.trace.len(), "the trace has accesses the bring-up never made");
    assert_eq!(report.cv, 1);
    assert_eq!(report.data_packets, 166);
    assert_eq!(report.version, [0, 27, 122, 0]);
    assert_eq!(report.fw_ctrl, 0xe2);
    assert!(steps > 0, "the bring-up logged nothing");

    // CH12 points at the ring given; every other TX channel at the idle ring;
    // RXQ and RPQ at theirs; all high words zero.
    let ch12 = regs::TX_RINGS[12].desa_l;
    assert_eq!(bus.ring_bases[&ch12], 0x1_0000);
    for ring in &regs::TX_RINGS[..12] {
        assert_eq!(bus.ring_bases[&ring.desa_l], 0x3_0000);
    }
    assert_eq!(bus.ring_bases[&regs::RX_RINGS[0].1], 0x4_0000);
    assert_eq!(bus.ring_bases[&regs::RX_RINGS[1].1], 0x5_0000);
    assert!(bus.ring_bases.iter().filter(|(k, _)| *k % 8 == 4).all(|(_, v)| *v == 0));
}
