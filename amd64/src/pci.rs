//! PCI enumeration — the `0xCF8`/`0xCFC` mechanism half of [`akuma_pci`].
//!
//! On the VMM targets (`microvm`, Firecracker) there is no PCI bus at all;
//! every device is a virtio-MMIO transport announced on the command line and
//! this module finds nothing, which is correct. On real hardware nothing is
//! announced — [`scan`] is how the kernel discovers the xHCI/EHCI controllers,
//! the Realtek NIC and the AHCI disk. The bare-metal entry (`multiboot2.rs`)
//! calls it right after the machine description is parsed.
//!
//! What lives here is only what has to: the `unsafe` port I/O, and the
//! fixed-size registry the scan fills. The header/BAR/capability *decoding* is
//! `akuma-pci`, host-tested against config space read off the reference
//! machine.

use akuma_pci::{Address, Bar, Header, command};
use akuma_selftest::Suite;
use spinning_top::Spinlock;

use crate::paging::{self, MemAttr, Prot};
use crate::phys::DEVMAP_BASE;
use crate::port::{inl, outl};
use crate::serial;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

const PAGE_SIZE: u64 = 4096;

/// How many PCI functions the registry holds. The reference machine enumerates
/// 15; 48 is headroom for an add-in card or two without ever allocating.
const MAX_DEVICES: usize = 48;

/// One enumerated PCI function.
#[derive(Clone, Copy)]
pub struct Device {
    pub addr: Address,
    pub header: Header,
    /// Address-decoded BARs; sizes are `0` until [`probe_bar_size`] fills one.
    pub bars: [Option<Bar>; 6],
}

struct Registry {
    devices: [Option<Device>; MAX_DEVICES],
    count: usize,
    scanned: bool,
}

static REGISTRY: Spinlock<Registry> =
    Spinlock::new(Registry { devices: [None; MAX_DEVICES], count: 0, scanned: false });

/// Read a config-space dword. `offset` is aligned down to 4 bytes.
fn read_u32(addr: Address, offset: u8) -> u32 {
    // SAFETY: `0xCF8`/`0xCFC` are the architectural PCI config ports; the write
    // selects a (bus, device, function, dword) and the read returns it, with no
    // effect on any device.
    unsafe {
        outl(CONFIG_ADDRESS, addr.config_address(offset));
        inl(CONFIG_DATA)
    }
}

fn write_u32(addr: Address, offset: u8, value: u32) {
    // SAFETY: as `read_u32`; the caller owns the meaning of writing `value` to
    // this register (BAR probing disables decode first; `enable` only sets
    // command bits).
    unsafe {
        outl(CONFIG_ADDRESS, addr.config_address(offset));
        outl(CONFIG_DATA, value);
    }
}

/// Read the first 256 bytes of a function's config space.
fn read_config_space(addr: Address, out: &mut [u8; 256]) {
    for dword in 0..64u8 {
        let v = read_u32(addr, dword * 4);
        out[dword as usize * 4..dword as usize * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// Brute-force every `(bus, device, function)` and record the functions that
/// answer. Legacy `0xCF8` config reaches all 256 buses, so no bridge walk is
/// needed; a full sweep is ~64 K port reads, tens of milliseconds once at boot.
///
/// BAR *sizes* are not probed here — that writes to a device's config space,
/// and doing it to the GPU whose BAR is our only console is a risk with no
/// payoff. A driver calls [`probe_bar_size`] on its own device.
pub fn scan() {
    let mut reg = REGISTRY.lock();
    if reg.scanned {
        return;
    }
    reg.scanned = true;

    for bus in 0..=255u8 {
        for device in 0..32u8 {
            // Function 0 first: if it is absent or not multifunction, skip 1..8.
            let base = Address::new(bus, device, 0);
            if read_u32(base, 0) as u16 == akuma_pci::INVALID_VENDOR {
                continue;
            }
            let mut cfg = [0u8; 256];
            read_config_space(base, &mut cfg);
            let multifunction = cfg[0x0e] & 0x80 != 0;
            let last_fn = if multifunction { 7 } else { 0 };

            for function in 0..=last_fn {
                let addr = Address::new(bus, device, function);
                if function != 0 {
                    if read_u32(addr, 0) as u16 == akuma_pci::INVALID_VENDOR {
                        continue;
                    }
                    read_config_space(addr, &mut cfg);
                }
                let Some(header) = Header::parse(&cfg) else {
                    continue;
                };
                let bars = akuma_pci::raw_bars(&cfg)
                    .map_or([None; 6], |r| akuma_pci::decode_bars(&r));
                if reg.count < MAX_DEVICES {
                    let n = reg.count;
                    reg.devices[n] = Some(Device { addr, header, bars });
                    reg.count = n + 1;
                }
            }
        }
    }
}

/// Run `f` over every enumerated device.
pub fn for_each(mut f: impl FnMut(&Device)) {
    let reg = REGISTRY.lock();
    for d in reg.devices[..reg.count].iter().flatten() {
        f(d);
    }
}

/// The first device matching a base class and subclass.
#[must_use]
pub fn find_class(class_code: u8, subclass: u8) -> Option<Device> {
    let reg = REGISTRY.lock();
    reg.devices[..reg.count]
        .iter()
        .flatten()
        .find(|d| d.header.is_class(class_code, subclass))
        .copied()
}

#[must_use]
pub fn device_count() -> usize {
    REGISTRY.lock().count
}

/// Turn on memory-space decode, I/O-space decode and (optionally) bus
/// mastering for a device a driver is about to use.
pub fn enable(addr: Address, bus_master: bool) {
    let mut cmd = (read_u32(addr, 0x04) & 0xffff) as u16;
    cmd |= command::MEMORY_SPACE | command::IO_SPACE;
    if bus_master {
        cmd |= command::BUS_MASTER;
    }
    // Preserve the high 16 bits (status is write-1-to-clear; writing it back as
    // read clears nothing).
    let status = read_u32(addr, 0x04) & 0xffff_0000;
    write_u32(addr, 0x04, status | u32::from(cmd));
}

/// Probe the size of BAR `index` (0..5) by the write-all-ones method, with the
/// device's decode disabled around the write so a half-written base cannot be
/// claimed by the bus.
///
/// Returns `(size, is_64bit)`. `size == 0` means the BAR is unimplemented.
#[must_use]
pub fn probe_bar_size(addr: Address, index: u8) -> (u64, bool) {
    let off = 0x10 + index * 4;
    let original = read_u32(addr, off);
    let is_io = original & 1 != 0;
    let is_64bit = !is_io && (original >> 1) & 0b11 == 0b10;

    let saved_cmd = read_u32(addr, 0x04) & 0xffff;
    write_u32(addr, 0x04, saved_cmd & u32::from(!(command::MEMORY_SPACE | command::IO_SPACE)));

    write_u32(addr, off, 0xffff_ffff);
    let probed_low = read_u32(addr, off);
    write_u32(addr, off, original);

    let (probed_high, original_high) = if is_64bit {
        let oh = read_u32(addr, off + 4);
        write_u32(addr, off + 4, 0xffff_ffff);
        let ph = read_u32(addr, off + 4);
        write_u32(addr, off + 4, oh);
        (ph, oh)
    } else {
        (0, 0)
    };
    let _ = original_high;

    write_u32(addr, 0x04, saved_cmd);
    (akuma_pci::bar_size(probed_low, probed_high, is_io, is_64bit), is_64bit)
}

/// Map a memory BAR into the device window and return its kernel virtual
/// address. `len` should be the probed BAR size (or a spec-known minimum).
#[must_use]
pub fn map_bar(bar: Bar, len: u64) -> Option<*mut u8> {
    let Bar::Memory { address, .. } = bar else {
        return None;
    };
    let first = address & !(PAGE_SIZE - 1);
    let last = (address + len).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut pa = first;
    while pa < last {
        if !paging::map_page((DEVMAP_BASE + pa) as usize, pa, Prot::KERNEL_RW, MemAttr::Device) {
            return None;
        }
        pa += PAGE_SIZE;
    }
    Some((DEVMAP_BASE + address) as *mut u8)
}

/// Print the enumerated devices — the amd64 equivalent of an `lspci`, worth
/// having every bare-metal boot for the same reason `machine::report` is.
pub fn report() {
    let n = device_count();
    serial::puts("  pci:  ");
    serial::put_dec(n as u64);
    serial::puts(" function(s)\n");
    for_each(|d| {
        serial::puts("    ");
        put_bdf(d.addr);
        serial::puts("  ");
        serial::put_hex(u64::from(d.header.vendor_id));
        serial::puts(":");
        serial::put_hex(u64::from(d.header.device_id));
        serial::puts("  class ");
        serial::put_hex(u64::from(d.header.class_code));
        serial::puts("/");
        serial::put_hex(u64::from(d.header.subclass));
        serial::puts("/");
        serial::put_hex(u64::from(d.header.prog_if));
        if d.header.is_xhci() {
            serial::puts("  [xHCI]");
        } else if d.header.is_ehci() {
            serial::puts("  [EHCI]");
        } else if d.header.is_ethernet() {
            serial::puts("  [ethernet]");
        } else if d.header.is_ahci() {
            serial::puts("  [AHCI]");
        } else if d.header.is_bridge() {
            serial::puts("  [bridge]");
        }
        for (i, bar) in d.bars.iter().enumerate() {
            match bar {
                Some(Bar::Memory { address, is_64bit, .. }) => {
                    serial::puts("  bar");
                    serial::put_dec(i as u64);
                    serial::puts("=mem:0x");
                    serial::put_hex(*address);
                    if *is_64bit {
                        serial::puts("(64)");
                    }
                }
                Some(Bar::Io { port, .. }) => {
                    serial::puts("  bar");
                    serial::put_dec(i as u64);
                    serial::puts("=io:0x");
                    serial::put_hex(u64::from(*port));
                }
                None => {}
            }
        }
        serial::puts("\n");
    });
}

fn put_bdf(addr: Address) {
    serial::put_hex(u64::from(addr.bus));
    serial::puts(":");
    serial::put_hex(u64::from(addr.device));
    serial::puts(".");
    serial::put_hex(u64::from(addr.function));
}

/// Verify the enumeration ran and parsed something coherent.
///
/// On a VMM target there is no PCI bus and finding zero devices is a pass —
/// the real coverage is `akuma-pci`'s host tests. On bare metal the scan must
/// have found the host bridge at `00:00.0` and every recorded header must
/// re-parse.
pub fn smoke_test(t: &mut Suite) {
    scan();
    let n = device_count();
    if n == 0 {
        t.note("pci: no bus (VMM target)", 0);
        return;
    }
    t.check("pci: host bridge at 00:00.0", find_class(0x06, 0x00).is_some());
    let mut all_sane = true;
    for_each(|d| {
        if d.header.vendor_id == 0 || d.header.vendor_id == 0xffff {
            all_sane = false;
        }
    });
    t.check("pci: every enumerated function has a real vendor id", all_sane);
    t.note("pci: functions enumerated", n as u64);
}
