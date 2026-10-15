//! `/proc/power`: battery and AC state (`docs/archive/AKUMA_ACPI_POWER.md`),
//! plus the machine facts and the dim factor the power daemon drives through
//! `/dev/power` (the `powersaved` control device).
//!
//! The pure parts — finding the EC's memory window in AML, decoding it,
//! rendering the text, decoding `/dev/power`'s command lines — are
//! `akuma-power`, host-tested. This file is only what cannot move: reading the
//! ACPI tables through the physmap, mapping the window uncached, the volatile
//! byte reads, and the dim state itself.
//!
//! # Sources
//!
//! | | selected by | |
//! |---|---|---|
//! | none | default, nothing found | `/proc/power` says `source=none`; the machine is a desktop or a VM |
//! | ec | the DSDT/SSDTs declare an `ERAM` `SystemMemory` region, or `ecram=0x<pa>` | the laptop's embedded controller |
//! | sim | `powersim` | `akuma_power::sim`: a battery that cycles, for QEMU and the trashcan |
//!
//! # No allocation
//!
//! Init scans table bytes through a 512-byte stack chunk; a read copies 20
//! bytes out of the window into a stack array and renders into the caller's
//! buffer. Nothing here can fail under memory pressure.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use akuma_power::aml::{self, MAX_DECL_LEN};
use akuma_power::ctl::{parse, Ctl, Parsed};
use akuma_power::ec::{BLOCK_LEN, Reading};
use akuma_power::ledger::Ledger;
use akuma_power::render::{self, Source};
use akuma_power::sim;
use akuma_ryzen_amd64::{MachineDescription, PhysMem};
use spinning_top::Spinlock;

use crate::machine::Physmap;
use crate::paging::{self, MemAttr, PteProt};
use crate::phys::DEVMAP_BASE;
use crate::serial;

const PAGE: u64 = 4096;
/// The region name the Ryzen laptop's DSDT gives the EC's memory mirror.
const REGION: &[u8; 4] = b"ERAM";
/// Bytes of table read per call while scanning.
const CHUNK: usize = 512;

const MODE_NONE: u8 = 0;
const MODE_EC: u8 = 1;
const MODE_SIM: u8 = 2;
static MODE: AtomicU8 = AtomicU8::new(MODE_NONE);
/// Kernel-virtual address of the window's first byte, when `MODE_EC`.
static EC_VA: AtomicU64 = AtomicU64::new(0);

/// The running energy total, integrated between `/proc/power` reads. The
/// renderer is the sampler: nothing reads the EC or ticks a timer when nobody
/// is looking, which is the same property the file itself has. A reader that
/// wants fine energy resolution reads more often — the rio panel's once a
/// second keeps the integration error near the truncation floor.
static LEDGER: Spinlock<Ledger> = Spinlock::new(Ledger::new());

/// Find the window, map it, and register `/proc/power`.
///
/// Always registers: a machine with no battery still answers `source=none`, so a
/// status bar can tell "no battery here" from "kernel too old to say".
pub fn init(machine: &MachineDescription, cmdline: &str) {
    let mode = if cmdline.split_ascii_whitespace().any(|t| t == "powersim") {
        serial::puts("  [power] source=sim\n");
        MODE_SIM
    } else if let Some(pa) = ecram_override(cmdline).or_else(|| find_window(machine).map(|base| base + akuma_power::ec::BATTERY_BLOCK_AT)) {
        if map_window(pa) {
            serial::puts("  [power] source=ec window=0x");
            serial::put_hex(pa);
            serial::puts("\n");
            MODE_EC
        } else {
            serial::puts("  [power] EC window found but unmappable; source=none\n");
            MODE_NONE
        }
    } else {
        serial::puts("  [power] source=none (no ERAM region in the DSDT/SSDTs)\n");
        MODE_NONE
    };
    MODE.store(mode, Ordering::Release);
    akuma_vfs_glue::set_power_renderer(render_proc);
}

/// `ecram=0x<physical address>` on the command line: a window the AML scan did
/// not find (or a machine being investigated), read with the same layout.
fn ecram_override(cmdline: &str) -> Option<u64> {
    let v = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("ecram="))?;
    u64::from_str_radix(v.strip_prefix("0x")?, 16).ok()
}

/// Scan the DSDT and every SSDT for the `ERAM` region.
fn find_window(machine: &MachineDescription) -> Option<u64> {
    let rsdp = machine.rsdp.as_ref()?;
    let mut found = None;
    if let Some(t) = akuma_ryzen_amd64::acpi::dsdt(&Physmap, rsdp) {
        found = scan_table(t.addr, t.length);
    }
    if found.is_none() {
        akuma_ryzen_amd64::acpi::for_each_table(&Physmap, rsdp, |t| {
            if found.is_none() && &t.signature == b"SSDT" {
                found = scan_table(t.addr, t.length);
            }
        });
    }
    found
}

/// Scan one table's AML (after its 36-byte header) in overlapping chunks, so a
/// declaration straddling a chunk edge is still seen whole.
fn scan_table(addr: u64, length: u32) -> Option<u64> {
    let end = addr + u64::from(length);
    let mut at = addr + 36;
    let mut buf = [0u8; CHUNK];
    while at < end {
        let n = ((end - at) as usize).min(CHUNK);
        if !Physmap.read(at, &mut buf[..n]) {
            return None;
        }
        if let Some(r) = aml::find_region(&buf[..n], REGION) {
            return Some(r.offset);
        }
        if n < CHUNK {
            break;
        }
        at += (CHUNK - MAX_DECL_LEN) as u64;
    }
    None
}

/// Map the pages covering the window into the device window, uncached.
fn map_window(pa: u64) -> bool {
    let first = pa & !(PAGE - 1);
    let last = (pa + BLOCK_LEN as u64).div_ceil(PAGE) * PAGE;
    let mut p = first;
    while p < last {
        if !paging::map_page((DEVMAP_BASE + p) as usize, p, PteProt::KERNEL_RW, MemAttr::Device) {
            return false;
        }
        p += PAGE;
    }
    EC_VA.store(DEVMAP_BASE + pa, Ordering::Release);
    true
}

/// Copy the window out a byte at a time. Byte-wide because the mirror is EC
/// memory: a wider access is allowed to tear, a byte is not.
fn read_window() -> [u8; BLOCK_LEN] {
    let va = EC_VA.load(Ordering::Acquire) as *const u8;
    let mut b = [0u8; BLOCK_LEN];
    for (i, slot) in b.iter_mut().enumerate() {
        // SAFETY: `map_window` mapped every page of `[va, va + BLOCK_LEN)` as
        // device memory before `MODE_EC` was published, and `EC_VA` is only
        // non-zero then.
        *slot = unsafe { va.add(i).read_volatile() };
    }
    b
}

/// Integrate one decoded sample into the running total and return it.
///
/// Only valid samples integrate: a window of garbage has no power, and
/// carrying the last known power across an invalid read would invent energy
/// the meter never reported. The total survives a glitch — the reader sees it
/// pause, not reset.
fn energy_mwh(raw: &[u8; BLOCK_LEN]) -> u64 {
    let r = Reading::decode(raw);
    if !r.validate() {
        return LEDGER.lock().total_mwh();
    }
    LEDGER.lock().sample(r.power_mw(), crate::net::uptime_us())
}

// ---------------------------------------------------------------------------
// The dim factor, and `/dev/power` — the power daemon's control device.
//
// `powersaved` (userspace) owns the *policy* — how long an idle keyboard
// precedes which dim level, `/proc/power` being its only input. This side owns
// only the *effect*: one recorded percentage, applied to the framebuffer
// console (`multiboot2::fb_set_dim`) and reported back through both files. A
// machine with no console records the number all the same, so the facts stay
// honest where there is nothing to dim.
// ---------------------------------------------------------------------------

/// The global dim factor, percent 1..=100. 100 is a boot that has not been
/// dimmed — the value every reader must see before the daemon's first step.
static DIM_PCT: AtomicU8 = AtomicU8::new(100);

/// The dim factor in force.
#[must_use]
pub fn dim_pct() -> u8 {
    DIM_PCT.load(Ordering::Acquire)
}

/// Record and apply a dim factor. The clamp already happened in
/// `akuma_power::ctl::parse`; this asserts the same invariant on the kernel
/// side because a second caller will eventually exist and the two must not
/// drift.
pub fn set_dim_pct(pct: u8) {
    let pct = pct.clamp(1, 100);
    DIM_PCT.store(pct, Ordering::Release);
    crate::multiboot2::fb_set_dim(pct);
}

/// Longest `/dev/power` snapshot; the facts are two short lines.
const DEV_SNAPSHOT_MAX: usize = 64;

/// `read(fd)` on a `/dev/power` descriptor: the same facts `/proc/power`
/// appends — `idle_ms=` and `dim=` — regenerated fresh per read. There is no
/// cursor state: the snapshot is two lines, a reader wanting all of it passes
/// a buffer of [`DEV_SNAPSHOT_MAX`], and a short read truncates rather than
/// paging the rest out on the next call. A control device a daemon polls once
/// a second does not want the per-descriptor cursor machinery `wifi`'s
/// multi-line status needs.
pub fn dev_read(buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let mut snap = [0u8; DEV_SNAPSHOT_MAX];
    let n = render::render_kernel_facts(&mut snap, crate::input::idle_ms(), dim_pct());
    let n = n.min(len);
    if !crate::uaccess::write_bytes(buf, &snap[..n]) {
        return crate::fd::errno::EFAULT;
    }
    n as u64
}

/// `write(fd)` on a `/dev/power` descriptor: `dim=<percent>` lines. All lines
/// are parsed before any is applied (`wifi`'s rule), so a batch with one bad
/// line changes nothing.
pub fn dev_write(buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    // The daemon writes one short line per decision; 256 bytes is slack for a
    // batched write and bounds what a hostile writer can make us stage.
    if len > 256 {
        return crate::fd::errno::EINVAL;
    }
    let mut text = [0u8; 256];
    if !crate::uaccess::read_bytes(buf, &mut text[..len]) {
        return crate::fd::errno::EFAULT;
    }
    let text = &text[..len];
    let mut applied = false;
    for line in text.split(|&c| c == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)) {
        match parse(line) {
            Parsed::Ok(Ctl::Dim(pct)) => {
                set_dim_pct(pct);
                applied = true;
            }
            Parsed::Invalid => return crate::fd::errno::EINVAL,
        }
    }
    if applied {
        len as u64
    } else {
        0
    }
}

/// The `/proc/power` renderer: the battery render (or its `source=none`
/// placeholder), then the kernel's own facts — `idle_ms=` and `dim=` — which
/// every machine answers, EC or not. The daemon's once-a-second read is also
/// what keeps the ledger integrating; an unread file costs nothing, and the
/// daemon is the reader that wants the resolution.
pub fn render_proc(out: &mut [u8]) -> usize {
    let n = match MODE.load(Ordering::Acquire) {
        MODE_EC => {
            let raw = read_window();
            render::render(out, Source::Ec, Some(&raw), Some(energy_mwh(&raw)))
        }
        MODE_SIM => {
            let secs = crate::net::uptime_us() / 1_000_000;
            let raw = sim::block(secs);
            render::render(out, Source::Sim, Some(&raw), Some(energy_mwh(&raw)))
        }
        _ => render::render(out, Source::None, None, None),
    };
    n + render::render_kernel_facts(&mut out[n..], crate::input::idle_ms(), dim_pct())
}
