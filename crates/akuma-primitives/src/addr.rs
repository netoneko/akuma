//! Kernel address translation and the fixed device-mapping window.
//!
//! # Why these are here
//!
//! `akuma-virtio` needed exactly three things from `akuma-exec`'s `mmu` — the two
//! translators and [`DEV_VIRTIO_VA`] — and that dependency is what kept
//! `akuma-net` compiling the 23.8k-line execution crate even after
//! `PreemptGuard` moved out (`akuma-net → akuma-virtio → akuma-exec`). Counting
//! `use` statements said the edge was gone; `cargo tree` said otherwise.
//!
//! # These must stay `#[inline(always)]` functions, not hooks
//!
//! Both translators are the identity, and that is load-bearing history rather
//! than an accident. `akuma-net` used to reach the kernel's translators through
//! `NetRuntime` function pointers precisely so it could avoid depending on
//! `akuma-exec` — and Phase 3 of
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` **deleted** that
//! indirection because it "cost a spinlocked struct read on the per-packet DMA
//! path to reach two identity functions".
//!
//! So this module must not reintroduce a hook. It relocates the identity-map
//! assumption into the leaf crate rather than introducing it — the assumption was
//! already baked into `#[inline(always)] fn virt_to_phys(v) -> usize { v }`.
//!
//! **If the kernel ever gains a non-identity kernel map, this is one of the
//! places that has to change**, and it cannot become a runtime hook without
//! re-paying the cost Phase 3 measured away. The honest options at that point are
//! a compile-time offset constant or a per-region translation the caller passes
//! in — not a registered function pointer.

/// Kernel virtual address → physical address.
///
/// The kernel map is the identity, so this is a no-op that exists to name the
/// conversion at call sites and to give the assumption a single home. See the
/// module header before changing it.
#[inline(always)]
#[must_use]
pub fn virt_to_phys(vaddr: usize) -> usize {
    vaddr
}

/// Physical address → kernel-mapped pointer.
///
/// The kernel map is the identity. See the module header before changing it.
#[inline(always)]
#[must_use]
pub fn phys_to_virt(paddr: usize) -> *mut u8 {
    paddr as *mut u8
}

// =============================================================================
// Fixed device-mapping window (L0[1])
// =============================================================================
//
// One 4 KB page per device, mapped Device-nGnRnE at boot. These are *virtual*
// addresses in the kernel's device window, deliberately outside the identity
// region so a stray Normal-memory access cannot land on MMIO.
//
// This is the weaker half of the case for living in a leaf crate: the window is
// the L0[1] device mapping, which is genuinely `mmu`'s business, and only
// `DEV_VIRTIO_VA` is actually needed outside `akuma-exec`. They move as one table
// rather than one constant because splitting a fixed layout across two crates is
// how layouts drift. `akuma_exec::mmu` re-exports them, so no call site moved.
//
// The alternative — passing the virtio base in as a parameter — was rejected:
// Phase 3 removed the `mmio_addrs` parameter from `akuma_net::init` /
// `smoltcp_net::init` / `rump_tap::init` precisely because every caller passed
// the same table.

/// GICv2/v3 distributor.
pub const DEV_GIC_DIST_VA: usize = 0x80_0000_0000;
/// GICv2 CPU interface.
pub const DEV_GIC_CPU_VA: usize = 0x80_0000_1000;
/// PL011 UART — the console.
pub const DEV_UART_VA: usize = 0x80_0000_2000;
/// QEMU fw_cfg.
pub const DEV_FW_CFG_VA: usize = 0x80_0000_3000;
/// Base of the virtio-mmio slot array. Slots are `DEV_VIRTIO_VA + n * 0x200`.
pub const DEV_VIRTIO_VA: usize = 0x80_0000_4000;
/// GICv3 redistributor, CPU0 RD_base frame (PA 0x080A_0000). GICR_WAKER lives here.
pub const DEV_GICR_RD_VA: usize = 0x80_0000_5000;
/// GICv3 redistributor, CPU0 SGI_base frame (PA 0x080B_0000). SGI/PPI enable,
/// priority and group registers live here.
pub const DEV_GICR_SGI_VA: usize = 0x80_0000_6000;

#[cfg(test)]
mod tests {
    use super::{DEV_UART_VA, DEV_VIRTIO_VA, phys_to_virt, virt_to_phys};

    #[test]
    fn translation_is_the_identity() {
        // Pinned deliberately: the whole reason these can be `#[inline(always)]`
        // free functions in a leaf crate rather than a registered hook is that
        // they are the identity. If this test has to change, read the module
        // header first — a hook is not the answer.
        assert_eq!(virt_to_phys(0), 0);
        assert_eq!(virt_to_phys(0x4010_0000), 0x4010_0000);
        assert_eq!(virt_to_phys(usize::MAX), usize::MAX);
        assert_eq!(phys_to_virt(0x4010_0000) as usize, 0x4010_0000);
    }

    #[test]
    fn device_window_pages_are_distinct_and_page_aligned() {
        // A collision here silently aliases two devices' MMIO.
        let all = [
            super::DEV_GIC_DIST_VA,
            super::DEV_GIC_CPU_VA,
            DEV_UART_VA,
            super::DEV_FW_CFG_VA,
            DEV_VIRTIO_VA,
            super::DEV_GICR_RD_VA,
            super::DEV_GICR_SGI_VA,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a % 4096, 0, "device VA {a:#x} is not page-aligned");
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two devices share VA {a:#x}");
            }
        }
    }
}
