//! Address-space layout randomization: one switch and one source of slides.
//!
//! The loader (`akuma-elf`) and the mapping cursor (`akuma-exec`'s
//! `ProcessMemory`) each add a random, aligned offset to an address they used
//! to place at a constant: the static-PIE base, the dynamic linker's base, the
//! top of the stack and the start of the `mmap` area. Without it every process
//! has the same layout on every boot, so one leaked or guessed address is every
//! address (`docs/archive/SECURITY_AUDIT_2026_11_16.md` H3).
//!
//! # Off until the kernel says otherwise
//!
//! [`slide`] answers `0` until [`set_enabled`] is called with `true`. A kernel turns it on
//! immediately before it starts init, and leaves it off for `aslr=0` on the
//! boot line. That ordering is deliberate: the boot self-tests assert exact
//! addresses and run first, and `aslr=0` is what makes a crash address
//! comparable between two boots when hunting a bug.
//!
//! # What a slide is worth
//!
//! The values come from the kernel entropy pool ([`crate::rng`]). A machine
//! with no entropy source gets `0`: no randomization, rather than a guessable
//! one presented as random. The ranges are small where the AArch64 layout is
//! tight (its kernel occupies the user half from 1 GiB up), so this raises the
//! cost of an attack by a factor of thousands, not billions.

use core::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn randomization on or off for every image loaded from now on.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
}

/// Whether [`slide`] can answer anything but `0`.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// A random multiple of `align` in `[0, span)`, or `0` when randomization is
/// off, the pool has no entropy, or `span` holds less than one step.
///
/// `align` must be a power of two.
#[must_use]
pub fn slide(span: usize, align: usize) -> usize {
    if !enabled() {
        return 0;
    }
    let mut bytes = [0u8; 8];
    if crate::rng::fill_bytes(&mut bytes) != Some(true) {
        return 0;
    }
    pick(u64::from_le_bytes(bytes), span, align)
}

/// The pure half of [`slide`]: map `random` onto the steps of `align` in `[0, span)`.
#[must_use]
pub const fn pick(random: u64, span: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    let steps = span / align;
    if steps == 0 {
        return 0;
    }
    (random % steps as u64) as usize * align
}

#[cfg(test)]
mod tests {
    use super::pick;

    /// Every result is aligned and strictly inside the span, including the
    /// extremes of the random word.
    #[test]
    fn pick_is_aligned_and_in_range() {
        for random in [0, 1, 12_345, u64::MAX - 1, u64::MAX] {
            for (span, align) in [(64 << 20, 0x1_0000), (16 << 20, 0x1000), (0x1_0000, 0x1_0000), (4096, 16)] {
                let s = pick(random, span, align);
                assert!(s < span, "{s:#x} outside {span:#x}");
                assert_eq!(s % align, 0);
            }
        }
    }

    /// A span smaller than one step, or empty, slides nothing.
    #[test]
    fn pick_of_a_short_span_is_zero() {
        assert_eq!(pick(u64::MAX, 0, 0x1000), 0);
        assert_eq!(pick(u64::MAX, 0xFFF, 0x1000), 0);
    }

    /// Different random words reach different steps (it is not a constant).
    #[test]
    fn pick_varies_with_the_random_word() {
        assert_ne!(pick(1, 1 << 20, 0x1000), pick(2, 1 << 20, 0x1000));
    }

    /// Off by default: nothing slides until a kernel opts in.
    #[test]
    fn slide_is_zero_while_disabled() {
        assert!(!super::enabled());
        assert_eq!(super::slide(1 << 30, 0x1000), 0);
    }
}
