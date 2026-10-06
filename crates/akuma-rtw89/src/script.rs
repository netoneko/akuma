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

use crate::Bus;

/// The recorded start, from `fw ready` to the end of Linux's `rtw89_core_start`
/// (and the interface's first channel set, channel 1).
pub static UP: &[u8] = include_bytes!("../seq/up.seq");

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

/// Run `seq` against the card, counting into `st`. `mac` replaces the zeroed
/// MAC address in the H2C commands that carry one.
///
/// # Errors
///
/// [`Error::Corrupt`] for a malformed stream, [`Error::H2c`] if a command could
/// not be sent. A mismatching read or a timed-out poll is not an error: it is
/// counted in [`Stats`] and the replay goes on, as Linux would have.
pub fn run<B: Bus>(
    bus: &mut B,
    seq: &[u8],
    mac: [u8; 6],
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
            0x40 => {
                let len = usize::from(rd.u16()?);
                let mac_at = rd.u16()?;
                let at = rd.at;
                let body = rd.take(len)?;
                let c = cmd.get_mut(..len).ok_or(Error::Corrupt(at))?;
                c.copy_from_slice(body);
                if mac_at != 0xffff {
                    let m = usize::from(mac_at);
                    c.get_mut(m..m + 6).ok_or(Error::Corrupt(at))?.copy_from_slice(&mac);
                }
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
        run(&mut bus, &s, [1, 2, 3, 4, 5, 6], &mut sink, &mut st).unwrap();
        assert_eq!((st.writes, st.checks, st.mismatches, st.polls, st.poll_timeouts, st.h2c), (1, 2, 1, 1, 0, 1));
        assert_eq!(st.first[0], Departure { op: 3, off: 12, want: 5, got: 0 });
        assert_eq!(&sink.0[..10], &[9, 9, 1, 2, 3, 4, 5, 6, 9, 9]);
        assert!(bus.slept >= 250);
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
        run(&mut bus, &s, [0; 6], &mut Sink([0; 32], 0), &mut st).unwrap();
        assert_eq!((st.poll_timeouts, st.writes), (1, 1));
    }

    #[test]
    fn truncated_stream_is_corrupt() {
        let mut s = Vec::new();
        op_w32(&mut s, 8, 7);
        s.truncate(6);
        let mut st = Stats::default();
        assert_eq!(run(&mut Fake::default(), &s, [0; 6], &mut Sink([0; 32], 0), &mut st), Err(Error::Corrupt(5)));
    }

    #[test]
    fn recorded_up_sequence_parses_to_the_end() {
        // Every op decodes, the stream ends with END, and it has the shape
        // the generator reported: 45 H2C commands, two carrying the MAC.
        let mut rd = Rd { s: UP, at: 0 };
        let (mut h2c, mut mac) = (0, 0);
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
                    let at = rd.u16().unwrap();
                    assert!(len <= H2C_MAX);
                    rd.take(len).unwrap();
                    h2c += 1;
                    mac += usize::from(at != 0xffff);
                }
                _ => panic!("bad op {op:#x} at {}", rd.at - 1),
            }
        }
        assert_eq!(rd.at, UP.len());
        assert_eq!((h2c, mac), (45, 2));
    }
}
