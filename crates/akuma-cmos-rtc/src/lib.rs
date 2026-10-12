//! The PC CMOS real-time clock, read.
//!
//! Every x86 machine this kernel boots on — the ryzen laptop, QEMU's q35 —
//! has an MC146818-compatible clock behind ports `0x70`/`0x71` that keeps
//! the date across power-off. Until 2026-10-12 the amd64 kernel never read
//! it: the wall clock was set by SNTP or not at all, so a boot without a
//! network (the QEMU rehearsals, a wifi boot before the join) stamped every
//! inode 1970 and every unlinked inode with a zero `dtime` — which `e2fsck`
//! reports as "deleted inode has zero dtime" after a hard reboot (found by
//! ryzen menu entry 7's fsck, `docs/archive/WIFI_FUZZ_AND_LOSSY_SIM.md`).
//!
//! This crate is the decode and the encode, host-tested; the kernel supplies
//! the port accesses through [`Cmos`], anchors its wall clock with [`read`]'s
//! result, and once SNTP has synced writes the chip back with [`write`] so the
//! next boot without a network starts from a corrected clock — what Linux's
//! `sync_cmos_clock` does, and like it only from NTP, never from a ring-3
//! `settimeofday` (that is `hwclock --systohc`'s job). SNTP still corrects the
//! seed every boot: a CMOS clock drifts seconds a month and may hold local
//! time on a machine that dual-boots Windows (ryzen's holds UTC: `timedatectl`
//! says `RTC in local TZ: no`).
//!
//! # Writing it right (Linux's `mc146818_set_time`)
//!
//! Set **SET** (status B bit 7) so the chip stops updating, write every
//! field in the format status B says the chip is in (BCD or binary, 12- or
//! 24-hour) plus the century, clear SET. The date goes in as UTC, the
//! format the chip was found in is kept, and [`write`] reads the chip back
//! and refuses to report success unless it agrees.
//!
//! # Reading it right (Linux's `mc146818_get_time`)
//!
//! The chip updates its registers once a second and sets **UIP** (status A
//! bit 7) for ~2 ms beforehand; a read during the update can mix the old
//! seconds with the new minute. So: wait for UIP clear (bounded), read every
//! register, read them all again, and accept only a pair that agrees. Status
//! B says the format: bit 2 binary (else BCD), bit 1 24-hour (else 12-hour
//! with the hour's bit 7 as PM). The century lives in register `0x32` on
//! every machine this runs on (ACPI's FADT names the register; QEMU and the
//! laptop both use `0x32`); a century that does not decode to 19–21 is
//! ignored and the year is taken as 20xx.
//!
//! # Plausibility
//!
//! A dead CMOS battery reads as 2000-01-01 (or garbage); seeding the wall
//! clock with that would make [`is_set`]-style gates answer "yes" for a
//! machine that does not know the year, and every TLS certificate would be
//! not-yet-valid — the exact failure SNTP exists to prevent. So [`unix_seconds`]
//! refuses dates before [`EARLIEST_YEAR`] (this code was written in 2026; a
//! clock before that is wrong) and after 2099, and the kernel then waits for
//! SNTP as before.

#![no_std]
#![forbid(unsafe_code)]

/// The two CMOS ports, as the caller implements them: select a register on
/// `0x70`, read or write it on `0x71`.
pub trait Cmos {
    fn read(&mut self, reg: u8) -> u8;
    fn write(&mut self, reg: u8, v: u8);
}

/// Register numbers.
pub mod reg {
    pub const SECONDS: u8 = 0x00;
    pub const MINUTES: u8 = 0x02;
    pub const HOURS: u8 = 0x04;
    pub const DAY: u8 = 0x07;
    pub const MONTH: u8 = 0x08;
    pub const YEAR: u8 = 0x09;
    pub const STATUS_A: u8 = 0x0a;
    pub const STATUS_B: u8 = 0x0b;
    pub const CENTURY: u8 = 0x32;
}

/// Status A: update in progress.
const UIP: u8 = 1 << 7;
/// Status B: updates stopped, registers may be written.
const SET: u8 = 1 << 7;
/// Status B: values are binary rather than BCD.
const BINARY: u8 = 1 << 2;
/// Status B: 24-hour mode.
const HOUR_24: u8 = 1 << 1;
/// In 12-hour mode, the hour's PM bit.
const PM: u8 = 1 << 7;

/// Reads of status A before giving up on UIP clearing (the update takes
/// ~2 ms; a port read is ~1 µs, so this is several seconds of patience for
/// a chip that never clears it).
const UIP_WAIT_READS: u32 = 1_000_000;
/// Consistent double reads attempted before giving up.
const READ_TRIES: u32 = 8;

/// A civil date and time, UTC as far as this crate knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The earliest year a clock may claim and be believed.
pub const EARLIEST_YEAR: u16 = 2025;
pub const LATEST_YEAR: u16 = 2099;

/// Why the clock was not read, or not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Status A never showed the update finished.
    UpdateNeverEnded,
    /// Two reads never agreed.
    Unstable,
    /// A field was not a BCD digit pair, or out of its range.
    Malformed,
    /// The date to write is outside [`EARLIEST_YEAR`]..=[`LATEST_YEAR`], or
    /// not a date at all.
    OutOfRange,
    /// Written, but the chip read back something else.
    ReadBackDiffers,
}

/// The raw registers of one pass.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Raw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
    status_b: u8,
}

fn raw(c: &mut impl Cmos) -> Raw {
    Raw {
        sec: c.read(reg::SECONDS),
        min: c.read(reg::MINUTES),
        hour: c.read(reg::HOURS),
        day: c.read(reg::DAY),
        month: c.read(reg::MONTH),
        year: c.read(reg::YEAR),
        century: c.read(reg::CENTURY),
        status_b: c.read(reg::STATUS_B),
    }
}

fn bcd(v: u8) -> Option<u8> {
    let (hi, lo) = (v >> 4, v & 0xf);
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

/// Decode one pass's registers.
fn decode(r: &Raw) -> Result<DateTime, Error> {
    let binary = r.status_b & BINARY != 0;
    let num = |v: u8| -> Result<u8, Error> { if binary { Ok(v) } else { bcd(v).ok_or(Error::Malformed) } };
    let pm = r.hour & PM != 0;
    let hour_raw = r.hour & !PM;
    let mut hour = num(hour_raw)?;
    if r.status_b & HOUR_24 == 0 {
        // 12-hour mode: 12 is midnight/noon, PM adds twelve.
        if hour == 12 {
            hour = 0;
        }
        if pm {
            hour += 12;
        }
    } else if pm {
        // In 24-hour mode bit 7 of the hour is not a PM flag; a set one is
        // garbage.
        return Err(Error::Malformed);
    }
    let (second, minute, day, month, year2) = (num(r.sec)?, num(r.min)?, num(r.day)?, num(r.month)?, num(r.year)?);
    let century = num(r.century).ok().filter(|c| (19..=21).contains(c)).unwrap_or(20);
    let dt = DateTime { year: u16::from(century) * 100 + u16::from(year2), month, day, hour, minute, second };
    if second > 59 || minute > 59 || hour > 23 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(dt.year, month) {
        return Err(Error::Malformed);
    }
    Ok(dt)
}

/// Read the clock: wait out an update, then read until two passes agree.
///
/// # Errors
///
/// [`Error`] when the chip never finished updating, never read the same
/// twice, or holds values that are not a date.
pub fn read(c: &mut impl Cmos) -> Result<DateTime, Error> {
    let mut waited = 0;
    while c.read(reg::STATUS_A) & UIP != 0 {
        waited += 1;
        if waited >= UIP_WAIT_READS {
            return Err(Error::UpdateNeverEnded);
        }
    }
    let mut last = raw(c);
    for _ in 0..READ_TRIES {
        let again = raw(c);
        if again == last {
            return decode(&again);
        }
        last = again;
    }
    Err(Error::Unstable)
}

fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn bcd_encode(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Encode one pass's registers for a chip in the format `status_b` says.
fn encode(dt: &DateTime, status_b: u8) -> Raw {
    let binary = status_b & BINARY != 0;
    let num = |v: u8| if binary { v } else { bcd_encode(v) };
    let hour = if status_b & HOUR_24 != 0 {
        num(dt.hour)
    } else {
        // 12-hour mode: 0 is 12 AM, 12 is 12 PM, 13..23 are 1..11 PM.
        let (h, pm) = match dt.hour {
            0 => (12, false),
            1..=11 => (dt.hour, false),
            12 => (12, true),
            _ => (dt.hour - 12, true),
        };
        num(h) | if pm { PM } else { 0 }
    };
    Raw {
        sec: num(dt.second),
        min: num(dt.minute),
        hour,
        day: num(dt.day),
        month: num(dt.month),
        year: num((dt.year % 100) as u8),
        century: num((dt.year / 100) as u8),
        status_b,
    }
}

/// Write `dt` (UTC) to the chip, in the format the chip is already in, and
/// read it back.
///
/// # Errors
///
/// [`Error::OutOfRange`] for a date this crate would not believe on reading
/// (so a write can never produce a seed the next boot refuses), the read
/// errors if the chip cannot be read afterwards, and
/// [`Error::ReadBackDiffers`] if what it holds is not what was written — to
/// the second, which allows the chip to have ticked once meanwhile.
pub fn write(c: &mut impl Cmos, dt: &DateTime) -> Result<(), Error> {
    if unix_seconds(dt).is_none() {
        return Err(Error::OutOfRange);
    }
    let status_b = c.read(reg::STATUS_B);
    let r = encode(dt, status_b);
    // Stop the chip's updates for the duration of the write; the SET bit is
    // the only bit of status B this touches (daylight saving, 24-hour and
    // binary modes, the interrupt enables stay the chip's).
    c.write(reg::STATUS_B, status_b | SET);
    c.write(reg::SECONDS, r.sec);
    c.write(reg::MINUTES, r.min);
    c.write(reg::HOURS, r.hour);
    c.write(reg::DAY, r.day);
    c.write(reg::MONTH, r.month);
    c.write(reg::YEAR, r.year);
    c.write(reg::CENTURY, r.century);
    c.write(reg::STATUS_B, status_b & !SET);
    let back = read(c)?;
    let (want, got) = (unix_seconds(dt).ok_or(Error::OutOfRange)?, unix_seconds(&back).ok_or(Error::ReadBackDiffers)?);
    if got.abs_diff(want) > 1 {
        return Err(Error::ReadBackDiffers);
    }
    Ok(())
}

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `y-m-d` for days since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The civil UTC date and time for `secs` since the Unix epoch — `None`
/// outside the years this crate believes, so what is written is always what
/// the next boot will accept.
#[must_use]
pub fn civil_from_unix(secs: u64) -> Option<DateTime> {
    let days = i64::try_from(secs / 86_400).ok()?;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let year = u16::try_from(y).ok()?;
    if !(EARLIEST_YEAR..=LATEST_YEAR).contains(&year) {
        return None;
    }
    Some(DateTime {
        year,
        month: m as u8,
        day: d as u8,
        hour: (rem / 3600) as u8,
        minute: (rem % 3600 / 60) as u8,
        second: (rem % 60) as u8,
    })
}

/// Seconds since the Unix epoch for `dt`, taken as UTC — or `None` when the
/// date is not one a working clock would show ([`EARLIEST_YEAR`]..=[`LATEST_YEAR`]),
/// which is what a dead battery looks like.
#[must_use]
pub fn unix_seconds(dt: &DateTime) -> Option<u64> {
    if !(EARLIEST_YEAR..=LATEST_YEAR).contains(&dt.year) {
        return None;
    }
    let days = days_from_civil(i64::from(dt.year), i64::from(dt.month), i64::from(dt.day));
    let secs = days * 86_400 + i64::from(dt.hour) * 3600 + i64::from(dt.minute) * 60 + i64::from(dt.second);
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chip model: the registers, how many status-A reads show UIP first,
    /// and whether the first full pass is torn (the seconds roll over
    /// between the two reads).
    struct Chip {
        regs: [u8; 0x40],
        uip_reads: u32,
        tear: bool,
        passes: u32,
        /// Data-register writes that landed while SET was clear (must be 0).
        unguarded_writes: u32,
        writes: u32,
    }

    impl Chip {
        fn bcd(y: u8, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> Self {
            let enc = |v: u8| (v / 10) << 4 | (v % 10);
            let mut regs = [0u8; 0x40];
            regs[reg::SECONDS as usize] = enc(s);
            regs[reg::MINUTES as usize] = enc(mi);
            regs[reg::HOURS as usize] = enc(h);
            regs[reg::DAY as usize] = enc(d);
            regs[reg::MONTH as usize] = enc(mo);
            regs[reg::YEAR as usize] = enc(y);
            regs[reg::CENTURY as usize] = 0x20;
            regs[reg::STATUS_B as usize] = HOUR_24;
            Self { regs, uip_reads: 0, tear: false, passes: 0, unguarded_writes: 0, writes: 0 }
        }
    }

    impl Cmos for Chip {
        fn read(&mut self, r: u8) -> u8 {
            if r == reg::STATUS_A {
                if self.uip_reads > 0 {
                    self.uip_reads -= 1;
                    return UIP;
                }
                return 0;
            }
            if r == reg::STATUS_B {
                self.passes += 1;
            }
            let v = self.regs[usize::from(r)];
            // The first pass sees the seconds one lower than every later one.
            if self.tear && self.passes == 0 && r == reg::SECONDS {
                return v.wrapping_sub(1);
            }
            v
        }
        fn write(&mut self, r: u8, v: u8) {
            if r != reg::STATUS_B && self.regs[reg::STATUS_B as usize] & SET == 0 {
                self.unguarded_writes += 1;
            }
            self.writes += 1;
            self.regs[usize::from(r)] = v;
        }
    }

    #[test]
    fn reads_a_bcd_24h_clock_and_converts_to_unix() {
        // 2026-10-12 20:40:02 UTC, what ryzen's chip held when this was written.
        let mut c = Chip::bcd(26, 10, 12, 20, 40, 2);
        let dt = read(&mut c).unwrap();
        assert_eq!(dt, DateTime { year: 2026, month: 10, day: 12, hour: 20, minute: 40, second: 2 });
        assert_eq!(unix_seconds(&dt), Some(1_791_837_602), "`date -u -d 2026-10-12T20:40:02Z +%s`");
        // The epoch's neighbours, through the same arithmetic.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
    }

    #[test]
    fn waits_out_an_update_and_a_torn_read() {
        let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
        c.uip_reads = 5;
        c.tear = true;
        assert_eq!(read(&mut c).unwrap().second, 0);
        assert!(c.passes >= 2, "two agreeing passes were needed");
        let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
        c.uip_reads = u32::MAX;
        assert_eq!(read(&mut c), Err(Error::UpdateNeverEnded));
    }

    #[test]
    fn binary_and_12_hour_modes() {
        let mut c = Chip::bcd(26, 6, 15, 0, 0, 0);
        c.regs[reg::STATUS_B as usize] = BINARY | HOUR_24;
        c.regs[reg::HOURS as usize] = 23;
        c.regs[reg::MINUTES as usize] = 59;
        c.regs[reg::DAY as usize] = 15;
        c.regs[reg::MONTH as usize] = 6;
        c.regs[reg::YEAR as usize] = 26;
        let dt = read(&mut c).unwrap();
        assert_eq!((dt.hour, dt.minute, dt.year), (23, 59, 2026));
        // 12-hour BCD: 12 AM is 0, 12 PM is 12, 11 PM is 23.
        for (raw, want) in [(0x12, 0), (0x12 | PM, 12), (0x11 | PM, 23), (0x01, 1)] {
            let mut c = Chip::bcd(26, 6, 15, 0, 0, 0);
            c.regs[reg::STATUS_B as usize] = 0;
            c.regs[reg::HOURS as usize] = raw;
            assert_eq!(read(&mut c).unwrap().hour, want, "raw {raw:#x}");
        }
    }

    #[test]
    fn century_register_is_believed_only_when_sane() {
        for (century, want) in [(0x20, 2026), (0x19, 1926), (0x00, 2026), (0xff, 2026), (0x45, 2026)] {
            let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
            c.regs[reg::CENTURY as usize] = century;
            assert_eq!(read(&mut c).unwrap().year, want, "century {century:#x}");
        }
    }

    #[test]
    fn garbage_and_dead_batteries_are_refused() {
        let mut c = Chip::bcd(26, 13, 1, 0, 0, 0);
        assert_eq!(read(&mut c), Err(Error::Malformed), "month 13");
        let mut c = Chip::bcd(26, 2, 29, 0, 0, 0);
        assert_eq!(read(&mut c), Err(Error::Malformed), "2026-02-29");
        let mut c = Chip::bcd(24, 2, 29, 0, 0, 0);
        assert!(read(&mut c).is_ok(), "2024-02-29 is a day");
        let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
        c.regs[reg::SECONDS as usize] = 0x7a; // not BCD
        assert_eq!(read(&mut c), Err(Error::Malformed));
        let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
        c.regs[reg::HOURS as usize] = 0x80 | 0x05; // PM bit in 24-hour mode
        assert_eq!(read(&mut c), Err(Error::Malformed));
        // A dead battery: 2000-01-01 reads fine and is not believed.
        let dt = read(&mut Chip::bcd(0, 1, 1, 0, 0, 0)).unwrap();
        assert_eq!(dt.year, 2000);
        assert_eq!(unix_seconds(&dt), None);
        assert_eq!(unix_seconds(&DateTime { year: 2100, month: 1, day: 1, hour: 0, minute: 0, second: 0 }), None);
        assert_eq!(unix_seconds(&DateTime { year: EARLIEST_YEAR, month: 1, day: 1, hour: 0, minute: 0, second: 0 }), Some(1_735_689_600));
    }

    #[test]
    fn write_round_trips_in_every_chip_format() {
        let dt = DateTime { year: 2031, month: 2, day: 28, hour: 23, minute: 59, second: 58 };
        for status_b in [HOUR_24, BINARY | HOUR_24, 0, BINARY] {
            let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
            c.regs[reg::STATUS_B as usize] = status_b;
            write(&mut c, &dt).unwrap();
            assert_eq!(read(&mut c).unwrap(), dt, "status B {status_b:#x}");
            assert_eq!(c.unguarded_writes, 0, "every data write under SET");
            assert_eq!(c.regs[reg::STATUS_B as usize], status_b, "SET cleared, the rest kept");
            assert_eq!(c.regs[reg::CENTURY as usize], if status_b & BINARY != 0 { 20 } else { 0x20 });
            assert_eq!(c.writes, 9);
        }
        // Noon and midnight in 12-hour mode, the two the PM bit gets wrong.
        for hour in [0u8, 12, 1, 13, 11, 23] {
            let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
            c.regs[reg::STATUS_B as usize] = 0;
            let dt = DateTime { hour, ..dt };
            write(&mut c, &dt).unwrap();
            assert_eq!(read(&mut c).unwrap().hour, hour);
        }
    }

    #[test]
    fn write_refuses_what_read_would_not_believe() {
        let mut c = Chip::bcd(26, 1, 1, 0, 0, 0);
        let before = c.regs;
        let bad = DateTime { year: 2000, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
        assert_eq!(write(&mut c, &bad), Err(Error::OutOfRange));
        assert_eq!(c.regs, before, "nothing written");
        assert_eq!(c.writes, 0);
        // A chip that drops writes is caught by the read-back.
        struct Stuck(Chip);
        impl Cmos for Stuck {
            fn read(&mut self, r: u8) -> u8 {
                self.0.read(r)
            }
            fn write(&mut self, r: u8, v: u8) {
                if r == reg::STATUS_B {
                    self.0.write(r, v);
                }
            }
        }
        let mut s = Stuck(Chip::bcd(26, 1, 1, 0, 0, 0));
        let dt = DateTime { year: 2040, month: 6, day: 1, hour: 1, minute: 2, second: 3 };
        assert_eq!(write(&mut s, &dt), Err(Error::ReadBackDiffers));
    }

    #[test]
    fn civil_from_unix_inverts_unix_seconds_across_the_window() {
        assert_eq!(civil_from_unix(1_791_837_602), Some(DateTime { year: 2026, month: 10, day: 12, hour: 20, minute: 40, second: 2 }));
        assert_eq!(civil_from_unix(1_735_689_600), Some(DateTime { year: 2025, month: 1, day: 1, hour: 0, minute: 0, second: 0 }));
        assert_eq!(civil_from_unix(1_735_689_599), None, "2024-12-31");
        assert_eq!(civil_from_unix(4_102_444_800), None, "2100-01-01");
        // Every 7th day plus an odd offset from 2025 to 2099, both ways.
        let mut t = 1_735_689_600u64 + 12_345;
        while t < 4_102_444_800 {
            let dt = civil_from_unix(t).unwrap();
            assert_eq!(unix_seconds(&dt), Some(t), "{dt:?}");
            assert!(dt.day <= days_in_month(dt.year, dt.month));
            t += 7 * 86_400 + 3_601;
        }
    }

    #[test]
    fn a_chip_that_never_agrees_is_given_up_on() {
        struct Noisy(u8);
        impl Cmos for Noisy {
            fn read(&mut self, r: u8) -> u8 {
                if r == reg::STATUS_A {
                    return 0;
                }
                self.0 = self.0.wrapping_add(1);
                self.0
            }
            fn write(&mut self, _: u8, _: u8) {}
        }
        assert_eq!(read(&mut Noisy(0)), Err(Error::Unstable));
    }
}
