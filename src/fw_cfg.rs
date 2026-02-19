//! QEMU fw_cfg MMIO driver for AArch64
//!
//! The fw_cfg device provides firmware configuration data to the guest.
//! On the AArch64 virt machine it is exposed as MMIO at:
//!   - 0x0902_0000: data register (8 bytes, read/write)
//!   - 0x0902_0008: selector register (2 bytes, write)
//!   - 0x0902_0010: DMA register (8 bytes, write)
//!
//! We use the legacy selector+data interface for simplicity and reliability.
//! The DMA interface is used only for write operations (required for ramfb).
//!
//! Reference: <https://www.qemu.org/docs/master/specs/fw_cfg.html>

use core::ptr::{addr_of, read_volatile, write_volatile};

/// MMIO base address for fw_cfg on AArch64 virt
const FW_CFG_BASE: usize = 0x0902_0000;
/// Data register: read/write 1 byte at a time
const FW_CFG_DATA: *mut u8 = FW_CFG_BASE as *mut u8;
/// Selector register: write a 16-bit big-endian value to select a key
const FW_CFG_SELECTOR: *mut u16 = (FW_CFG_BASE + 0x08) as *mut u16;
/// DMA register: write a 64-bit big-endian physical address
const FW_CFG_DMA: *mut u64 = (FW_CFG_BASE + 0x10) as *mut u64;

/// Well-known selector for the file directory
const FW_CFG_FILE_DIR: u16 = 0x0019;

// DMA control bits
const FW_CFG_DMA_CTL_WRITE: u32 = 0x10;
const FW_CFG_DMA_CTL_SELECT: u32 = 0x08;
const FW_CFG_DMA_CTL_ERROR: u32 = 0x01;

/// DMA access descriptor – must be naturally aligned
#[repr(C)]
struct FWCfgDmaAccess {
    control: u32,
    len: u32,
    addr: u64,
}

/// Directory entry as stored by QEMU (64 bytes each)
#[repr(C)]
pub struct FWCfgFile {
    pub size: u32,    // big-endian
    pub select: u16,  // big-endian
    _reserved: u16,
    pub name: [u8; 56],
}

/// Select a fw_cfg entry by its selector number.
fn select(key: u16) {
    unsafe {
        // The selector register expects big-endian on MMIO
        write_volatile(FW_CFG_SELECTOR, key.to_be());
    }
}

/// Read `n` bytes from the currently selected entry via the data register.
fn read_bytes(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        unsafe {
            *byte = read_volatile(FW_CFG_DATA);
        }
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
    // Select the file directory
    select(FW_CFG_FILE_DIR);

    // First 4 bytes: number of entries (big-endian u32)
    let num_entries = read_be_u32();

    crate::console::print("[fw_cfg] Directory has ");
    crate::console::print_dec(num_entries as usize);
    crate::console::print(" entries\n");

    // Each entry is 64 bytes: size(4) + select(2) + reserved(2) + name(56)
    for i in 0..num_entries {
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
            crate::console::print_hex(sel as u64);
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
/// # Safety
/// `data` must be a valid byte slice whose contents match what QEMU expects.
pub unsafe fn write_entry(selector: u16, data: &[u8]) {
    // Build DMA descriptor (all fields big-endian)
    let dma = FWCfgDmaAccess {
        control: ((selector as u32) << 16
            | FW_CFG_DMA_CTL_SELECT
            | FW_CFG_DMA_CTL_WRITE)
            .to_be(),
        len: (data.len() as u32).to_be(),
        addr: (data.as_ptr() as u64).to_be(),
    };

    // Write the physical address of the descriptor to the DMA register (big-endian)
    let desc_phys = addr_of!(dma) as u64;
    unsafe {
        write_volatile(FW_CFG_DMA, desc_phys.to_be());
    }

    // Spin-wait until DMA completes (control field is zeroed by QEMU)
    loop {
        let ctrl = unsafe { read_volatile(addr_of!(dma.control)) };
        let ctrl_host = u32::from_be(ctrl);
        if ctrl_host == 0 || ctrl_host & FW_CFG_DMA_CTL_ERROR != 0 {
            break;
        }
        core::hint::spin_loop();
    }
}
