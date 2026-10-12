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
//! This crate is the decode, host-tested; the kernel supplies the two port
//! accesses through [`Cmos`] and anchors its wall clock with the result.
//! SNTP still corrects it afterwards: a CMOS clock drifts seconds a month and
//! may hold local time on a machine that dual-boots Windows (ryzen's holds
//! UTC: `timedatectl` says `RTC in local TZ: no`).
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
/// `0x70`, read it on `0x71`.
pub trait Cmos {
    fn read(&mut self, reg: u8) -> u8;
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

/// Why the clock was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Status A never showed the update finished.
    UpdateNeverEnded,
    /// Two reads never agreed.
    Unstable,
    /// A field was not a BCD digit pair, or out of its range.
    Malformed,
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
            Self { regs, uip_reads: 0, tear: false, passes: 0 }
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
        }
        assert_eq!(read(&mut Noisy(0)), Err(Error::Unstable));
    }
}
