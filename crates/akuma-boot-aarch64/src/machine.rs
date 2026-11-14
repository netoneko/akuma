//! The two physical-map facts the boot assembly needs before any FDT can be
//! read. `akuma_kernel_core::platform` re-exports them into its machine
//! descriptions, so they are still written down once.

/// PL011 UART: the console, mapped by the boot tables so the first line of
/// `rust_start` can print.
#[cfg(not(feature = "platform-firecracker"))]
pub const UART_PA: usize = 0x0900_0000;
/// Firecracker: `layout::SERIAL_MEM_START` = `RTC_MEM_START + MMIO_LEN`
/// = `0x4000_0000 + 0x1000 + 0x1000`. PL011-compatible.
#[cfg(feature = "platform-firecracker")]
pub const UART_PA: usize = 0x4000_2000;

/// Is the 1 GiB identity block at `0x4000_0000` device memory?
///
/// No on QEMU virt, where that block *is* RAM. Yes on Firecracker, where
/// `0x4000_0000..0x8000_0000` is `MMIO32` (the RTC, the serial port and every
/// virtio slot): mapping it Normal-cacheable would alias live device registers
/// with mismatched memory attributes, which the ARM ARM leaves CONSTRAINED
/// UNPREDICTABLE.
#[cfg(not(feature = "platform-firecracker"))]
pub const MMIO_WINDOW_IS_DEVICE: bool = false;
#[cfg(feature = "platform-firecracker")]
pub const MMIO_WINDOW_IS_DEVICE: bool = true;
