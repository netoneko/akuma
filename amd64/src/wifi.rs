//! `/dev/wifi0`: the wifi control device (`proposals/AKUMA_WIFI_CONTROL.md`).
//!
//! Akuma has no supplicant — the WPA2 handshake will run in the kernel — so the
//! whole userspace/kernel boundary for wifi is this device: **write** a command
//! line (`scan wlan0`, `connect wlan0 <ssid-hex> <psk-hex | -> [bssid]`,
//! `disconnect wlan0`), **read** the driver's state as `key=value` lines plus
//! one `bss` line per scan result. The grammar and the text are `akuma-wifi`'s,
//! shared with the `wifi` tool so the two cannot drift.
//!
//! # Backends
//!
//! | backend | selected by | |
//! |---|---|---|
//! | none | default | no `/dev/wifi0` node at all |
//! | simulated | `wifisim` on the command line | `akuma_wifi::sim`: fixed networks, deterministic connect rules — tests the tool, device and protocol anywhere, QEMU included |
//! | rtw89 | (W1–W4, not yet) | ryzen's RTL8852CE |
//!
//! # Reads: a snapshot per descriptor
//!
//! `cat /dev/wifi0` must see the state once and then EOF, so each open
//! descriptor has a read cursor over a snapshot taken at its first read. When
//! the cursor reaches the end, that read returns 0 and the next one takes a
//! fresh snapshot — a poller just reads to EOF each time. Cursors live in a
//! fixed table keyed by (thread group, fd), **no allocation**: `SLOTS` of 4 KiB,
//! which holds the largest status (`MAX_BSS` results of 32-byte SSIDs). A table
//! that is full evicts the oldest cursor (its reader merely restarts at a fresh
//! snapshot). A `fork`ed child's copy of the fd is a different key — its own
//! cursor, which is the sane answer for a state device.
//!
//! # Permissions
//!
//! The node is `0600` because a write hands the driver a network key. Today
//! every Akuma process is uid 0 (`geteuid` answers 0), so the mode states the
//! intent rather than enforcing it.

use core::sync::atomic::{AtomicBool, Ordering};

use akuma_wifi::cmd::{self, CmdError};
use akuma_wifi::sim::SimRadio;
use akuma_wifi::status::Status;
use akuma_wifi::IfName;
use spinning_top::Spinlock;

use crate::fd::errno;
use crate::serial;

/// The one interface name this kernel answers to.
const IFACE: &[u8] = b"wlan0";
/// Bytes one snapshot may take; the largest status fits (`akuma-wifi` tests it).
const SNAP: usize = 4096;
/// Concurrent readers with a cursor.
const SLOTS: usize = 8;
/// Longest command a single `write` is parsed from.
const MAX_WRITE: usize = 512;

enum Backend {
    Sim(SimRadio),
}

impl Backend {
    fn status(&self) -> &Status {
        match self {
            Self::Sim(r) => r.status(),
        }
    }

    fn apply(&mut self, c: &cmd::Command) {
        match self {
            Self::Sim(r) => r.apply(c),
        }
    }
}

static BACKEND: Spinlock<Option<Backend>> = Spinlock::new(None);
static PRESENT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct Slot {
    tgid: u32,
    fd: u32,
    used: bool,
    /// Bytes of the current snapshot; 0 = none taken (next read takes one).
    len: usize,
    pos: usize,
    /// For eviction: higher = more recently used.
    stamp: u64,
    buf: [u8; SNAP],
}

const EMPTY_SLOT: Slot = Slot { tgid: 0, fd: 0, used: false, len: 0, pos: 0, stamp: 0, buf: [0; SNAP] };

static CURSORS: Spinlock<([Slot; SLOTS], u64)> = Spinlock::new(([EMPTY_SLOT; SLOTS], 0));

/// Register the backend the command line asks for. With none, `/dev/wifi0`
/// does not exist.
pub fn init(cmdline: &str) {
    let Some(iface) = IfName::new(IFACE) else { return };
    if cmdline.split_ascii_whitespace().any(|t| t == "wifisim") {
        *BACKEND.lock() = Some(Backend::Sim(SimRadio::new(iface)));
        PRESENT.store(true, Ordering::Release);
        akuma_vfs_glue::set_wifi_present(true);
        serial::puts("  wifi: simulated radio on /dev/wifi0 (wlan0) — `wifisim`\n");
    }
}

/// Is there a `/dev/wifi0`?
#[must_use]
pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// `read(fd, buf, len)` on a `/dev/wifi0` descriptor: the next bytes of this
/// descriptor's snapshot, or 0 at its end (and a fresh snapshot next time).
pub fn read(tgid: u32, fd: u32, buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let mut g = CURSORS.lock();
    let (slots, clock) = &mut *g;
    *clock += 1;
    let now = *clock;
    let idx = if let Some(i) = slots.iter().position(|s| s.used && s.tgid == tgid && s.fd == fd) {
        i
    } else {
        // A free slot, else the least recently used one.
        let i = slots
            .iter()
            .position(|s| !s.used)
            .unwrap_or_else(|| slots.iter().enumerate().min_by_key(|(_, s)| s.stamp).map_or(0, |(i, _)| i));
        slots[i] = Slot { tgid, fd, used: true, ..EMPTY_SLOT };
        i
    };
    let slot = &mut slots[idx];
    slot.stamp = now;
    if slot.len == 0 {
        let b = BACKEND.lock();
        let Some(backend) = b.as_ref() else { return errno::ENODEV };
        match backend.status().write(&mut slot.buf) {
            Some(n) => {
                slot.len = n;
                slot.pos = 0;
            }
            None => return errno::EIO,
        }
    }
    if slot.pos >= slot.len {
        slot.len = 0;
        slot.pos = 0;
        return 0;
    }
    let n = (slot.len - slot.pos).min(len);
    if !crate::uaccess::write_bytes(buf, &slot.buf[slot.pos..slot.pos + n]) {
        return errno::EFAULT;
    }
    slot.pos += n;
    n as u64
}

/// `write(fd, buf, len)` on a `/dev/wifi0` descriptor: one or more command
/// lines. All are parsed before any is applied, so a bad line applies nothing.
pub fn write(buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > MAX_WRITE {
        return errno::EINVAL;
    }
    let mut text = [0u8; MAX_WRITE];
    if !crate::uaccess::read_bytes(buf, &mut text[..len]) {
        return errno::EFAULT;
    }
    let text = &text[..len];
    let lines = || text.split(|&c| c == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace));
    for line in lines() {
        match cmd::parse(line) {
            Ok(c) if c.iface().as_str().as_bytes() != IFACE => return errno::ENODEV,
            Ok(_) => {}
            Err(e) => return refusal(e),
        }
    }
    let mut b = BACKEND.lock();
    let Some(backend) = b.as_mut() else { return errno::ENODEV };
    for line in lines() {
        if let Ok(c) = cmd::parse(line) {
            backend.apply(&c);
        }
    }
    len as u64
}

fn refusal(e: CmdError) -> u64 {
    match e {
        CmdError::UnknownVerb => errno::EOPNOTSUPP,
        CmdError::BadInterface => errno::ENODEV,
        CmdError::Empty
        | CmdError::MissingArgument
        | CmdError::TooManyArguments
        | CmdError::BadSsid
        | CmdError::BadKey
        | CmdError::BadBssid => errno::EINVAL,
    }
}

/// Forget the cursor of a closed descriptor.
pub fn release(tgid: u32, fd: u32) {
    let mut g = CURSORS.lock();
    for s in g.0.iter_mut().filter(|s| s.used && s.tgid == tgid && s.fd == fd) {
        *s = EMPTY_SLOT;
    }
}
