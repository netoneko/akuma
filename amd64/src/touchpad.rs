//! The ryzen laptop's I²C-HID touchpad: bring-up, polling, and the
//! `/dev/input/mouse0` byte stream (`docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md`).
//!
//! Decisions live in host-tested crates — `akuma-dwi2c` (controller),
//! `akuma-i2c-hid` (transport), `akuma-hid` (descriptor), `akuma-touchpad`
//! (PTP decode, click/right-click/scroll policy, wire format). This file is
//! the sequencing and the state.
//!
//! # Gating
//!
//! The device is found by name, not assumed: init runs only when the DSDT
//! contains `GXTP5100` (the 16AHP9's touchpad) or the command line says
//! `touchpad=0x<mmio>:0x<addr>` (any other box, or a different lot).
//! Everything else — QEMU, the trashcan — skips this file at the cost of one
//! table scan. Failure at any step logs one line and leaves the node absent;
//! it never blocks the boot beyond the bring-up delays (~0.3 s).
//!
//! # Locking
//!
//! One spinlock, **taken IRQ-masked and only via `try_lock`** — the xHCI
//! keyboard's rule, for the same single-core reason: a tick landing inside a
//! hold would switch to a spinner for a holder that cannot run. A poll is a
//! 40 ms-bounded I²C transaction; eight consecutive failures disable the pad
//! rather than stall the pump for ever.
//!
//! # No allocation
//!
//! Buffers are stack arrays or fields; the event ring is `Clickpad`'s.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use akuma_dwi2c::{BusSpeed, XferError};
use akuma_hid::{Filter, HidParser, ItemKind, locate, report_size};
use akuma_i2c_hid::{
    HID_DESC_REGISTER, HID_DESC_SIZE, HidDescriptor, PowerState, Quirks, ReportType, decode_input,
    encode_command, encode_reset, encode_set_power, is_reset_ack, opcode,
};
use akuma_ryzen_amd64::{MachineDescription, PhysMem};
use akuma_touchpad::ptp::{Ctl, EVENT_BYTES, PtpDecoder, PtpLayout, encode, parse_ctl};
use akuma_touchpad::tty::{Grid, MAX_REPORT, MouseEncoder, Tracking};
use akuma_touchpad::{Clickpad, Config};
use spinning_top::Spinlock;

use crate::i2c::{self, Dw};
use crate::machine::Physmap;
use crate::polltime::spin_us;
use crate::serial;

/// The 16AHP9's measured constants (M0, 2026-10-16).
const DEFAULT_MMIO: u64 = 0xFEDC_2000;
const DEFAULT_ADDR: u8 = 0x5D;
const AOAC_DEV: u32 = akuma_dwi2c::aoac::DEV_I2C0;
/// The ACPI `_HID` the DSDT scan looks for.
const HID_NAME: &[u8] = b"GXTP5100";

/// Longest HID report descriptor accepted (the Goodix one is 665 bytes).
const RDESC_MAX: usize = 1024;
/// Longest input read buffer (`wMaxInputLength` is 32 on the Goodix).
const INBUF: usize = 128;
/// Consecutive failed polls before the pad is given up on.
const MAX_FAILS: u8 = 8;
/// Poll period while a finger is down / recently was, and once idle.
const ACTIVE_PERIOD_US: u64 = 8_000;
const IDLE_PERIOD_US: u64 = 16_000;
/// How long after the last contact the active period lasts.
const ACTIVE_HOLD_US: u64 = 2_000_000;

struct Pad {
    dw: Dw,
    dec: PtpDecoder,
    pad: Clickpad,
    inbuf: [u8; INBUF],
    in_len: usize,
    fails: u8,
    next_poll_us: u64,
    last_touch_us: u64,
    /// Poll accounting, for the first-boot log: reads that completed, NACKs,
    /// empty (length 0) reads, reports decoded, and raw dumps still allowed.
    st: Stats,
    /// The console bridge's state: translation of pointer events into the
    /// mouse reports a console program asked for ([`console_bridge`]).
    mouse: MouseEncoder,
    bridge_on: bool,
}

#[derive(Default)]
struct Stats {
    polls: u32,
    nack: u32,
    empty: u32,
    reports: u32,
    other_id: u32,
    down_frames: u32,
    dumps: u8,
}

static PAD: Spinlock<Option<Pad>> = Spinlock::new(None);
/// Published once bring-up succeeded; cleared if the pad is given up on.
static UP: AtomicBool = AtomicBool::new(false);
/// Natural (MacBook-style) scrolling: on unless `touchpad.natural=0` is on the
/// command line; `echo natural=0 > /dev/input/mouse0` flips it live.
static NATURAL: AtomicBool = AtomicBool::new(true);
/// Tap-to-click, MacBook-style (a quick, still one-finger touch clicks; two
/// quick taps are a double click): on unless `touchpad.tap=0` is on the
/// command line; `echo tap=0 > /dev/input/mouse0` flips it live.
static TAP: AtomicBool = AtomicBool::new(true);

/// Is a touchpad serving `/dev/input/mouse0`?
#[must_use]
pub fn present() -> bool {
    UP.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// `touchpad=0x<mmio>:0x<addr>` — force a controller window and slave address.
fn cmdline_override(cmdline: &str) -> Option<(u64, u8)> {
    let v = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("touchpad="))?;
    let (mmio, addr) = v.split_once(':')?;
    let mmio = u64::from_str_radix(mmio.strip_prefix("0x")?, 16).ok()?;
    let addr = u8::from_str_radix(addr.strip_prefix("0x")?, 16).ok()?;
    (addr < 0x80).then_some((mmio, addr))
}

fn table_has_name(addr: u64, length: u32) -> bool {
    const CHUNK: usize = 512;
    let end = addr + u64::from(length);
    let mut at = addr + 36;
    let mut buf = [0u8; CHUNK];
    while at < end {
        let n = ((end - at) as usize).min(CHUNK);
        if !Physmap.read(at, &mut buf[..n]) {
            return false;
        }
        if buf[..n].windows(HID_NAME.len()).any(|w| w == HID_NAME) {
            return true;
        }
        if n < CHUNK {
            break;
        }
        // Overlap so a name straddling a chunk edge is still seen whole.
        at += (CHUNK - HID_NAME.len()) as u64;
    }
    false
}

fn dsdt_names_the_pad(machine: &MachineDescription) -> bool {
    let Some(rsdp) = machine.rsdp.as_ref() else { return false };
    let mut found =
        akuma_ryzen_amd64::acpi::dsdt(&Physmap, rsdp).is_some_and(|t| table_has_name(t.addr, t.length));
    if !found {
        akuma_ryzen_amd64::acpi::for_each_table(&Physmap, rsdp, |t| {
            if !found && &t.signature == b"SSDT" {
                found = table_has_name(t.addr, t.length);
            }
        });
    }
    found
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

fn fail(step: &str) {
    serial::puts("  [touchpad] ");
    serial::puts(step);
    serial::puts("\n");
}

fn delay_ms(ms: u64) {
    spin_us(ms * 1000);
}

/// Write a command, retrying the first attempt once after the 400 µs
/// wake-from-sleep gap some panels need.
fn write_retry(dw: &mut Dw, bytes: &[u8]) -> Result<(), XferError> {
    match dw.xfer(bytes, &mut []) {
        Err(XferError::NoDevice) => {
            spin_us(akuma_i2c_hid::ADDR_RETRY_DELAY_US);
            dw.xfer(bytes, &mut [])
        }
        r => r,
    }
}

fn read_register(dw: &mut Dw, reg: u16, out: &mut [u8]) -> Result<(), XferError> {
    let r = reg.to_le_bytes();
    match dw.xfer(&r, out) {
        Err(XferError::NoDevice) => {
            spin_us(akuma_i2c_hid::ADDR_RETRY_DELAY_US);
            dw.xfer(&r, out)
        }
        r => r,
    }
}

/// Set `value` into the `loc` bit field of `payload` (little-endian bits).
fn put_bits(payload: &mut [u8], pos: u32, size: u32, value: u32) {
    for i in 0..size {
        let bit = pos + i;
        let (byte, off) = ((bit / 8) as usize, bit % 8);
        if byte < payload.len() {
            if value >> i & 1 != 0 {
                payload[byte] |= 1 << off;
            } else {
                payload[byte] &= !(1 << off);
            }
        }
    }
}

/// `SET_REPORT(feature)` — spec §7.2.3: the 4/5-byte command, the data
/// register, a length word covering itself, the id and the data, then the
/// report id and the data.
fn set_feature(dw: &mut Dw, d: &HidDescriptor, id: u8, data: &[u8]) -> Result<(), XferError> {
    let mut b = [0u8; 48];
    let mut n = encode_command(&mut b, d.command_register, opcode::SET_REPORT, ReportType::Feature, id);
    if n + 4 + 1 + data.len() > b.len() {
        return Err(XferError::Bus);
    }
    b[n..n + 2].copy_from_slice(&d.data_register.to_le_bytes());
    let size = (2 + usize::from(id != 0) + data.len()) as u16;
    b[n + 2..n + 4].copy_from_slice(&size.to_le_bytes());
    n += 4;
    if id != 0 {
        b[n] = id;
        n += 1;
    }
    b[n..n + data.len()].copy_from_slice(data);
    n += data.len();
    dw.xfer(&b[..n], &mut [])
}

/// What `hid-multitouch` does at probe: put the pad in Precision Touchpad
/// mode. Without it a Windows-PTP pad stays on its boot-mouse report. The
/// usages (Digitizer page): Input Mode `0x52` = 3 (touchpad), Surface Switch
/// `0x57` = 1, Button Switch `0x58` = 1.
fn enable_ptp_mode(dw: &mut Dw, d: &HidDescriptor, rdesc: &[u8]) {
    const PAGE: u32 = 0x0D << 16;
    // Several of these fields can share one feature report (Surface Switch
    // and Button Switch do), and `SET_REPORT` replaces the whole report: one
    // write per field would clear the previous field's bit and leave the pad
    // reporting buttons but no contacts (2026-10-16, first metal run). So
    // build one payload per report id with every field set, then send each once.
    let mut fields = [(0u8, akuma_hid::HidLocation::NONE, 0u32); 3];
    let mut n = 0;
    for (usage, value) in [(0x52u32, 3u32), (0x57, 1), (0x58, 1)] {
        if let Some((id, loc)) = find_feature(rdesc, PAGE | usage) {
            fields[n] = (id, loc, value);
            n += 1;
        }
    }
    let mut done = [false; 3];
    for i in 0..n {
        if done[i] {
            continue;
        }
        let id = fields[i].0;
        let len = report_size(rdesc, ItemKind::Feature, id).unwrap_or(1).min(16);
        let mut payload = [0u8; 16];
        for j in i..n {
            if fields[j].0 == id {
                put_bits(&mut payload[..len], fields[j].1.pos, fields[j].1.size, fields[j].2);
                done[j] = true;
            }
        }
        if set_feature(dw, d, id, &payload[..len]).is_err() {
            fail("set-feature failed (continuing)");
        }
    }
}

/// A feature field's report id and location, by full usage.
fn find_feature(rdesc: &[u8], usage: u32) -> Option<(u8, akuma_hid::HidLocation)> {
    let mut p = HidParser::new(rdesc, Filter::Kind(ItemKind::Feature));
    while let Some(i) = p.next_item() {
        if i.kind == ItemKind::Feature && i.usage == usage && i.flags & akuma_hid::HIO_CONST == 0 {
            let (loc, _) = locate(rdesc, usage, i.report_id, ItemKind::Feature)?;
            return Some((i.report_id, loc));
        }
    }
    None
}

fn bring_up(mmio: u64, addr: u8) -> Option<Pad> {
    if !i2c::wake(AOAC_DEV) {
        fail("controller would not power up (AOAC)");
        return None;
    }
    let base = i2c::map_mmio(mmio, 0x1000)?;
    // SAFETY: `map_mmio` mapped the 4 KiB window at `mmio` as device memory
    // for the life of the kernel; it is a DesignWare block (checked by
    // `init`'s COMP_TYPE test before anything is written).
    let mut dw = unsafe { Dw::new(base) };
    let Some(probe) = dw.init(addr, BusSpeed::Fast) else {
        fail("not a DesignWare controller (COMP_TYPE)");
        return None;
    };
    serial::puts("  [touchpad] dw_i2c v=0x");
    serial::put_hex(u64::from(probe.version));
    serial::puts(" fs=");
    serial::put_dec(u64::from(probe.fs_hcnt));
    serial::puts(":");
    serial::put_dec(u64::from(probe.fs_lcnt));
    serial::puts(if probe.kept_counts { " (kept)" } else { " (computed)" });
    serial::puts(" fifo=");
    serial::put_dec(u64::from(probe.tx_depth));
    serial::puts("/");
    serial::put_dec(u64::from(probe.rx_depth));
    serial::puts("\n");

    let mut raw = [0u8; HID_DESC_SIZE];
    let mut desc = None;
    for _ in 0..akuma_i2c_hid::BRINGUP_ATTEMPTS {
        match read_register(&mut dw, HID_DESC_REGISTER, &mut raw) {
            Ok(()) => match HidDescriptor::parse(&raw) {
                Ok(d) => {
                    desc = Some(d);
                    break;
                }
                Err(_) => fail("bad HID descriptor"),
            },
            Err(XferError::NoDevice) => fail("no ACK at the slave address"),
            Err(_) => {
                fail("descriptor read failed");
                dw.recover();
            }
        }
        delay_ms(20);
    }
    let desc = desc?;
    serial::puts("  [touchpad] hid ");
    serial::put_hexn(u64::from(desc.vendor_id), 4);
    serial::puts(":");
    serial::put_hexn(u64::from(desc.product_id), 4);
    serial::puts(" rdesc=");
    serial::put_dec(u64::from(desc.report_desc_len));
    serial::puts(" maxin=");
    serial::put_dec(u64::from(desc.max_input_len));
    serial::puts("\n");
    if usize::from(desc.report_desc_len) > RDESC_MAX || usize::from(desc.max_input_len) > INBUF {
        fail("descriptor larger than the static buffers");
        return None;
    }

    // Power on, settle, reset, wait for the zero-length acknowledgement.
    let mut cmd = [0u8; 8];
    let n = encode_set_power(&mut cmd, desc.command_register, PowerState::On);
    if write_retry(&mut dw, &cmd[..n]).is_err() {
        fail("SET_POWER failed");
        return None;
    }
    let quirks = Quirks::for_vid_pid(desc.vendor_id, desc.product_id);
    delay_ms(akuma_i2c_hid::POWER_ON_DELAY_MS + quirks.post_power_delay_ms);
    let n = encode_reset(&mut cmd, desc.command_register);
    if write_retry(&mut dw, &cmd[..n]).is_err() {
        fail("RESET failed");
        return None;
    }
    let mut acked = false;
    let mut waited = 0;
    while waited < 1000 {
        delay_ms(10);
        waited += 10;
        let mut two = [0xFFu8; 2];
        if dw.xfer(&[], &mut two).is_ok() && is_reset_ack(&two) {
            acked = true;
            break;
        }
    }
    if !acked {
        fail("no reset acknowledgement (continuing)");
    }

    let mut rdesc = [0u8; RDESC_MAX];
    let rlen = usize::from(desc.report_desc_len);
    if read_register(&mut dw, desc.report_desc_register, &mut rdesc[..rlen]).is_err() {
        fail("report descriptor read failed");
        return None;
    }
    let rdesc = &rdesc[..rlen];
    let Some(layout) = PtpLayout::from_descriptor(rdesc) else {
        fail("no precision-touchpad report in the descriptor");
        return None;
    };
    let Some(fb) = crate::fbdev::geometry() else {
        fail("no framebuffer to map the pointer onto");
        return None;
    };
    let Ok(geom) = layout.geometry(fb.width, fb.height) else {
        fail("bad pointer geometry");
        return None;
    };
    enable_ptp_mode(&mut dw, &desc, rdesc);
    serial::puts("  [touchpad] ptp report ");
    serial::put_dec(u64::from(layout.report_id));
    serial::puts(" contacts=");
    serial::put_dec(layout.contacts as u64);
    serial::puts(" range=");
    serial::put_dec(layout.x_max as u64);
    serial::puts("x");
    serial::put_dec(layout.y_max as u64);
    serial::puts(" -> ");
    serial::put_dec(u64::from(fb.width));
    serial::puts("x");
    serial::put_dec(u64::from(fb.height));
    serial::puts("\n");

    Some(Pad {
        dw,
        dec: PtpDecoder::new(layout),
        pad: Clickpad::new(geom, Config {
            natural_scroll: NATURAL.load(Ordering::Relaxed),
            tap_to_click: TAP.load(Ordering::Relaxed),
            ..Config::DEFAULT
        }),
        inbuf: [0; INBUF],
        in_len: usize::from(desc.max_input_len).max(4),
        fails: 0,
        next_poll_us: 0,
        last_touch_us: 0,
        st: Stats { dumps: 6, ..Stats::default() },
        mouse: MouseEncoder::new(),
        bridge_on: false,
    })
}

/// Controller window found by [`discover`], 0 = none. Two stages because the
/// machine description exists early (beside `power::init`) and the bring-up
/// delays need the calibrated TSC, which comes later (`boot_to_init`).
static FOUND_MMIO: AtomicU64 = AtomicU64::new(0);
static FOUND_ADDR: AtomicU8 = AtomicU8::new(0);

/// Decide whether this machine has the touchpad: the DSDT names it, or
/// `touchpad=` forces a window. Touches no hardware.
pub fn discover(machine: &MachineDescription, cmdline: &str) {
    if let Some(v) = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("touchpad.natural=")) {
        NATURAL.store(!matches!(v, "0" | "off" | "false" | "no"), Ordering::Relaxed);
    }
    if let Some(v) = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("touchpad.tap=")) {
        TAP.store(!matches!(v, "0" | "off" | "false" | "no"), Ordering::Relaxed);
    }
    let found = if let Some(o) = cmdline_override(cmdline) {
        Some(o)
    } else if dsdt_names_the_pad(machine) {
        Some((DEFAULT_MMIO, DEFAULT_ADDR))
    } else {
        None
    };
    if let Some((mmio, addr)) = found {
        FOUND_ADDR.store(addr, Ordering::Relaxed);
        FOUND_MMIO.store(mmio, Ordering::Release);
    } else {
        // One line, so "nothing in dmesg" can no longer mean "never ran".
        serial::puts("  [touchpad] not found (");
        serial::puts(if machine.rsdp.is_some() { "no GXTP5100 in DSDT/SSDT" } else { "no RSDP" });
        serial::puts("; touchpad=0xMMIO:0xADDR forces it)\n");
    }
}

/// Bring up what [`discover`] found; a no-op when it found nothing. Called
/// once, from `boot_to_init`, after the framebuffer is registered.
pub fn init() {
    let mmio = FOUND_MMIO.load(Ordering::Acquire);
    if mmio == 0 {
        return;
    }
    let addr = FOUND_ADDR.load(Ordering::Relaxed);
    serial::puts("  [touchpad] GXTP5100 @0x");
    serial::put_hex(u64::from(addr));
    serial::puts(" on dw_i2c 0x");
    serial::put_hex(mmio);
    serial::puts("\n");
    let Some(pad) = bring_up(mmio, addr) else { return };
    *PAD.lock() = Some(pad);
    UP.store(true, Ordering::Release);
    serial::puts("  [touchpad] up: /dev/input/mouse0\n");
}

// ---------------------------------------------------------------------------
// Polling and the byte stream
// ---------------------------------------------------------------------------

impl Pad {
    /// The first few unexpected reads, raw: `[touchpad] <why>: len hh hh ...`.
    fn dump(&mut self, why: &str) {
        if self.st.dumps == 0 {
            return;
        }
        self.st.dumps -= 1;
        serial::puts("  [touchpad] ");
        serial::puts(why);
        serial::puts(":");
        for b in &self.inbuf[..self.in_len.min(14)] {
            serial::puts(" ");
            serial::put_hexn(u64::from(*b), 2);
        }
        serial::puts("\n");
    }

    /// A decoded frame, for the first-boot log: raw head, then each contact.
    fn dump_frame(&mut self, touching: bool) {
        serial::puts("  [touchpad] frame:");
        for b in &self.inbuf[..self.in_len.min(40)] {
            serial::puts(" ");
            serial::put_hexn(u64::from(*b), 2);
        }
        let f = self.dec.frame();
        for (i, c) in f.contacts.iter().enumerate().filter(|(_, c)| c.down).take(3) {
            serial::puts(" | c");
            serial::put_dec(i as u64);
            serial::puts(" ");
            serial::put_dec(c.x as u64);
            serial::puts(",");
            serial::put_dec(c.y as u64);
        }
        serial::puts(if touching { " touching" } else { " not-touching" });
        serial::puts(if f.button { " BTN" } else { "" });
        serial::puts(" pending=");
        serial::puts(if self.pad.pending() { "yes" } else { "no" });
        serial::puts("\n");
    }

    /// One line per 4096 polls (about 65 s idle): the counters.
    fn summary(&mut self) {
        let total = self.st.polls + self.st.nack;
        if total == 0 || !total.is_multiple_of(4096) {
            return;
        }
        serial::puts("  [touchpad] polls=");
        serial::put_dec(u64::from(self.st.polls));
        serial::puts(" nack=");
        serial::put_dec(u64::from(self.st.nack));
        serial::puts(" empty=");
        serial::put_dec(u64::from(self.st.empty));
        serial::puts(" reports=");
        serial::put_dec(u64::from(self.st.reports));
        serial::puts(" other=");
        serial::put_dec(u64::from(self.st.other_id));
        serial::puts(" down=");
        serial::put_dec(u64::from(self.st.down_frames));
        serial::puts("\n");
    }

    /// One poll: read, decode, feed the policy. Cheap when nothing is due.
    fn tick(&mut self, now: u64) -> bool {
        if now < self.next_poll_us {
            return true;
        }
        let n = self.in_len;
        match self.dw.xfer(&[], &mut self.inbuf[..n]) {
            Ok(()) => self.fails = 0,
            // A NACK on a poll is the pad saying "nothing to report" (we poll
            // with no interrupt line to tell us when), not a dead bus: back
            // off, don't count it toward giving up.
            Err(XferError::NoDevice) => {
                self.st.nack += 1;
                self.summary();
                self.next_poll_us = now + IDLE_PERIOD_US;
                return true;
            }
            Err(e) => {
                if e == XferError::Timeout {
                    self.dw.recover();
                }
                self.fails += 1;
                self.next_poll_us = now + IDLE_PERIOD_US;
                return self.fails < MAX_FAILS;
            }
        }
        let mut touching = false;
        self.st.polls += 1;
        match decode_input(&self.inbuf[..n]) {
            None => {
                self.st.empty += 1;
                if self.st.empty <= 2 {
                    self.dump("empty read");
                }
            }
            Some(report) => match report.split_first() {
                Some((&id, payload)) if id == self.dec.layout().report_id => {
                    if self.dec.decode(payload) {
                        self.st.reports += 1;
                        let f = self.dec.frame();
                        touching = f.contacts.iter().any(|c| c.down) || f.button;
                        self.pad.update(&f, crate::net::uptime_us());
                        let down = f.contacts.iter().any(|c| c.down);
                        if down {
                            self.st.down_frames += 1;
                        }
                        if self.st.reports <= 4 || (down && self.st.down_frames <= 12) || self.st.reports.is_multiple_of(512) {
                            self.dump_frame(touching);
                        }
                    }
                }
                _ => {
                    self.st.other_id += 1;
                    self.dump("other report");
                }
            },
        }
        self.summary();
        if touching {
            self.last_touch_us = now;
        }
        let active = now.saturating_sub(self.last_touch_us) < ACTIVE_HOLD_US;
        self.next_poll_us = now + if active { ACTIVE_PERIOD_US } else { IDLE_PERIOD_US };
        true
    }
}

/// Poll the pad once, if one is up and a poll is due. Called by the console
/// pump once a lap; `try_lock` so a reader holding the lock costs a skipped
/// lap, not a stall.
pub fn poll() {
    if !UP.load(Ordering::Acquire) {
        return;
    }
    let _irq = akuma_primitives::irq::IrqGuard::new();
    let Some(mut g) = PAD.try_lock() else { return };
    let Some(p) = g.as_mut() else { return };
    if !p.tick(crate::net::uptime_us()) {
        serial::puts("  [touchpad] bus failing; disabled (dropped ");
        serial::put_dec(u64::from(p.pad.dropped_moves()));
        serial::puts(" moves)\n");
        UP.store(false, Ordering::Release);
    }
}

/// Events waiting? `poll(2)`'s readiness answer for the node.
#[must_use]
pub fn has_events() -> bool {
    if !UP.load(Ordering::Acquire) {
        return false;
    }
    let _irq = akuma_primitives::irq::IrqGuard::new();
    PAD.try_lock().is_some_and(|g| g.as_ref().is_some_and(|p| p.pad.pending()))
}

/// `read(fd)` on `/dev/input/mouse0`: whole 8-byte records
/// (`akuma_touchpad::ptp::encode`), as many as fit and as are waiting.
/// Never blocks: nothing waiting is `EAGAIN` (readers poll, as the rest of
/// this kernel's input is polled).
pub fn dev_read(buf: u64, len: usize) -> u64 {
    const MAX_RECORDS: usize = 16;
    let want = (len / EVENT_BYTES).min(MAX_RECORDS);
    if want == 0 {
        return crate::fd::errno::EINVAL;
    }
    let mut out = [0u8; MAX_RECORDS * EVENT_BYTES];
    let mut n = 0;
    {
        let _irq = akuma_primitives::irq::IrqGuard::new();
        let Some(mut g) = PAD.try_lock() else {
            return crate::fd::errno::EAGAIN;
        };
        let Some(p) = g.as_mut() else {
            return crate::fd::errno::ENODEV;
        };
        while n < want
            && let Some(ev) = p.pad.pop()
        {
            out[n * EVENT_BYTES..(n + 1) * EVENT_BYTES].copy_from_slice(&encode(&ev));
            n += 1;
        }
    }
    if n == 0 {
        return crate::fd::errno::EAGAIN;
    }
    let bytes = n * EVENT_BYTES;
    if !crate::uaccess::write_bytes(buf, &out[..bytes]) {
        return crate::fd::errno::EFAULT;
    }
    bytes as u64
}

// ---------------------------------------------------------------------------
// The console bridge: pointer -> a terminal program's mouse input
// ---------------------------------------------------------------------------

/// Wheel ticks per event the encoder may emit; the bridge leaves room for a
/// full event before taking one off the ring.
const BRIDGE_ROOM: usize = MAX_REPORT * 8;

/// Turn pending pointer events into the mouse reports the program on the
/// console asked for (`CSI ? 1000/1002/1003 h`, SGR with `1006`), written into
/// `out` — the same input queue the keyboard types into. Returns the bytes
/// written.
///
/// Only while a program has asked: otherwise the events stay for
/// `/dev/input/mouse0` readers (and the first enable discards what piled up,
/// so a click made long ago does not fire the moment a program starts
/// listening). The pointer's cell is marked on the console while reporting is
/// on and unmarked when it ends.
pub fn console_bridge(out: &mut [u8]) -> usize {
    if !UP.load(Ordering::Acquire) {
        return 0;
    }
    let Some((tracking, sgr, g)) = crate::multiboot2::fb_mouse() else { return 0 };
    let _irq = akuma_primitives::irq::IrqGuard::new();
    let Some(mut guard) = PAD.try_lock() else { return 0 };
    let Some(p) = guard.as_mut() else { return 0 };
    if tracking == 0 {
        if p.bridge_on {
            p.bridge_on = false;
            p.mouse.reset();
            crate::multiboot2::fb_set_pointer(None);
        }
        return 0;
    }
    if !p.bridge_on {
        p.bridge_on = true;
        p.mouse.reset();
        while p.pad.pop().is_some() {}
    }
    let mode = match tracking {
        1000 => Tracking::Press,
        1002 => Tracking::Drag,
        _ => Tracking::Any,
    };
    let grid = Grid { origin_x: g[0], origin_y: g[1], cell_w: g[2], cell_h: g[3], cols: g[4], rows: g[5] };
    if !p.mouse.placed() {
        // A touchpad moves the pointer relatively: it starts in the middle of
        // the screen, not wherever the first finger happens to land.
        let (cx, cy) = grid.center();
        p.mouse.warp(cx, cy, &grid);
    }
    let mut n = 0;
    while out.len() - n >= BRIDGE_ROOM {
        let Some(ev) = p.pad.pop() else { break };
        n += p.mouse.feed(&ev, mode, sgr, &grid, &mut out[n..]);
    }
    let (col, row) = p.mouse.cell();
    crate::multiboot2::fb_set_pointer(Some((row.saturating_sub(1) as usize, col.saturating_sub(1) as usize)));
    n
}

/// `write(fd)` on `/dev/input/mouse0`: control lines (`natural`, `natural=0|1`,
/// `tap`, `tap=0|1`). Every line is parsed before any applies, so a batch with
/// a bad line changes nothing (`/dev/power`'s rule).
pub fn dev_write(buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > 256 {
        return crate::fd::errno::EINVAL;
    }
    let mut text = [0u8; 256];
    if !crate::uaccess::read_bytes(buf, &mut text[..len]) {
        return crate::fd::errno::EFAULT;
    }
    let mut ctls = [None; 8];
    for (n, line) in text[..len].split(|&c| c == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).enumerate() {
        let Some(c) = parse_ctl(line) else { return crate::fd::errno::EINVAL };
        if n == ctls.len() {
            return crate::fd::errno::EINVAL;
        }
        ctls[n] = Some(c);
    }
    let _irq = akuma_primitives::irq::IrqGuard::new();
    let Some(mut g) = PAD.try_lock() else { return crate::fd::errno::EAGAIN };
    let Some(p) = g.as_mut() else { return crate::fd::errno::ENODEV };
    for c in ctls.into_iter().flatten() {
        match c {
            Ctl::Natural(v) => {
                let on = v.unwrap_or_else(|| !p.pad.natural_scroll());
                p.pad.set_natural_scroll(on);
                NATURAL.store(on, Ordering::Relaxed);
                serial::puts(if on { "  [touchpad] natural scrolling on\n" } else { "  [touchpad] natural scrolling off\n" });
            }
            Ctl::Tap(v) => {
                let on = v.unwrap_or_else(|| !p.pad.tap_to_click());
                p.pad.set_tap_to_click(on);
                TAP.store(on, Ordering::Relaxed);
                serial::puts(if on { "  [touchpad] tap-to-click on\n" } else { "  [touchpad] tap-to-click off\n" });
            }
        }
    }
    len as u64
}
