//! `/proc/power`: battery and AC state (`docs/archive/AKUMA_ACPI_POWER.md`).
//!
//! The pure parts — finding the EC's memory window in AML, decoding it,
//! rendering the text — are `akuma-power`, host-tested. This file is only what
//! cannot move: reading the ACPI tables through the physmap, mapping the window
//! uncached, and the volatile byte reads.
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
use akuma_power::ec::BLOCK_LEN;
use akuma_power::render::{self, Source};
use akuma_power::sim;
use akuma_ryzen_amd64::{MachineDescription, PhysMem};

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

/// Find the window, map it, and register `/proc/power`.
///
/// Always registers: a machine with no battery still answers `source=none`, so a
/// status bar can tell "no battery here" from "kernel too old to say".
pub fn init(machine: &MachineDescription, cmdline: &str) {
    let mode = if cmdline.split_ascii_whitespace().any(|t| t == "powersim") {
        serial::puts("  [power] source=sim\n");
        MODE_SIM
    } else if let Some(pa) = ecram_override(cmdline).or_else(|| find_window(machine)) {
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

/// The `/proc/power` renderer.
pub fn render_proc(out: &mut [u8]) -> usize {
    match MODE.load(Ordering::Acquire) {
        MODE_EC => render::render(out, Source::Ec, Some(&read_window())),
        MODE_SIM => {
            let secs = crate::net::uptime_us() / 1_000_000;
            render::render(out, Source::Sim, Some(&sim::block(secs)))
        }
        _ => render::render(out, Source::None, None),
    }
}
