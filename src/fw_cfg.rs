//! QEMU fw_cfg MMIO driver for AArch64
//!
//! The fw_cfg device provides firmware configuration data to the guest.
//! Physical MMIO at 0x0902_0000; accessed via remapped VA (see `mmu::DEV_FW_CFG_VA`).
//!
//! We use the legacy selector+data interface for simplicity and reliability.
//! The DMA interface is used only for write operations (required for ramfb).
//!
//! Reference: <https://www.qemu.org/docs/master/specs/fw_cfg.html>

use core::sync::atomic::{AtomicU32, Ordering};

use akuma_exec::mmu::virt_to_phys;
use akuma_primitives::mmio::MmioReg;

/// Remapped VA for fw_cfg (physical 0x0902_0000 via L0[1])
const FW_CFG_BASE: usize = akuma_exec::mmu::DEV_FW_CFG_VA;

/// The fw_cfg register file.
///
/// The three registers differ in width, which is why they cannot be one
/// [`MmioReg`]: the data port is byte-at-a-time, the selector is a 16-bit
/// big-endian key, and the DMA port takes a 64-bit big-endian address.
struct FwCfgRegs {
    /// Data register: read/write 1 byte at a time.
    data: MmioReg<u8>,
    /// Selector register: write a 16-bit big-endian value to select a key.
    selector: MmioReg<u16>,
    /// DMA register: write a 64-bit big-endian physical address.
    dma: MmioReg<u64>,
}

/// The whole register file, vouched for once.
///
/// `const` rather than built at init: the device window is at a fixed VA, so
/// naming it costs nothing and adds no init-order dependency. One `unsafe` for
/// the whole file rather than one per register is the point of
/// [`akuma_primitives::mmio`] — the fact that needs a human's word is "this
/// window is the fw_cfg device", and it is true once per device, not once per
/// register.
///
/// SAFETY: `DEV_FW_CFG_VA` is the kernel's fixed device mapping of the QEMU
/// fw_cfg MMIO window, established at boot before any of these are touched, and
/// the offsets and widths are the ones the fw_cfg spec defines.
const REGS: FwCfgRegs = unsafe {
    FwCfgRegs {
        data: MmioReg::new(FW_CFG_BASE),
        selector: MmioReg::new(FW_CFG_BASE + 0x08),
        dma: MmioReg::new(FW_CFG_BASE + 0x10),
    }
};

/// Does this machine have an fw_cfg device at all?
///
/// QEMU virt does; Firecracker does not — it has no `fw_cfg` and nothing is
/// mapped at [`akuma_exec::mmu::DEV_FW_CFG_VA`] there, so *touching* one of the
/// registers above is an EL1 translation fault rather than a read of zeroes.
/// That is exactly how this surfaced: the first Firecracker boot got all the way
/// through device init and then took `EC=0x25` (data abort, same EL) with
/// `FAR=0x8000012008` — `DEV_FW_CFG_VA + 0x08`, the selector register — from
/// `ramfb::init`. See `docs/archive/AKUMA_FIRECRACKER_KVM.md`.
///
/// Every public entry point checks this, so callers do not need to: they get the
/// same "not found" answer they would get from a machine whose fw_cfg simply has
/// no such file.
const AVAILABLE: bool = crate::platform::machine::FW_CFG_PA.is_some();

/// Well-known selector for the file directory
const FW_CFG_FILE_DIR: u16 = 0x0019;

// DMA control bits
const FW_CFG_DMA_CTL_WRITE: u32 = 0x10;
const FW_CFG_DMA_CTL_SELECT: u32 = 0x08;
const FW_CFG_DMA_CTL_ERROR: u32 = 0x01;

/// DMA access descriptor – must be naturally aligned.
///
/// `control` is an [`AtomicU32`] rather than a plain `u32` because it is the one
/// field the *device* writes: QEMU zeroes it (or sets [`FW_CFG_DMA_CTL_ERROR`])
/// when the transfer completes, while this CPU is polling it. A plain read of a
/// location another agent is concurrently writing is a data race no matter how
/// volatile it is spelled; an atomic load is the operation that is actually
/// defined, and `Acquire` is what orders the completion against whatever the
/// transfer wrote. Same size, same alignment, same `repr(C)` layout as the `u32`
/// the spec describes.
#[repr(C)]
struct FWCfgDmaAccess {
    control: AtomicU32,
    len: u32,
    addr: u64,
}

/// Select a fw_cfg entry by its selector number.
fn select(key: u16) {
    // The selector register expects big-endian on MMIO
    REGS.selector.write(key.to_be());
}

/// Read `n` bytes from the currently selected entry via the data register.
fn read_bytes(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = REGS.data.read();
    }
}

/// Read a big-endian u32 from the currently selected entry.
fn read_be_u32() -> u32 {
    let mut buf = [0u8; 4];
    read_bytes(&mut buf);
    u32::from_be_bytes(buf)
}

/// Look up a fw_cfg file entry by name.
///
/// Returns `Some((selector, size))` if found, `None` otherwise.
pub fn find_file(name: &str) -> Option<(u16, u32)> {
    if !AVAILABLE {
        return None;
    }
    // Select the file directory
    select(FW_CFG_FILE_DIR);

    // First 4 bytes: number of entries (big-endian u32)
    let num_entries = read_be_u32();

    crate::console::print("[fw_cfg] Directory has ");
    crate::console::print_dec(num_entries as usize);
    crate::console::print(" entries\n");

    // Each entry is 64 bytes: size(4) + select(2) + reserved(2) + name(56)
    for _i in 0..num_entries {
        let mut entry_buf = [0u8; 64];
        read_bytes(&mut entry_buf);

        // Parse entry fields (all big-endian)
        let size = u32::from_be_bytes([entry_buf[0], entry_buf[1], entry_buf[2], entry_buf[3]]);
        let sel = u16::from_be_bytes([entry_buf[4], entry_buf[5]]);

        // Name starts at offset 8, null-terminated
        let name_bytes = &entry_buf[8..64];
        let nul_pos = name_bytes.iter().position(|&b| b == 0).unwrap_or(56);
        let entry_name = core::str::from_utf8(&name_bytes[..nul_pos]).unwrap_or("");

        if entry_name == name {
            crate::console::print("[fw_cfg] Found '");
            crate::console::print(entry_name);
            crate::console::print("' selector=0x");
            crate::console::print_hex(u64::from(sel));
            crate::console::print(" size=");
            crate::console::print_dec(size as usize);
            crate::console::print("\n");
            return Some((sel, size));
        }
    }

    crate::console::print("[fw_cfg] '");
    crate::console::print(name);
    crate::console::print("' not found\n");
    None
}

/// Write `data` to the fw_cfg entry identified by `selector` using DMA.
///
/// DMA is required for write operations — the data register is read-only
/// for most entries.
///
/// Safe, though it hands a device a physical address: both addresses named in
/// the descriptor are derived from live borrows this call owns — `data` for its
/// whole body, and the descriptor itself a local — and the spin below does not
/// return until QEMU reports the transfer finished, so neither address outlives
/// the device's use of it. What the caller still owes is *correctness*, not
/// safety: `data` must be the wire layout the selected entry expects, and a
/// mismatch misconfigures the device rather than corrupting memory.
pub fn write_entry(selector: u16, data: &[u8]) {
    if !AVAILABLE {
        return;
    }
    // Build DMA descriptor (all fields big-endian)
    let dma = FWCfgDmaAccess {
        control: AtomicU32::new(
            (u32::from(selector) << 16 | FW_CFG_DMA_CTL_SELECT | FW_CFG_DMA_CTL_WRITE).to_be(),
        ),
        len: (data.len() as u32).to_be(),
        addr: (virt_to_phys(data.as_ptr() as usize) as u64).to_be(),
    };

    // Write the physical address of the descriptor to the DMA register
    // (big-endian). The kernel map is the identity, so `virt_to_phys` is a no-op
    // — it is here to name the conversion rather than leave a bare `as u64` cast
    // standing in for it, since a device reads these as physical addresses.
    let desc_phys = virt_to_phys(&raw const dma as usize) as u64;
    REGS.dma.write(desc_phys.to_be());

    // Spin-wait until DMA completes (control field is zeroed by QEMU).
    //
    // Deliberately not an `MmioReg`: this polls the descriptor in ordinary RAM,
    // which QEMU writes back by DMA. It is not a device register, so it does not
    // belong to the register newtype.
    loop {
        let ctrl_host = u32::from_be(dma.control.load(Ordering::Acquire));
        if ctrl_host == 0 || ctrl_host & FW_CFG_DMA_CTL_ERROR != 0 {
            break;
        }
        core::hint::spin_loop();
    }
}
