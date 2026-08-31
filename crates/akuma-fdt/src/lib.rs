//! Turning a boot-time pointer into a flattened device tree, once.
//!
//! # Why this crate exists
//!
//! Three places in the kernel needed the DTB and each materialised it for
//! itself: `main.rs` (RAM size), `platform.rs` (the device map, via
//! `akuma-firecracker`) and `smp_shared.rs` (CPU topology and the PSCI conduit).
//! That was six `unsafe` operations across three files — two speculative
//! `read_volatile`s to find the blob and three `Fdt::from_ptr` calls to parse it
//! — all discharging one obligation: *this address holds a complete FDT*.
//!
//! The obligation is true once per boot, not once per consumer. [`locate`] is
//! the one `unsafe fn` that discharges it; everything downstream takes a
//! [`Dtb`] or an [`Fdt`] and is ordinary safe code. `fdt::Fdt::new(&[u8])` is
//! safe — only `from_ptr` is not, and only because it has to dereference to
//! discover the blob's length. Discovering that length in one place is the whole
//! trick.
//!
//! This is the same move `akuma-firecracker` made on 2026-08-30 when
//! `describe_ptr` became `describe_fdt` and its `Fdt::from_ptr` moved out to
//! `platform::install_fdt_device_map`, which bought that crate
//! `#![forbid(unsafe_code)]`. That relocation put the pointer work in a caller;
//! this one gives it a home, so `platform::install_fdt_device_map` stops being
//! an `unsafe fn` too.
//!
//! # Stricter than what it replaces
//!
//! `Fdt::from_ptr` reads a 40-byte header, `unwrap()`s the parse, and builds a
//! slice of whatever `totalsize` it finds — with no bound. A wild pointer that
//! happens to parse yields a multi-gigabyte slice. [`locate`] reads **8 bytes**,
//! byte at a time (so it carries no alignment obligation, which `read_volatile`
//! of a `u32` would), checks the magic *before* trusting anything, and rejects a
//! `totalsize` outside [`MIN_TOTALSIZE`]..=[`MAX_TOTALSIZE`].
//!
//! Two of the replaced call sites did no validation at all when the bootloader
//! supplied a non-zero pointer; one checked the magic but not the size; only
//! `main.rs`'s scan checked both. Now every path gets the strictest of the three.
//!
//! The validation itself is [`header_totalsize`] — pure, byte-slice in, and unit
//! tested on the host, which is where the endianness bug would have lived.
//!
//! # Lifetimes, and why `Dtb` does not hold `&'static [u8]`
//!
//! The DTB's memory is **not** valid for the kernel's lifetime, which is exactly
//! why `smp_shared::probe_dtb` exists: it snapshots CPU topology early because
//! on large-RAM configs the heap can be placed on top of the blob. A `'static`
//! blob would be a lie the type system would then help propagate.
//!
//! So [`locate`] returns `Dtb<'a>` for a caller-chosen `'a`, and the caller's
//! obligation is to pick one that ends before the memory can be reused. In
//! practice `kernel_main` binds it to a local scope that closes before heap
//! init, and the borrow checker keeps every derived [`Fdt`] inside it.

#![cfg_attr(not(test), no_std)]

pub use fdt::Fdt;

/// Where QEMU's `virt` machine leaves the DTB for a flat kernel image.
///
/// QEMU does not set `x0` for flat kernels, so when the boot pointer is zero
/// this is the only candidate. It is `ALIGN_UP(kernel_load + image_size, 2 MiB)`
/// with the kernel at `0x4010_0000` (`text_offset` = 1 MiB).
pub const QEMU_VIRT_DTB_PA: usize = 0x4020_0000;

/// `0xd00dfeed`, the FDT magic, as it appears in the blob (big-endian).
const FDT_MAGIC: u32 = 0xd00d_feed;

/// Smallest `totalsize` worth believing: the FDT header alone is 40 bytes.
pub const MIN_TOTALSIZE: u32 = 64;

/// Largest `totalsize` worth believing. Real trees here are a few KiB; the cap
/// exists so a wild pointer cannot turn into a huge slice, not to bound any
/// machine we expect to meet.
pub const MAX_TOTALSIZE: u32 = 16 * 1024 * 1024;

/// A validated flattened device tree blob, borrowed for `'a`.
#[derive(Clone, Copy, Debug)]
pub struct Dtb<'a> {
    base: usize,
    blob: &'a [u8],
}

impl<'a> Dtb<'a> {
    /// Wrap a blob already in memory, validating its header.
    ///
    /// Safe, and the reason the host tests below can exercise everything
    /// [`locate`] does except the pointer materialisation itself.
    #[must_use]
    pub fn from_slice(blob: &'a [u8]) -> Option<Self> {
        let head: &[u8; 8] = blob.get(..8)?.try_into().ok()?;
        let total = header_totalsize(head)? as usize;
        let blob = blob.get(..total)?;
        Some(Self { base: blob.as_ptr() as usize, blob })
    }

    /// Address the blob was found at. Diagnostics only.
    #[must_use]
    pub fn base(&self) -> usize {
        self.base
    }

    /// The blob's declared length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blob.len()
    }

    /// Never true — a `Dtb` only exists once its header validated — but clippy
    /// asks for it next to `len`, and a caller reading `if dtb.is_empty()` is
    /// better served by a straight answer than by its absence.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blob.is_empty()
    }

    /// The validated bytes.
    #[must_use]
    pub fn blob(&self) -> &'a [u8] {
        self.blob
    }

    /// Parse the tree, and refuse one this kernel cannot use.
    ///
    /// Safe: the blob is a slice, and `Fdt::new` re-checks the header itself.
    ///
    /// # Why the required-node check is here and not at each consumer
    ///
    /// `Fdt::new` validates the magic and that the buffer is at least
    /// `totalsize` — and nothing else. A blob whose header is right and whose
    /// body is zeroes parses "successfully". That matters because two of the
    /// `fdt` crate's accessors **panic** rather than return an option:
    ///
    /// ```text
    /// pub fn memory(&self) -> Memory { self.find_node("/memory").expect("requires memory node") }
    /// pub fn cpus(&self)   -> ...    { self.find_node("/cpus").expect("/cpus is a required node") }
    /// ```
    ///
    /// This kernel calls both — `main::detect_memory` and
    /// `smp_shared::probe_dtb` — and builds `panic = "abort"`, so a tree missing
    /// either node is a dead kernel at boot rather than a fallback to defaults.
    /// It is reachable only through the scan path, where a stale blob may sit at
    /// [`QEMU_VIRT_DTB_PA`] and four bytes of magic are the whole filter; but the
    /// cost of ruling it out is one lookup per boot, and the consumers' fallback
    /// paths ("using default 256MB", "staying single-core") already exist and are
    /// correct.
    ///
    /// Requiring **both** nodes couples two questions that are in principle
    /// separate: a machine with `/memory` but no `/cpus` loses RAM detection too.
    /// No such machine exists here — this kernel needs CPUs — and the alternative
    /// is three scattered guards that a fourth consumer would forget. The failure
    /// is a printed fallback, not an abort, which is the point.
    #[must_use]
    pub fn parse(&self) -> Option<Fdt<'a>> {
        let fdt = Fdt::new(self.blob).ok()?;
        // Both, before either panicking accessor can be reached.
        fdt.find_node("/memory")?;
        fdt.find_node("/cpus")?;
        Some(fdt)
    }
}

/// Validate an FDT header's first 8 bytes and return its declared total size.
///
/// The blob is big-endian regardless of the CPU, and this is the only place that
/// knows it.
#[must_use]
pub fn header_totalsize(head: &[u8; 8]) -> Option<u32> {
    let magic = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if magic != FDT_MAGIC {
        return None;
    }
    let total = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
    (MIN_TOTALSIZE..=MAX_TOTALSIZE).contains(&total).then_some(total)
}

/// Where [`locate`] will look: the boot pointer if the bootloader gave one,
/// otherwise [`QEMU_VIRT_DTB_PA`].
///
/// Exposed so a caller can name the address it probed when the probe comes back
/// empty — reporting the scan constant after the bootloader supplied a pointer
/// sends the reader to the wrong address. The resolution rule lives here rather
/// than at the call site so the message and the probe cannot disagree.
#[must_use]
pub const fn resolve(pa: usize) -> usize {
    if pa != 0 { pa } else { QEMU_VIRT_DTB_PA }
}

/// Find and validate the device tree at [`resolve(pa)`](resolve).
///
/// Returns `None` when nothing at the resolved address looks like an FDT — which
/// every caller must treat as "no device tree", not as an error, because the
/// kernel boots on machines that supply none.
///
/// # Safety
///
/// - `pa` is zero, or names memory the caller has already mapped. On this kernel
///   that means calling `mmu::ensure_boot_identity_covers` first: Firecracker
///   puts the blob in the last 2 MiB of guest RAM, outside `boot.rs`'s static
///   identity map.
/// - Whichever address is resolved, at least 8 bytes there are readable. This
///   function reads no further until those 8 bytes have identified a blob.
/// - The memory must stay valid, and must not be written by anyone else, for the
///   whole of `'a`. Choose `'a` to end before the heap can be placed over the
///   blob — see the module docs.
#[must_use]
pub unsafe fn locate<'a>(pa: usize) -> Option<Dtb<'a>> {
    let base = resolve(pa);

    // Byte at a time, so an unaligned boot pointer is not undefined behaviour.
    // Eight loads, once per boot.
    let mut head = [0u8; 8];
    for (i, b) in head.iter_mut().enumerate() {
        // SAFETY: the caller vouches that 8 bytes at `base` are mapped and
        // readable. Volatile because this is a speculative probe of an address
        // the compiler knows nothing about.
        *b = unsafe { core::ptr::read_volatile((base + i) as *const u8) };
    }
    let total = header_totalsize(&head)? as usize;

    // SAFETY: the header validated, so the caller's guarantee extends to the
    // `total` bytes it declares, for `'a`.
    let blob = unsafe { core::slice::from_raw_parts(base as *const u8, total) };
    Some(Dtb { base, blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed header prefix: magic, then `totalsize`.
    fn head(magic: u32, total: u32) -> [u8; 8] {
        let mut h = [0u8; 8];
        h[..4].copy_from_slice(&magic.to_be_bytes());
        h[4..].copy_from_slice(&total.to_be_bytes());
        h
    }

    #[test]
    fn accepts_a_plausible_header() {
        assert_eq!(header_totalsize(&head(FDT_MAGIC, 4096)), Some(4096));
    }

    /// The magic is big-endian in the blob. The three call sites this crate
    /// replaced spelled that as a pre-swapped little-endian constant compared
    /// against a native `u32` load, which is correct only on a little-endian
    /// CPU and states nothing about the format.
    #[test]
    fn rejects_byte_swapped_magic() {
        assert_eq!(header_totalsize(&head(FDT_MAGIC.swap_bytes(), 4096)), None);
    }

    #[test]
    fn rejects_uninitialised_memory() {
        assert_eq!(header_totalsize(&[0; 8]), None);
        assert_eq!(header_totalsize(&[0xff; 8]), None);
    }

    /// `Fdt::from_ptr` trusts `totalsize` unconditionally, so a wild pointer
    /// whose first word happens to be the magic yields a slice of whatever the
    /// next word says — up to 4 GiB.
    #[test]
    fn rejects_absurd_totalsize() {
        assert_eq!(header_totalsize(&head(FDT_MAGIC, 0)), None);
        assert_eq!(header_totalsize(&head(FDT_MAGIC, MIN_TOTALSIZE - 1)), None);
        assert_eq!(header_totalsize(&head(FDT_MAGIC, u32::MAX)), None);
        assert_eq!(header_totalsize(&head(FDT_MAGIC, MAX_TOTALSIZE + 1)), None);
    }

    #[test]
    fn accepts_the_exact_bounds() {
        assert_eq!(header_totalsize(&head(FDT_MAGIC, MIN_TOTALSIZE)), Some(MIN_TOTALSIZE));
        assert_eq!(header_totalsize(&head(FDT_MAGIC, MAX_TOTALSIZE)), Some(MAX_TOTALSIZE));
    }

    #[test]
    fn from_slice_trims_to_the_declared_size() {
        let mut blob = vec![0u8; 4096];
        blob[..8].copy_from_slice(&head(FDT_MAGIC, 128));
        let dtb = Dtb::from_slice(&blob).expect("header is valid");
        assert_eq!(dtb.len(), 128);
        assert!(!dtb.is_empty());
    }

    /// A truncated blob is rejected rather than producing a `Dtb` that reads
    /// past its own storage.
    #[test]
    fn from_slice_rejects_a_blob_shorter_than_it_claims() {
        let mut blob = vec![0u8; 64];
        blob[..8].copy_from_slice(&head(FDT_MAGIC, 4096));
        assert!(Dtb::from_slice(&blob).is_none());
    }

    #[test]
    fn from_slice_rejects_a_stub_too_short_to_hold_a_header() {
        assert!(Dtb::from_slice(&[]).is_none());
        assert!(Dtb::from_slice(&[0xd0, 0x0d, 0xfe, 0xed]).is_none());
    }

    /// A valid header with a zeroed body: `Fdt::new` accepts it (it checks the
    /// magic and the length, and nothing else), and then `fdt.memory()` and
    /// `fdt.cpus()` would **panic** on the missing nodes. `parse` is what stops
    /// that reaching a `panic = "abort"` kernel.
    #[test]
    fn parse_declines_a_header_with_no_tree_behind_it() {
        let mut blob = vec![0u8; 256];
        blob[..8].copy_from_slice(&head(FDT_MAGIC, 256));
        let dtb = Dtb::from_slice(&blob).expect("header is valid");
        assert!(Fdt::new(dtb.blob()).is_ok(), "the fdt crate accepts this; that is the hazard");
        assert!(dtb.parse().is_none(), "and this is what stops it");
    }

    // Real trees from both machines this kernel boots on. The Firecracker
    // fixtures are the same `.dtb` files `akuma-firecracker`'s tests pin; the
    // QEMU ones are its `fixtures/`. They are here to prove the required-node
    // check in `parse` rejects nothing it must accept — a validator that only
    // ever gets tested against garbage is a validator that can be too strict.
    const FC_VCPU1: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu1.dtb");
    const FC_VCPU8: &[u8] = include_bytes!("../../../docs/reference/firecracker/fdt/fdt-vcpu8.dtb");
    const QEMU_SMP1: &[u8] = include_bytes!("../../akuma-firecracker/fixtures/qemu-virt-smp1.dtb");
    const QEMU_SMP4: &[u8] = include_bytes!("../../akuma-firecracker/fixtures/qemu-virt-smp4.dtb");

    #[test]
    fn accepts_every_real_tree() {
        for (name, blob) in [
            ("firecracker vcpu1", FC_VCPU1),
            ("firecracker vcpu8", FC_VCPU8),
            ("qemu virt smp1", QEMU_SMP1),
            ("qemu virt smp4", QEMU_SMP4),
        ] {
            let dtb = Dtb::from_slice(blob).unwrap_or_else(|| panic!("{name}: header rejected"));
            let fdt = dtb.parse().unwrap_or_else(|| panic!("{name}: tree rejected"));
            // The two accessors whose `expect` the check in `parse` exists to
            // make unreachable.
            assert!(fdt.memory().regions().next().is_some(), "{name}: no memory region");
            assert!(fdt.cpus().next().is_some(), "{name}: no cpus");
        }
    }

    /// `from_slice` trims to the declared size, so a `Dtb` built from a fixture
    /// that has trailing padding still parses. Guards against a future change
    /// that trims with an off-by-one.
    #[test]
    fn trailing_bytes_do_not_break_a_real_tree() {
        let mut padded = QEMU_SMP1.to_vec();
        padded.extend_from_slice(&[0xAA; 4096]);
        let dtb = Dtb::from_slice(&padded).expect("header is valid");
        assert_eq!(dtb.len(), QEMU_SMP1.len(), "trimmed to the declared totalsize");
        assert!(dtb.parse().is_some());
    }
}
