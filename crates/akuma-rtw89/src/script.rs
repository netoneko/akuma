//! Replaying a recorded bring-up: the rest of Linux's `rtw89_core_start` after
//! the firmware is up, as an op stream.
//!
//! # Why a recording
//!
//! After `fw ready`, Linux spends 89 ms and ~17 000 register accesses on this
//! card: the rest of `rtw89_mac_init` (DMAC/CMAC quotas, flow control,
//! interrupt masks), BB and RF register tables (from the firmware file's
//! elements), BB post-init, coexistence, DM init, the RF calibrations that
//! run at start (RCK, DACK, RX DCK), and channel 1 — plus 45 firmware
//! commands. Stage W2 (`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5.3)
//! recorded that from Linux on ryzen itself (`overlays/ryzen/w0-trace.sh`, with
//! the H2C bytes dumped by fprobe) and compiled it with
//! `overlays/ryzen/w2-seqgen.py` into [`UP`]. Two independent recordings of the
//! same start agree on every access but 564 — the RF calibration readouts and
//! the ~40 writes computed from them — which is what makes a recording a sound
//! stand-in for the code: the sequence is a property of the chip and the
//! driver, not of the run. [`Stats`] reports where this run's chip departs
//! from the recording.
//!
//! # Format
//!
//! Little-endian; one op byte, then operands. The low two bits of a width-
//! carrying op are the width: 1 = 8, 2 = 16, 3 = 32 bits; values are that wide.
//!
//! | op | operands | does |
//! |---|---|---|
//! | `0x00` | — | end |
//! | `0x01..=0x03` | `off: u32, val` | write `val` |
//! | `0x11..=0x13` | `off: u32, val` | read; count a mismatch if it is not `val` |
//! | `0x21..=0x23` | `off: u32, mask, val` | read until `read & mask == val` ([`POLL_BUDGET_US`]) |
//! | `0x30` | `us: u32` | wait |
//! | `0x40` | `len: u16, mac_at: u16, bytes[len]` | send an H2C (header included); if `mac_at != 0xffff`, the card's MAC goes at that offset first |
//! | `0x41` | `len: u16, n: u8, (kind: u8, at: u16) × n, bytes[len]` | send an H2C after filling in `n` run-time values ([`sub`]: MAC, BSSID, AID, keys) |
//!
//! `0x41` is what lets a recording of Linux *joining a network* be replayed
//! (stage W3/W4): the access point's address, the association id it handed
//! out and the keys the handshake produced are different every time, and none
//! of them may sit in the repository. The generator blanks them and records
//! where they go; [`Vars`] supplies them.

use crate::Bus;

/// The recorded start, from `fw ready` to the end of Linux's `rtw89_core_start`
/// (and the interface's first channel set, channel 1).
pub static UP: &[u8] = include_bytes!("../seq/up.seq");

/// The recorded join's four segments (`w2-seqgen.py --join`, boundaries in
/// its doc comment).
///
/// J1 is the post-`fw ready` start and interface setup, J2 the join prep
/// from the ADDR_CAM that carries the access point's address, J3 what
/// follows the association response, J4 the key installs and the beacon
/// filter. Run them in that order at those points, with [`Vars`] filled in
/// from the association response and the EAPOL handshake.
pub static JOIN1: &[u8] = include_bytes!("../seq/join1.seq");
pub static JOIN2: &[u8] = include_bytes!("../seq/join2.seq");
pub static JOIN3: &[u8] = include_bytes!("../seq/join3.seq");
pub static JOIN4: &[u8] = include_bytes!("../seq/join4.seq");
/// A group rekey: the group half of `JOIN4` (security-CAM entry 1, the DCTL and
/// ADDR_CAM that carry the group key's id), without the pairwise key or the
/// beacon filter. Cut from `JOIN4` by `overlays/ryzen/w2-join4-group.py`.
pub static JOIN4_GROUP: &[u8] = include_bytes!("../seq/join4g.seq");

/// One channel switch per 2.4 GHz channel, 1..=13 (`overlays/ryzen/w2-chans.py`
/// from a `CHAN=1 w0-trace.sh` run: Linux in monitor mode, `iw set channel N`).
///
/// Register-only (no H2C, nothing private), and each writes the whole channel
/// state — the channel fields (`0x1e060`/`0x1f060`, `0x10734`, `0xd2ec`/`0xd314`,
/// `0x19fe4`), the TX power tables at `0x1c1f8..0x1c24c`, the RX gain offsets —
/// rather than a delta, so the order channels are visited in does not matter
/// (two walks in different orders ended on the same register values, bar a few
/// calibration read-backs). `JOIN1` leaves the card on channel 1; `JOIN2`'s
/// last write to `0x19fe4` puts channel 1 back in that one register, so a join
/// on another channel replays its switch again after `JOIN2`.
pub static CHAN: [&[u8]; CHAN_COUNT] = [
    include_bytes!("../seq/chan01.seq"),
    include_bytes!("../seq/chan02.seq"),
    include_bytes!("../seq/chan03.seq"),
    include_bytes!("../seq/chan04.seq"),
    include_bytes!("../seq/chan05.seq"),
    include_bytes!("../seq/chan06.seq"),
    include_bytes!("../seq/chan07.seq"),
    include_bytes!("../seq/chan08.seq"),
    include_bytes!("../seq/chan09.seq"),
    include_bytes!("../seq/chan10.seq"),
    include_bytes!("../seq/chan11.seq"),
    include_bytes!("../seq/chan12.seq"),
    include_bytes!("../seq/chan13.seq"),
];
/// How many channels [`CHAN`] holds (2.4 GHz, 1 to 13).
pub const CHAN_COUNT: usize = 13;

/// The switch to 2.4 GHz channel `ch`, or `None` outside 1..=13.
#[must_use]
pub fn chan(ch: u8) -> Option<&'static [u8]> {
    CHAN.get(usize::from(ch).wrapping_sub(1)).copied()
}

/// The order a scan or a rejoin tries channels in.
///
/// The channel the network was last on (`hint`, 0 for none) first, then 1, 6 and 11 — what a router's
/// auto-select picks from, so where an access point that re-picks its channel
/// usually lands — then the rest, in order. Fills `out` and returns how many
/// channels it holds (always [`CHAN_COUNT`]); each channel appears once.
pub fn scan_order(hint: u8, out: &mut [u8; CHAN_COUNT]) -> usize {
    let mut n = 0;
    let mut put = |ch: u8| {
        if chan(ch).is_some() && !out[..n].contains(&ch) {
            out[n] = ch;
            n += 1;
        }
    };
    put(hint);
    for ch in [1, 6, 11] {
        put(ch);
    }
    for ch in 1..=CHAN_COUNT as u8 {
        put(ch);
    }
    n
}

/// How long a poll may wait before the replay counts it as timed out and goes
/// on. Linux's longest poll in the recording finishes in under 3 ms.
pub const POLL_BUDGET_US: u32 = 50_000;

/// Timed-out polls after which the replay gives up: past this many the chip is
/// plainly not following the recording, and every further poll would cost
/// [`POLL_BUDGET_US`] (3 503 polls would be minutes).
pub const MAX_TIMEOUTS: usize = 16;

/// One departure from the recording: the register, what Linux read, what this
/// run read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Departure {
    pub op: usize,
    pub off: u32,
    pub want: u32,
    pub got: u32,
}

/// What a replay did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub ops: usize,
    pub writes: usize,
    pub checks: usize,
    pub mismatches: usize,
    pub polls: usize,
    pub poll_timeouts: usize,
    pub delays_us: u64,
    pub h2c: usize,
    /// The first mismatching checks and timed-out polls, in order.
    pub first: [Departure; 8],
    pub first_n: usize,
}

impl Stats {
    fn note(&mut self, d: Departure) {
        if self.first_n < self.first.len() {
            self.first[self.first_n] = d;
            self.first_n += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The stream is malformed at this byte offset.
    Corrupt(usize),
    /// The H2C sender failed on the Nth command.
    H2c(usize),
    /// [`MAX_TIMEOUTS`] polls timed out (the stats say where).
    Diverged,
}

/// An H2C that could not be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendFailed;

/// Sends one H2C command (its bytes include the 8-byte H2C header).
pub trait H2cSink<B: Bus> {
    /// # Errors
    ///
    /// Whatever stopped the command going out; the replay stops.
    fn send(&mut self, bus: &mut B, cmd: &[u8]) -> Result<(), SendFailed>;
}

struct Rd<'a> {
    s: &'a [u8],
    at: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let b = self.s.get(self.at..self.at + n).ok_or(Error::Corrupt(self.at))?;
        self.at += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn val(&mut self, w: u8) -> Result<u32, Error> {
        match w {
            1 => self.u8().map(u32::from),
            2 => self.u16().map(u32::from),
            3 => self.u32(),
            _ => Err(Error::Corrupt(self.at)),
        }
    }
}

fn read(bus: &mut impl Bus, w: u8, off: u32) -> u32 {
    match w {
        1 => u32::from(bus.read8(off)),
        2 => u32::from(bus.read16(off)),
        _ => bus.read32(off),
    }
}

/// Largest H2C the stream may carry (the CH12 slot less its descriptor).
pub const H2C_MAX: usize = 2032;

/// What `0x41` fills in: one value per [`sub`] kind.
pub mod sub {
    /// This station's address (6 bytes).
    pub const MAC: u8 = 0;
    /// The access point's address (6 bytes).
    pub const BSSID: u8 = 1;
    /// The association id into the low 12 bits of a little-endian u16
    /// (the address CAM's `AID12`).
    pub const AID12: u8 = 2;
    /// The association id with bits 15:14 set, as a PS-Poll's duration field
    /// carries it (the firmware's PS-Poll template).
    pub const AID_PSPOLL: u8 = 3;
    /// The pairwise temporal key (16 bytes, in key order).
    pub const TK: u8 = 4;
    /// The group temporal key (16 bytes).
    pub const GTK: u8 = 5;
    /// The group key's id into bits 7:6 of one byte (the address CAM's and
    /// DCTL's key-id field for security entry 2, where the group key goes).
    pub const GTK_IDX_HI2: u8 = 6;
}

/// The run-time values a replay may need.
#[derive(Clone, Copy, Default)]
pub struct Vars {
    pub mac: [u8; 6],
    pub bssid: [u8; 6],
    pub aid: u16,
    pub tk: [u8; 16],
    pub gtk: [u8; 16],
    pub gtk_idx: u8,
}

impl core::fmt::Debug for Vars {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Vars(aid {}, gtk idx {})", self.aid, self.gtk_idx)
    }
}

impl Vars {
    /// Fill `kind` at `at` in `c`; `false` if it does not fit.
    fn fill(&self, c: &mut [u8], kind: u8, at: usize) -> bool {
        let put = |c: &mut [u8], v: &[u8]| c.get_mut(at..at + v.len()).map(|d| d.copy_from_slice(v)).is_some();
        match kind {
            sub::MAC => put(c, &self.mac),
            sub::BSSID => put(c, &self.bssid),
            sub::AID12 => match c.get_mut(at..at + 2) {
                Some(d) => {
                    let old = u16::from_le_bytes([d[0], d[1]]);
                    d.copy_from_slice(&((old & 0xf000) | (self.aid & 0x0fff)).to_le_bytes());
                    true
                }
                None => false,
            },
            sub::AID_PSPOLL => put(c, &((self.aid & 0x3fff) | 0xc000).to_le_bytes()),
            sub::TK => put(c, &self.tk),
            sub::GTK => put(c, &self.gtk),
            sub::GTK_IDX_HI2 => match c.get_mut(at) {
                Some(b) => {
                    *b = (*b & 0x3f) | ((self.gtk_idx & 3) << 6);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
}

/// `H2C_CL_MAC_ADDR_CAM_UPDATE` / `H2C_FUNC_MAC_ADDR_CAM_UPD`: the address
/// CAM entry (`rtw89_cam_fill_addr_cam_info`).
const ADDR_CAM: (u8, u8) = (6, 0);

/// Recompute an address-CAM command's two address hashes from the addresses
/// it now carries. A no-op for every other command.
///
/// The entry stores, beside the station address (SMA, bytes 24..30 counting
/// the 8-byte H2C header) and the peer's (TMA, 30..36), one-byte hashes of
/// each (`SMA_HASH` byte 18, `TMA_HASH` byte 19): the XOR of the address's
/// bytes from the first one the entry's address mask covers
/// (`rtw89_cam_addr_hash`). The hardware matches received frames' addresses
/// through them, so a replay that fills in a different station address or
/// BSSID and keeps the recording's hashes builds an entry no frame hits —
/// the card then drops every frame addressed to the station (measured on
/// ryzen 2026-10-06: the AP's authentication reply arrived only with the RX
/// filter opened).
fn fix_addr_cam(c: &mut [u8]) {
    if c.len() < 36 {
        return;
    }
    let h0 = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    if (((h0 >> 2) & 0x3f) as u8, ((h0 >> 8) & 0xff) as u8) != ADDR_CAM {
        return;
    }
    // ADDR_MASK bits 5:0 and MASK_SEL bits 7:6 of byte 17 (`RTW89_SMA` 1,
    // `RTW89_TMA` 2).
    let mask = c[17] & 0x3f;
    let start = if mask == 0 { 0 } else { mask.trailing_zeros() as usize };
    let (sma_start, tma_start) = match c[17] >> 6 {
        1 => (start, 0),
        2 => (0, start),
        _ => (0, 0),
    };
    let hash = |a: &[u8]| a.iter().fold(0u8, |h, &b| h ^ b);
    c[18] = hash(&c[24 + sma_start..30]);
    c[19] = hash(&c[30 + tma_start..36]);
}

/// Run `seq` against the card, counting into `st`. `vars` supplies what the
/// stream left blank: the MAC for `0x40`, everything [`sub`] names for `0x41`.
///
/// # Errors
///
/// [`Error::Corrupt`] for a malformed stream, [`Error::H2c`] if a command could
/// not be sent. A mismatching read or a timed-out poll is not an error: it is
/// counted in [`Stats`] and the replay goes on, as Linux would have.
pub fn run<B: Bus>(
    bus: &mut B,
    seq: &[u8],
    vars: &Vars,
    h2c: &mut impl H2cSink<B>,
    st: &mut Stats,
) -> Result<(), Error> {
    let mut rd = Rd { s: seq, at: 0 };
    let mut cmd = [0u8; H2C_MAX];
    loop {
        let op = rd.u8()?;
        st.ops += 1;
        let w = op & 3;
        match op & 0xf0 {
            0x00 if op == 0 => return Ok(()),
            0x00 => {
                let off = rd.u32()?;
                let v = rd.val(w)?;
                match w {
                    1 => bus.write8(off, v as u8),
                    2 => bus.write16(off, v as u16),
                    _ => bus.write32(off, v),
                }
                st.writes += 1;
            }
            0x10 => {
                let off = rd.u32()?;
                let want = rd.val(w)?;
                let got = read(bus, w, off);
                st.checks += 1;
                if got != want {
                    st.mismatches += 1;
                    st.note(Departure { op: st.ops, off, want, got });
                }
            }
            0x20 => {
                let off = rd.u32()?;
                let mask = rd.val(w)?;
                let want = rd.val(w)?;
                st.polls += 1;
                let mut waited = 0;
                loop {
                    let got = read(bus, w, off);
                    if got & mask == want {
                        break;
                    }
                    if waited >= POLL_BUDGET_US {
                        st.poll_timeouts += 1;
                        st.note(Departure { op: st.ops, off, want, got });
                        if st.poll_timeouts >= MAX_TIMEOUTS {
                            return Err(Error::Diverged);
                        }
                        break;
                    }
                    bus.delay_us(1);
                    waited += 1;
                }
            }
            0x30 => {
                let us = rd.u32()?;
                bus.delay_us(us);
                st.delays_us += u64::from(us);
            }
            0x40 if op == 0x40 => {
                let len = usize::from(rd.u16()?);
                let mac_at = rd.u16()?;
                let at = rd.at;
                let body = rd.take(len)?;
                let c = cmd.get_mut(..len).ok_or(Error::Corrupt(at))?;
                c.copy_from_slice(body);
                if mac_at != 0xffff {
                    let m = usize::from(mac_at);
                    c.get_mut(m..m + 6).ok_or(Error::Corrupt(at))?.copy_from_slice(&vars.mac);
                }
                fix_addr_cam(c);
                h2c.send(bus, c).map_err(|SendFailed| Error::H2c(st.h2c))?;
                st.h2c += 1;
            }
            0x40 if op == 0x41 => {
                let len = usize::from(rd.u16()?);
                let n = usize::from(rd.u8()?);
                let subs_at = rd.at;
                rd.take(n * 3)?;
                let at = rd.at;
                let body = rd.take(len)?;
                let c = cmd.get_mut(..len).ok_or(Error::Corrupt(at))?;
                c.copy_from_slice(body);
                for s in seq[subs_at..subs_at + n * 3].as_chunks::<3>().0 {
                    if !vars.fill(c, s[0], usize::from(u16::from_le_bytes([s[1], s[2]]))) {
                        return Err(Error::Corrupt(subs_at));
                    }
                }
                fix_addr_cam(c);
                h2c.send(bus, c).map_err(|SendFailed| Error::H2c(st.h2c))?;
                st.h2c += 1;
            }
            _ => return Err(Error::Corrupt(rd.at - 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        regs: [u32; 64],
        log: [(u8, u32, u32); 64],
        n: usize,
        slept: u64,
        flip_after: Option<u32>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self { regs: [0; 64], log: [(0, 0, 0); 64], n: 0, slept: 0, flip_after: None }
        }
    }

    impl Fake {
        fn rec(&mut self, k: u8, off: u32, v: u32) {
            self.log[self.n] = (k, off, v);
            self.n += 1;
        }
    }

    impl Bus for Fake {
        fn read8(&mut self, off: u32) -> u8 {
            self.read32(off) as u8
        }
        fn read16(&mut self, off: u32) -> u16 {
            self.read32(off) as u16
        }
        fn read32(&mut self, off: u32) -> u32 {
            if let Some(n) = self.flip_after.as_mut() {
                if *n == 0 {
                    self.regs[(off / 4) as usize] |= 0x80;
                } else {
                    *n -= 1;
                }
            }
            self.regs[(off / 4) as usize]
        }
        fn write8(&mut self, off: u32, v: u8) {
            self.write32(off, u32::from(v));
        }
        fn write16(&mut self, off: u32, v: u16) {
            self.write32(off, u32::from(v));
        }
        fn write32(&mut self, off: u32, v: u32) {
            self.regs[(off / 4) as usize] = v;
            self.rec(b'W', off, v);
        }
        fn delay_us(&mut self, us: u32) {
            self.slept += u64::from(us);
        }
    }

    struct Sink([u8; 32], usize);
    impl H2cSink<Fake> for Sink {
        fn send(&mut self, _: &mut Fake, c: &[u8]) -> Result<(), SendFailed> {
            self.0[..c.len()].copy_from_slice(c);
            self.1 += 1;
            Ok(())
        }
    }

    fn op_w32(s: &mut Vec<u8>, off: u32, v: u32) {
        s.push(0x03);
        s.extend_from_slice(&off.to_le_bytes());
        s.extend_from_slice(&v.to_le_bytes());
    }

    extern crate std;
    use std::vec::Vec;

    /// Every channel segment is well-formed, ends where its stream ends, has no
    /// H2C (so nothing private can be in it), and leaves the channel number in
    /// the one register that carries it in the clear (`0x1e060` low byte, as
    /// `0x0c00 | channel`).
    #[test]
    fn channel_segments_are_register_only_and_name_their_channel() {
        for ch in 1..=CHAN_COUNT as u8 {
            let b = chan(ch).unwrap();
            let (mut i, mut last) = (0usize, None);
            loop {
                let op = b[i];
                i += 1;
                let w = [0, 1, 2, 4][usize::from(op & 3)];
                match op & 0xf0 {
                    0x00 if op == 0 => break,
                    0x00 => {
                        let off = u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
                        let v = b[i + 4..i + 4 + w].iter().rev().fold(0u32, |a, x| a << 8 | u32::from(*x));
                        if off == 0x1e060 {
                            last = Some(v);
                        }
                        i += 4 + w;
                    }
                    0x10 => i += 4 + w,
                    0x20 => i += 4 + 2 * w,
                    0x30 => i += 4,
                    other => panic!("channel {ch}: op {op:#x} ({other:#x}) at {i}"),
                }
            }
            assert_eq!(i, b.len(), "channel {ch}: trailing bytes");
            assert_eq!(last, Some(0x0c00 | u32::from(ch)), "channel {ch}");
        }
        assert!(chan(0).is_none() && chan(14).is_none());
    }

    #[test]
    fn scan_order_tries_the_hint_then_the_usual_three_then_the_rest() {
        let mut o = [0u8; CHAN_COUNT];
        assert_eq!(scan_order(0, &mut o), CHAN_COUNT);
        assert_eq!(o, [1, 6, 11, 2, 3, 4, 5, 7, 8, 9, 10, 12, 13]);
        assert_eq!(scan_order(11, &mut o), CHAN_COUNT);
        assert_eq!(o, [11, 1, 6, 2, 3, 4, 5, 7, 8, 9, 10, 12, 13]);
        assert_eq!(scan_order(9, &mut o), CHAN_COUNT);
        assert_eq!(o[..4], [9, 1, 6, 11]);
        // A hint outside the table (5 GHz, 14, junk) is ignored.
        assert_eq!(scan_order(36, &mut o), CHAN_COUNT);
        assert_eq!(o[..3], [1, 6, 11]);
    }

    #[test]
    fn writes_checks_polls_delays_and_h2c() {
        let mut s = Vec::new();
        op_w32(&mut s, 8, 0x1234);
        s.push(0x13); // check reg 8 = 0x1234: matches
        s.extend_from_slice(&8u32.to_le_bytes());
        s.extend_from_slice(&0x1234u32.to_le_bytes());
        s.push(0x13); // check reg 12 = 5: mismatch (reads 0)
        s.extend_from_slice(&12u32.to_le_bytes());
        s.extend_from_slice(&5u32.to_le_bytes());
        s.push(0x21); // poll8 reg 16 until bit 7 set
        s.extend_from_slice(&16u32.to_le_bytes());
        s.extend_from_slice(&[0x80, 0x80]);
        s.push(0x30);
        s.extend_from_slice(&250u32.to_le_bytes());
        s.push(0x40); // H2C of 10 bytes, MAC at 2
        s.extend_from_slice(&10u16.to_le_bytes());
        s.extend_from_slice(&2u16.to_le_bytes());
        s.extend_from_slice(&[9; 10]);
        s.push(0);
        let mut bus = Fake { flip_after: Some(4), ..Fake::default() };
        let mut sink = Sink([0; 32], 0);
        let mut st = Stats::default();
        run(&mut bus, &s, &Vars { mac: [1, 2, 3, 4, 5, 6], ..Vars::default() }, &mut sink, &mut st).unwrap();
        assert_eq!((st.writes, st.checks, st.mismatches, st.polls, st.poll_timeouts, st.h2c), (1, 2, 1, 1, 0, 1));
        assert_eq!(st.first[0], Departure { op: 3, off: 12, want: 5, got: 0 });
        assert_eq!(&sink.0[..10], &[9, 9, 1, 2, 3, 4, 5, 6, 9, 9]);
        assert!(bus.slept >= 250);
    }

    #[test]
    fn templated_h2c_fills_every_kind() {
        let mut s = Vec::new();
        s.push(0x41);
        s.extend_from_slice(&30u16.to_le_bytes());
        let subs: [(u8, u16); 4] = [(sub::BSSID, 0), (sub::AID12, 6), (sub::AID_PSPOLL, 8), (sub::GTK_IDX_HI2, 10)];
        s.push(subs.len() as u8);
        for (k, at) in subs {
            s.push(k);
            s.extend_from_slice(&at.to_le_bytes());
        }
        let mut body = [0u8; 30];
        body[7] = 0xa0; // bits 15:12 of the AID12 word belong to someone else
        body[10] = 0x02; // the low bits of the key-id byte too
        s.extend_from_slice(&body);
        s.push(0x41); // and one that does not fit
        s.extend_from_slice(&4u16.to_le_bytes());
        s.push(1);
        s.extend_from_slice(&[sub::TK, 0, 0]);
        s.extend_from_slice(&[0; 4]);
        s.push(0);
        let v = Vars { bssid: [0xb0, 1, 2, 3, 4, 5], aid: 0x123, gtk_idx: 2, ..Vars::default() };
        let mut sink = Sink([0; 32], 0);
        let mut st = Stats::default();
        let r = run(&mut Fake::default(), &s, &v, &mut sink, &mut st);
        assert_eq!(&sink.0[..11], &[0xb0, 1, 2, 3, 4, 5, 0x23, 0xa1, 0x23, 0xc1, 0x82]);
        assert_eq!(sink.1, 1);
        assert!(matches!(r, Err(Error::Corrupt(_))));
    }

    #[test]
    fn a_poll_that_never_completes_is_counted_not_fatal() {
        let mut s = Vec::new();
        s.push(0x23);
        s.extend_from_slice(&4u32.to_le_bytes());
        s.extend_from_slice(&1u32.to_le_bytes());
        s.extend_from_slice(&1u32.to_le_bytes());
        op_w32(&mut s, 8, 7);
        s.push(0);
        let mut bus = Fake::default();
        let mut st = Stats::default();
        run(&mut bus, &s, &Vars::default(), &mut Sink([0; 32], 0), &mut st).unwrap();
        assert_eq!((st.poll_timeouts, st.writes), (1, 1));
    }

    #[test]
    fn truncated_stream_is_corrupt() {
        let mut s = Vec::new();
        op_w32(&mut s, 8, 7);
        s.truncate(6);
        let mut st = Stats::default();
        assert_eq!(run(&mut Fake::default(), &s, &Vars::default(), &mut Sink([0; 32], 0), &mut st), Err(Error::Corrupt(5)));
    }

    #[test]
    fn recorded_up_sequence_parses_to_the_end() {
        // Every op decodes, the stream ends with END, and it has the shape
        // the generator reported: 45 H2C commands, two carrying the MAC.
        let (h2c40, mac, h2c41, _) = walk(UP);
        assert_eq!((h2c40 + h2c41, mac), (45, 2));
    }

    /// Walk a stream's ops, returning `(H2C count, MAC-carrying 0x40 count,
    /// 0x41 count, total substitutions)`; panics on a malformed op.
    fn walk(s: &[u8]) -> (usize, usize, usize, usize) {
        let mut rd = Rd { s, at: 0 };
        let (mut h2c40, mut mac, mut h2c41, mut subs) = (0, 0, 0, 0);
        loop {
            let op = rd.u8().unwrap();
            if op == 0 {
                break;
            }
            let w = op & 3;
            match op & 0xf0 {
                0x00 | 0x10 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                }
                0x20 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                    rd.val(w).unwrap();
                }
                0x30 => {
                    rd.u32().unwrap();
                }
                0x40 => {
                    let len = usize::from(rd.u16().unwrap());
                    assert!(len <= H2C_MAX);
                    if op == 0x40 {
                        let at = rd.u16().unwrap();
                        rd.take(len).unwrap();
                        h2c40 += 1;
                        mac += usize::from(at != 0xffff);
                    } else {
                        let n = usize::from(rd.u8().unwrap());
                        for k in rd.take(n * 3).unwrap().as_chunks::<3>().0 {
                            assert!(k[0] <= sub::GTK_IDX_HI2, "unknown sub kind {}", k[0]);
                        }
                        rd.take(len).unwrap();
                        h2c41 += 1;
                        subs += n;
                    }
                }
                _ => panic!("bad op {op:#x} at {}", rd.at - 1),
            }
        }
        assert_eq!(rd.at, s.len());
        (h2c40, mac, h2c41, subs)
    }

    /// The four join segments parse to the end with the shape the generator
    /// reported, and the key installs' security-CAM bodies are synthesized
    /// (CCMP-128, temporal key at byte 16) rather than redacted zeros.
    #[test]
    fn recorded_join_sequences_parse_to_the_end() {
        for (seq, h2c41, subs) in
            [(JOIN1, 29, 1), (JOIN2, 9, 3), (JOIN3, 15, 13), (JOIN4, 9, 12)] {
            let (_, _, got41, gotsubs) = walk(seq);
            assert_eq!((got41, gotsubs), (h2c41, subs));
        }
        // J4's two security-CAM commands: synthesized bodies, keys blank.
        // (The stored bodies include the 8-byte H2C header; the command's own
        // bytes follow it.)
        let (first, second) = sec_cam_bodies(JOIN4);
        assert_eq!(&first[8..16], &[0, 0, 20, 0, 6, 0, 0, 0]); // entry 0, len 20, CCMP-128
        assert_eq!(&second[8..16], &[1, 0, 20, 0, 6, 0, 0, 0]); // entry 1
        assert!(first[16..32].iter().all(|&b| b == 0));
        assert!(second[16..32].iter().all(|&b| b == 0));
    }

    /// Running J4 with a filled [`Vars`] puts the temporal keys into the
    /// synthesized security-CAM bodies and the group key's id into the
    /// ADDR_CAM's and DCTL's bits 7:6 — the whole point of the `0x41`
    /// substitutions.
    #[test]
    fn running_join4_fills_the_keys_and_group_key_id() {
        let tk = [0x11; 16];
        let gtk = [0x22; 16];
        let v = Vars { mac: [2; 6], bssid: [3; 6], aid: 4, tk, gtk, gtk_idx: 2 };
        // The sink above holds one command; capture them all instead.
        struct Capture {
            cmds: [Vec<u8>; 16],
            lens: [usize; 16],
            n: usize,
        }
        impl H2cSink<Fake> for Capture {
            fn send(&mut self, _: &mut Fake, c: &[u8]) -> Result<(), SendFailed> {
                self.cmds[self.n] = c.to_vec();
                self.lens[self.n] = c.len();
                self.n += 1;
                Ok(())
            }
        }
        let mut cap = Capture { cmds: core::array::from_fn(|_| Vec::new()), lens: [0; 16], n: 0 };
        let mut bus = Fake::default();
        let mut st = Stats::default();
        run(&mut bus, JOIN4, &v, &mut cap, &mut st).unwrap();
        assert_eq!(st.h2c, 9);
        let sec: Vec<&Vec<u8>> =
            (0..cap.n).filter(|&i| cap.cmds[i].len() == 32).map(|i| &cap.cmds[i]).collect();
        assert_eq!(sec.len(), 2);
        assert_eq!(&sec[0][16..32], &tk); // pairwise key, entry 0
        assert_eq!(&sec[1][16..32], &gtk); // group key, entry 1
        assert_eq!(sec[0][8], 0);
        assert_eq!(sec[1][8], 1);
        // The group key's id: bits 7:6 of the ADDR_CAM's and DCTL's key-id
        // bytes, and nowhere else.
        let cam = &cap.cmds[7];
        let dctl_pair = &cap.cmds[2];
        let dctl_group = &cap.cmds[6];
        assert_eq!(cam[46] & 0xc0, 2 << 6);
        assert_eq!(dctl_group[30] & 0xc0, 2 << 6);
        assert_eq!(dctl_pair[30] & 0xc0, 0);
    }

    /// The group rekey segment sends the group key and nothing of the pairwise
    /// one: three commands, the group key's id in the DCTL and ADDR_CAM.
    #[test]
    fn join4_group_is_the_group_half_only() {
        let v = Vars { mac: [2; 6], bssid: [3; 6], aid: 4, tk: [0x11; 16], gtk: [0x22; 16], gtk_idx: 2 };
        let cmds = sent(JOIN4_GROUP, &v);
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].len(), 32);
        assert_eq!(cmds[0][8], 1); // security-CAM entry 1
        assert_eq!(&cmds[0][16..32], &v.gtk);
        assert_eq!(cmds[1][30] & 0xc0, 2 << 6);
        assert_eq!(cmds[2][46] & 0xc0, 2 << 6);
        assert!(!cmds.iter().any(|c| c.windows(16).any(|w| w == v.tk)));
        walk(JOIN4_GROUP);
    }

    /// Every command `seq` sends with `v` filled in, built as [`run`] builds
    /// it (substitutions, then [`fix_addr_cam`]) — decoded straight from the
    /// stream, since the recordings poll registers no fake bus can answer.
    fn sent(seq: &[u8], v: &Vars) -> Vec<Vec<u8>> {
        let mut rd = Rd { s: seq, at: 0 };
        let mut out = Vec::new();
        loop {
            let op = rd.u8().unwrap();
            if op == 0 {
                return out;
            }
            let w = op & 3;
            match op & 0xf0 {
                0x00 | 0x10 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                }
                0x20 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                    rd.val(w).unwrap();
                }
                0x30 => {
                    rd.u32().unwrap();
                }
                0x40 if op == 0x40 => {
                    let len = usize::from(rd.u16().unwrap());
                    let mac_at = rd.u16().unwrap();
                    let mut c = rd.take(len).unwrap().to_vec();
                    if mac_at != 0xffff {
                        let m = usize::from(mac_at);
                        c[m..m + 6].copy_from_slice(&v.mac);
                    }
                    fix_addr_cam(&mut c);
                    out.push(c);
                }
                0x40 => {
                    let len = usize::from(rd.u16().unwrap());
                    let n = usize::from(rd.u8().unwrap());
                    let subs = rd.take(n * 3).unwrap().to_vec();
                    let mut c = rd.take(len).unwrap().to_vec();
                    for t in subs.as_chunks::<3>().0 {
                        assert!(v.fill(&mut c, t[0], usize::from(u16::from_le_bytes([t[1], t[2]]))));
                    }
                    fix_addr_cam(&mut c);
                    out.push(c);
                }
                _ => panic!("bad op {op:#x}"),
            }
        }
    }

    /// The address hashes follow the addresses: an address-CAM command whose
    /// station address and BSSID the runtime filled in carries the hashes of
    /// *those*, not the recording's — in every join segment that has one.
    #[test]
    fn address_cam_hashes_follow_the_filled_in_addresses() {
        let v = Vars { mac: [0x02, 0x41, 0x4b, 0x55, 0x4d, 0x41], bssid: [0xa4, 0x91, 0xb1, 1, 2, 3], aid: 5, ..Vars::default() };
        let mut seen = 0;
        for seq in [JOIN1, JOIN2, JOIN3, JOIN4] {
            for c in sent(seq, &v).iter().filter(|c| c.len() >= 36) {
                let h0 = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                if ((h0 >> 2) & 0x3f, (h0 >> 8) & 0xff) != (6, 0) {
                    continue;
                }
                seen += 1;
                assert_eq!(&c[24..30], &v.mac, "station address filled in");
                let x = |a: &[u8]| a.iter().fold(0u8, |h, &b| h ^ b);
                // These entries have no address mask: the hash covers all six bytes.
                assert_eq!(c[17] & 0x3f, 0);
                assert_eq!(c[18], x(&v.mac));
                assert_eq!(c[19], x(&c[30..36]));
            }
        }
        assert!(seen >= 3, "only {seen} address-CAM commands");
    }

    /// A masked entry hashes from the first masked byte, of the address the
    /// mask selects only.
    #[test]
    fn address_cam_hash_honours_the_mask() {
        let mut c = [0u8; 40];
        c[0..4].copy_from_slice(&(6u32 << 2).to_le_bytes()); // class 6, func 0
        c[24..30].copy_from_slice(&[1, 2, 4, 8, 16, 32]);
        c[30..36].copy_from_slice(&[64, 128, 3, 5, 6, 9]);
        c[17] = (1 << 6) | 0b0000_0100; // MASK_SEL = SMA, mask from byte 2
        fix_addr_cam(&mut c);
        assert_eq!(c[18], 4 ^ 8 ^ 16 ^ 32);
        assert_eq!(c[19], 64 ^ 128 ^ 3 ^ 5 ^ 6 ^ 9);
        // Not an address-CAM command: untouched.
        let mut d = c;
        d[0..4].copy_from_slice(&((6u32 << 2) | (1 << 8)).to_le_bytes());
        d[18] = 0xee;
        fix_addr_cam(&mut d);
        assert_eq!(d[18], 0xee);
    }

    /// The bodies of J4's two security-CAM `0x41` commands.
    fn sec_cam_bodies(s: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut rd = Rd { s, at: 0 };
        let mut found = Vec::new();
        loop {
            let op = rd.u8().unwrap();
            if op == 0 {
                break;
            }
            let w = op & 3;
            match op & 0xf0 {
                0x00 | 0x10 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                }
                0x20 => {
                    rd.u32().unwrap();
                    rd.val(w).unwrap();
                    rd.val(w).unwrap();
                }
                0x30 => {
                    rd.u32().unwrap();
                }
                0x40 => {
                    let len = usize::from(rd.u16().unwrap());
                    if op == 0x41 {
                        let n = usize::from(rd.u8().unwrap());
                        rd.take(n * 3).unwrap();
                        let body = rd.take(len).unwrap().to_vec();
                        if body.len() == 32 && body[10] == 20 && body[12] == 6 {
                            found.push((body[8], body));
                        }
                    } else {
                        rd.u16().unwrap();
                        rd.take(len).unwrap();
                    }
                }
                _ => panic!("bad op {op:#x} at {}", rd.at - 1),
            }
        }
        assert_eq!(found.len(), 2);
        (found.remove(0).1, found.remove(0).1)
    }
}
