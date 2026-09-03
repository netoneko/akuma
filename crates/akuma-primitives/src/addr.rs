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
//!
//! # That happened, 2026-09-04
//!
//! The amd64 kernel runs in the upper half: RAM is reached through a physmap at
//! `0xFFFF_8000_0000_0000 + pa`, not the identity. So this module took the first
//! of the two options its own paragraph above prescribed — **a compile-time
//! offset constant**, `cfg`'d per architecture, still `#[inline(always)]`, still
//! not a hook. On AArch64 the offset is zero and every function below compiles to
//! exactly what it did before.
//!
//! Two things fell out of doing it, and both are the point rather than incidental:
//!
//! * **The gate is the conjunction**, `all(target_os = "none", target_arch = ...)`.
//!   `target_arch = "x86_64"` alone would fire under `cargo test` on an x86_64
//!   host and silently offset every translation in a host test. That is exactly
//!   the bug `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §0 was written about;
//!   it is one gate away in either direction.
//! * **MMIO is a different question from RAM.** [`phys_to_virt`] answers "where is
//!   this *page of memory*"; [`mmio_phys_to_virt`] answers "where is this *device
//!   register*". On AArch64 both are the identity and the distinction is
//!   invisible, which is why there was only one function. On amd64 they are
//!   different windows with different cacheability — RAM is writeback in the
//!   physmap, MMIO is uncached in a device window — and conflating them hands a
//!   driver a cached alias of a register file.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Offset between a kernel RAM pointer and the physical address behind it.
///
/// Zero on AArch64: the kernel map is the identity there, and every function
/// below folds to a no-op exactly as it did before this constant existed.
#[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
pub const PHYSMAP_OFFSET: usize = 0;

/// Base of the amd64 kernel's physmap window (PML4 slot 256).
///
/// Duplicated in `amd64/src/phys.rs` as `PHYSMAP_BASE` — which re-exports this
/// rather than restating it, so there is one value. It cannot live only there:
/// `akuma-primitives` is a leaf crate and cannot depend on a binary.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub const PHYSMAP_OFFSET: usize = 0xFFFF_8000_0000_0000;

/// Base of the amd64 kernel's device (MMIO) window, PML4 slot 257.
///
/// A second window rather than a range inside the physmap, because device
/// registers must be mapped uncached and RAM must not be. See `amd64/src/phys.rs`.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub const DEVMAP_OFFSET: usize = 0xFFFF_8080_0000_0000;

/// MMIO offset on every other target: the identity, as for RAM.
#[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
pub const DEVMAP_OFFSET: usize = 0;

/// Kernel virtual address → physical address.
///
/// A subtraction rather than a no-op since 2026-09-04; see the module header. On
/// AArch64 [`PHYSMAP_OFFSET`] is zero and this is the identity it always was.
///
/// # Panics
/// If `vaddr` is not in the kernel's RAM window. This is a DMA path and the
/// header warns against cost, so it is worth saying what the cost is: one
/// compare and a not-taken branch, against a device round trip. What it buys is
/// that handing a driver an address from the *wrong window* — a `&'static [u8]`
/// in the kernel image, say, whose window has a different offset — fails loudly
/// instead of programming a plausible-looking wrong physical address into a
/// descriptor and letting the device DMA over unrelated memory.
#[inline(always)]
#[must_use]
pub fn virt_to_phys(vaddr: usize) -> usize {
    assert!(
        vaddr >= PHYSMAP_OFFSET,
        "virt_to_phys on an address outside the kernel RAM window"
    );
    vaddr - PHYSMAP_OFFSET
}

/// Physical address → kernel-mapped pointer, **for RAM**.
///
/// For device registers use [`mmio_phys_to_virt`]: on amd64 they are different
/// windows with different cacheability, and on AArch64 both are the identity.
#[inline(always)]
#[must_use]
pub fn phys_to_virt(paddr: usize) -> *mut u8 {
    (paddr + PHYSMAP_OFFSET) as *mut u8
}

/// Physical address → kernel-mapped pointer, **for MMIO**.
///
/// Separate from [`phys_to_virt`] because the two are separate questions the
/// moment the kernel stops being identity-mapped: RAM is writeback-cached in the
/// physmap, device registers are uncached in a window of their own. On AArch64
/// both offsets are zero, so this is the identity and costs nothing.
#[inline(always)]
#[must_use]
pub fn mmio_phys_to_virt(paddr: usize) -> *mut u8 {
    (paddr + DEVMAP_OFFSET) as *mut u8
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

// Each device gets a *span*, not a single page. The one-page-per-device version
// of this table is what made every `GICD_IROUTER` write land on the
// redistributor: `GICD_IROUTER` is at distributor offset 0x6000, and
// `DEV_GIC_DIST_VA + 0x6000` was exactly `DEV_GICR_SGI_VA`. See
// `docs/archive/GICD_IROUTER_ALIASING.md`. Spans are asserted non-overlapping
// by `device_window_spans_do_not_overlap` below and by `DEV_WINDOW_NO_OVERLAP`.

/// GICv2/v3 distributor.
///
/// **64 KiB**, not one page: `GICD_IROUTER` starts at offset 0x6000 and the
/// register file runs to 0xE000 for INTID 1020. A 4 KiB mapping here silently
/// aliased the distributor's upper registers onto whatever device followed it.
pub const DEV_GIC_DIST_VA: usize = 0x80_0000_0000;
/// Bytes mapped for the distributor. GICv3 mandates 64 KiB.
pub const DEV_GIC_DIST_SIZE: usize = 0x1_0000;

/// GICv2 CPU interface. Unused under GICv3.
pub const DEV_GIC_CPU_VA: usize = 0x80_0001_0000;
/// PL011 UART — the console.
pub const DEV_UART_VA: usize = 0x80_0001_1000;
// 0x80_0001_2000 is a HOLE. It was `DEV_FW_CFG_VA` (QEMU fw_cfg) until the
// framebuffer path was removed 2026-08-31 — fw_cfg's only consumer was ramfb
// (`docs/archive/FRAMEBUFFER_REMOVED.md`). The window is deliberately not
// re-packed: these are just addresses in a 2 MB span, and shuffling them to
// close a gap would churn every device for nothing. A new device may take it.
/// GICv3 redistributor, CPU0 RD_base frame. `GICR_WAKER` lives here.
///
/// One page is enough: every redistributor register this kernel touches
/// (`GICR_WAKER` at 0x14) is inside the first 4 KiB of the frame.
pub const DEV_GICR_RD_VA: usize = 0x80_0001_3000;
/// GICv3 redistributor, CPU0 SGI_base frame. SGI/PPI enable, priority and group
/// registers live here — all inside the first 4 KiB.
pub const DEV_GICR_SGI_VA: usize = 0x80_0001_4000;

/// Base of the virtio-mmio slot array.
///
/// Slot *n* is at `DEV_VIRTIO_VA + n * virtio_stride()`. The stride is a
/// **runtime** value because it is machine-specific: QEMU virt packs 8 slots
/// 0x200 apart inside a single page, while Firecracker gives each device its own
/// 0x1000 page. [`DEV_VIRTIO_SIZE`] is sized for the larger of the two.
pub const DEV_VIRTIO_VA: usize = 0x80_0002_0000;
/// Bytes reserved for the virtio-mmio slot array — 8 slots at a 0x1000 stride.
pub const DEV_VIRTIO_SIZE: usize = 8 * 0x1000;

/// Runtime virtio-mmio slot geometry.
///
/// The stride is machine-specific — QEMU virt packs eight slots 0x200 apart
/// inside one page, Firecracker gives each device its own 0x1000 page — and the
/// count is what the machine actually instantiated. Both are set once during
/// early boot, before any driver probes, and read-only afterwards, so a plain
/// relaxed atomic is enough: no probe runs on a per-packet path (see the module
/// header on why this file must not grow a *hook*).
static VIRTIO_STRIDE: AtomicUsize = AtomicUsize::new(0x200);
static VIRTIO_SLOTS: AtomicUsize = AtomicUsize::new(8);

/// Where the slot array starts, as a kernel VA.
///
/// Runtime for the same reason the stride is, one architecture later: on AArch64
/// the window is a fixed slot in the kernel's L0[1] device map ([`DEV_VIRTIO_VA`],
/// the default here, so nothing about that kernel changes). On amd64 there is no
/// fixed device map — the machine announces virtio-MMIO on the boot command line
/// (`virtio_mmio.device=4K@0xc0001000:5` under Firecracker,
/// `512@0xfeb00000` under QEMU `microvm`, both measured) and the kernel maps it
/// where it lands. A const cannot express an address the VMM chooses.
///
/// This costs `virtio_slot_va` a second relaxed load, on a path that runs once
/// per probe and never per packet.
static VIRTIO_BASE_VA: AtomicUsize = AtomicUsize::new(DEV_VIRTIO_VA);

/// Bytes between consecutive virtio-mmio slots.
#[inline]
#[must_use]
pub fn virtio_stride() -> usize {
    VIRTIO_STRIDE.load(Ordering::Relaxed)
}

/// Number of virtio-mmio slots the machine exposes.
#[inline]
#[must_use]
pub fn virtio_slots() -> usize {
    VIRTIO_SLOTS.load(Ordering::Relaxed)
}

/// Kernel VA the virtio-mmio slot array starts at.
#[inline]
#[must_use]
pub fn virtio_base_va() -> usize {
    VIRTIO_BASE_VA.load(Ordering::Relaxed)
}

/// Kernel VA of virtio-mmio slot `i`.
#[inline]
#[must_use]
pub fn virtio_slot_va(i: usize) -> usize {
    virtio_base_va() + i * virtio_stride()
}

/// Install the machine's virtio-mmio geometry. Call during early boot, before
/// any driver probes. `slots * stride` must fit in [`DEV_VIRTIO_SIZE`].
///
/// Leaves the window base at [`DEV_VIRTIO_VA`]; a machine that places the array
/// somewhere else calls [`set_virtio_window`] instead.
pub fn set_virtio_geometry(stride: usize, slots: usize) {
    set_virtio_window(DEV_VIRTIO_VA, stride, slots);
}

/// Install the machine's virtio-mmio window: where the slot array is, how far
/// apart the slots are, and how many there are.
///
/// Call during early boot, before any driver probes. `slots * stride` must fit in
/// [`DEV_VIRTIO_SIZE`], which is what the caller reserved VA for.
pub fn set_virtio_window(base_va: usize, stride: usize, slots: usize) {
    debug_assert!(stride > 0 && slots > 0, "degenerate virtio geometry");
    debug_assert!(base_va != 0, "virtio window at VA 0");
    debug_assert!(
        slots.saturating_mul(stride) <= DEV_VIRTIO_SIZE,
        "virtio slot array does not fit its VA reservation"
    );
    VIRTIO_BASE_VA.store(base_va, Ordering::Relaxed);
    VIRTIO_STRIDE.store(stride, Ordering::Relaxed);
    VIRTIO_SLOTS.store(slots, Ordering::Relaxed);
}

/// Every device span in the L0[1] window, as `(base, size)`.
///
/// The single source of truth for the layout above. `akuma_exec::mmu` walks this
/// to build the page tables, so adding a device here is all it takes to get it
/// mapped — and the overlap assertion below then covers it for free.
pub const DEV_WINDOW_SPANS: &[(usize, usize)] = &[
    (DEV_GIC_DIST_VA, DEV_GIC_DIST_SIZE),
    (DEV_GIC_CPU_VA, 0x1000),
    (DEV_UART_VA, 0x1000),
    (DEV_GICR_RD_VA, 0x1000),
    (DEV_GICR_SGI_VA, 0x1000),
    (DEV_VIRTIO_VA, DEV_VIRTIO_SIZE),
];

/// Base of the whole device window. Every span must live inside
/// `[DEV_WINDOW_VA, DEV_WINDOW_VA + DEV_WINDOW_SIZE)`, which is what one L2
/// entry's worth of L3 (512 pages, 2 MiB) covers.
pub const DEV_WINDOW_VA: usize = 0x80_0000_0000;
/// Size of the device window: one L3 table.
pub const DEV_WINDOW_SIZE: usize = 512 * 0x1000;

/// Compile-time proof that no two device spans overlap and all are page-aligned
/// and in-window.
///
/// This is the assertion whose absence let the `GICD_IROUTER` aliasing survive:
/// the old check compared base addresses only, so a 64 KiB device declared as
/// one page passed it. Evaluating this `const` is what enforces the layout —
/// `DEV_WINDOW_NO_OVERLAP` is referenced from `akuma_exec::mmu` so it cannot be
/// optimized into irrelevance.
pub const DEV_WINDOW_NO_OVERLAP: () = {
    let n = DEV_WINDOW_SPANS.len();
    let mut i = 0;
    while i < n {
        let (a_base, a_size) = DEV_WINDOW_SPANS[i];
        assert!(a_base % 4096 == 0, "device span base is not page-aligned");
        assert!(a_size % 4096 == 0, "device span size is not a page multiple");
        assert!(a_size > 0, "device span is empty");
        assert!(
            a_base >= DEV_WINDOW_VA && a_base - DEV_WINDOW_VA + a_size <= DEV_WINDOW_SIZE,
            "device span escapes the L0[1] device window"
        );
        let mut j = i + 1;
        while j < n {
            let (b_base, b_size) = DEV_WINDOW_SPANS[j];
            assert!(
                a_base + a_size <= b_base || b_base + b_size <= a_base,
                "two device spans overlap"
            );
            j += 1;
        }
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::{phys_to_virt, virt_to_phys};

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
    fn device_window_spans_do_not_overlap() {
        // Forcing the const to be evaluated is the actual check; this test exists
        // so `cargo test` reports it by name rather than only failing a build.
        //
        // The predecessor of this test compared *base addresses* only, which is
        // why it passed for years while `DEV_GIC_DIST_VA + 0x6000` aliased
        // `DEV_GICR_SGI_VA` — the distributor is 64 KiB but was declared as one
        // page. See docs/archive/GICD_IROUTER_ALIASING.md.
        let () = super::DEV_WINDOW_NO_OVERLAP;

        for (i, &(a_base, a_size)) in super::DEV_WINDOW_SPANS.iter().enumerate() {
            assert_eq!(a_base % 4096, 0, "device VA {a_base:#x} is not page-aligned");
            for &(b_base, b_size) in &super::DEV_WINDOW_SPANS[i + 1..] {
                assert!(
                    a_base + a_size <= b_base || b_base + b_size <= a_base,
                    "spans {a_base:#x}+{a_size:#x} and {b_base:#x}+{b_size:#x} overlap"
                );
            }
        }
    }

    #[test]
    fn gicd_irouter_stays_inside_the_distributor_span() {
        // The specific regression: GICD_IROUTER is at distributor offset 0x6000,
        // 8 bytes per INTID from INTID 0. The highest INTID this kernel can pass
        // to enable_irq is 1019, so the last byte touched is 0x6000 + 1019*8 + 8.
        const IROUTER: usize = 0x6000;
        const MAX_INTID: usize = 1019;
        let last = IROUTER + MAX_INTID * 8 + 8;
        assert!(
            last <= super::DEV_GIC_DIST_SIZE,
            "GICD_IROUTER for INTID {MAX_INTID} ends at {last:#x}, past the \
             {:#x}-byte distributor mapping",
            super::DEV_GIC_DIST_SIZE
        );
    }
}
