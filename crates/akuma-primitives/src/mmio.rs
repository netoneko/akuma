//! Typed access to memory-mapped device registers.
//!
//! # Why this exists
//!
//! Device drivers here spelled every register access as a raw
//! `read_volatile((base + OFFSET) as *const u32)`, which put an `unsafe` block on
//! *each access* — 30 of them in `akuma-virtio`'s rng driver alone, all vouching
//! for the same fact: that `base` is a mapped device window. That is `unsafe`
//! marking the wrong thing. The property that needs a human's word is **"this
//! address is a device register"**, and it is true once per device, not once per
//! read.
//!
//! [`MmioReg`] moves the obligation to construction. After a caller has vouched
//! for an address, reads and writes of that register are safe operations, because
//! a volatile access to a real, mapped register cannot violate memory safety on
//! its own.
//!
//! See `docs/archive/TRIM_FAT_MMIO_NEWTYPE.md` for the conversion plan and the
//! sites deliberately left raw.
//!
//! # What this is not
//!
//! Not an abstraction over device protocol. It carries no ordering, no
//! endianness, and no named registers: fences stay at the call site where the
//! driver put them, `to_be()` stays at the call site, and a read whose only
//! purpose is the protocol step still has to be written as a read. Converting a
//! driver to `MmioReg` must not change a single instruction it emits.

/// A memory-mapped device register of width `T`.
///
/// `unsafe` lives at construction: the caller vouches that the address is a
/// device register of exactly this width, mapped (Device-nGnRnE) for the
/// kernel's lifetime. Afterwards [`read`](Self::read) and [`write`](Self::write)
/// are safe.
///
/// The raw-pointer field makes this `!Send`/`!Sync` by default, which is
/// deliberate — a driver that wants to park its registers in a `static` has to
/// write the `unsafe impl` itself and say what serialises the accesses.
#[derive(Clone, Copy, Debug)]
pub struct MmioReg<T>(*mut T);

impl<T: Copy> MmioReg<T> {
    /// Name the register at `addr`.
    ///
    /// `const` so drivers with fixed register addresses (the PL011 console) can
    /// build theirs in a `const` item and stay free of any init-order dependency.
    ///
    /// # Safety
    /// `addr` is a device register of width `T`, correctly aligned, and mapped
    /// as device memory for the kernel's lifetime.
    #[inline]
    #[must_use]
    pub const unsafe fn new(addr: usize) -> Self {
        Self(addr as *mut T)
    }

    /// Volatile read of the register.
    #[inline]
    #[must_use]
    pub fn read(&self) -> T {
        // SAFETY: the constructor's caller vouched that this is a mapped device
        // register of width `T`.
        unsafe { self.0.read_volatile() }
    }

    /// Volatile write of the register.
    #[inline]
    pub fn write(&self, value: T) {
        // SAFETY: as `read` — the address was vouched for at construction.
        unsafe { self.0.write_volatile(value) };
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// The register widths an [`MmioWindow`] accesses: `u8`, `u16`, `u32`, `u64`.
///
/// Sealed. A register is an integer of its own width, and keeping the set closed
/// is what makes "naturally aligned" mean `offset % size_of::<T>() == 0`.
pub trait MmioWidth: Copy + sealed::Sealed {}
impl MmioWidth for u8 {}
impl MmioWidth for u16 {}
impl MmioWidth for u32 {}
impl MmioWidth for u64 {}

/// A mapped device window: `len` bytes of registers starting at `base`.
///
/// [`MmioReg`] vouches for *one* address; most drivers name a BAR and then index
/// into it by offset (`xHCI`'s capability/operational/runtime/doorbell blocks,
/// HDA's stream descriptors). Doing that with `MmioReg::new(base + off)` puts an
/// `unsafe` block on every access again, each vouching that `off` is inside the
/// window — a fact this type checks instead of asserting.
///
/// The one `unsafe` is [`MmioWindow::new`]: "this range is a device mapping".
/// After it, [`read`](Self::read) and [`write`](Self::write) are safe because
/// they refuse (panic, like a slice index) an access that is out of bounds or not
/// naturally aligned; [`try_read`](Self::try_read) returns `None` instead, for
/// offsets the *device* supplied (an xHCI extended-capability pointer).
///
/// Like [`MmioReg`], it carries no ordering, endianness or register names. It is
/// plain data (an address and a length), so it is `Send + Sync`; what serialises
/// the accesses made through it is the driver's lock, as before.
#[derive(Clone, Copy, Debug)]
pub struct MmioWindow {
    base: usize,
    len: usize,
}

impl MmioWindow {
    /// Name the `len` bytes at `base` as a device window.
    ///
    /// # Safety
    /// `base..base + len` is device registers, mapped as device memory for the
    /// kernel's lifetime, and `base` is aligned to the widest access made through
    /// the window (a page-aligned BAR always is).
    #[inline]
    #[must_use]
    pub const unsafe fn new(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    /// Address of the first register.
    #[inline]
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Size of the window in bytes.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` for a zero-length window.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The sub-window `[off, off + len)`, or `None` if it does not fit.
    ///
    /// Safe: a sub-range of a vouched window is vouched.
    #[must_use]
    pub const fn sub(&self, off: usize, len: usize) -> Option<Self> {
        match off.checked_add(len) {
            Some(end) if end <= self.len => Some(Self { base: self.base + off, len }),
            _ => None,
        }
    }

    /// The sub-window from `off` to the end of this one.
    #[must_use]
    pub const fn sub_from(&self, off: usize) -> Option<Self> {
        if off > self.len {
            return None;
        }
        Some(Self { base: self.base + off, len: self.len - off })
    }

    fn reg<T: MmioWidth>(&self, off: usize) -> Option<MmioReg<T>> {
        let size = core::mem::size_of::<T>();
        let end = off.checked_add(size)?;
        if end > self.len || !(self.base + off).is_multiple_of(size) {
            return None;
        }
        // SAFETY: `off + size <= len` and the address is `size`-aligned, so this
        // is one register of width `T` inside the window `new`'s caller vouched
        // is mapped device memory.
        Some(unsafe { MmioReg::<T>::new(self.base + off) })
    }

    /// Volatile read of the `T`-wide register at `off`; `None` if it is outside
    /// the window or misaligned.
    #[inline]
    #[must_use]
    pub fn try_read<T: MmioWidth>(&self, off: usize) -> Option<T> {
        self.reg::<T>(off).map(|r| r.read())
    }

    /// Volatile write; `false` (nothing written) if outside the window or
    /// misaligned.
    #[inline]
    pub fn try_write<T: MmioWidth>(&self, off: usize, value: T) -> bool {
        match self.reg::<T>(off) {
            Some(r) => {
                r.write(value);
                true
            }
            None => false,
        }
    }

    /// Store `value` to `count` consecutive dwords starting at `off`; `false`
    /// (nothing written) if the span is outside the window or `off` is not
    /// dword-aligned.
    ///
    /// For a framebuffer fill. On x86_64 bare metal this is one `rep stosd`, which
    /// is what lets a write-combining mapping gather the span into full-line
    /// bursts (a loop of single volatile stores does not, measurably); everywhere
    /// else it is that loop. The direction flag is clear at every call (SysV ABI,
    /// and every kernel entry stub `cld`s), so `stosd` ascends.
    #[inline]
    #[must_use]
    pub fn fill32(&self, off: usize, count: usize, value: u32) -> bool {
        let Some(bytes) = count.checked_mul(4) else { return false };
        let Some(end) = off.checked_add(bytes) else { return false };
        if end > self.len || !(self.base + off).is_multiple_of(4) {
            return false;
        }
        #[cfg(all(target_os = "none", target_arch = "x86_64"))]
        // SAFETY: `off + 4 * count <= len` and the address is dword-aligned, so
        // the `count` stores land inside the window `new`'s caller vouched is
        // mapped device memory; `rep stosd` stores `count` dwords of `eax` at `rdi`.
        unsafe {
            core::arch::asm!(
                "rep stosd",
                inout("rdi") self.base + off => _,
                inout("rcx") count => _,
                in("eax") value,
                options(nostack, preserves_flags),
            );
        }
        #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
        for i in 0..count {
            self.write::<u32>(off + i * 4, value);
        }
        true
    }

    /// Volatile read of the `T`-wide register at `off`.
    ///
    /// # Panics
    /// If the access is outside the window or not naturally aligned — a driver
    /// bug, refused exactly like an out-of-range slice index.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn read<T: MmioWidth>(&self, off: usize) -> T {
        self.try_read(off).expect("MMIO read outside its window or misaligned")
    }

    /// Volatile write of the `T`-wide register at `off`.
    ///
    /// # Panics
    /// As [`read`](Self::read).
    #[inline]
    #[track_caller]
    pub fn write<T: MmioWidth>(&self, off: usize, value: T) {
        assert!(self.try_write(off, value), "MMIO write outside its window or misaligned");
    }
}

#[cfg(test)]
mod tests {
    use super::{MmioReg, MmioWindow};

    /// There is no device on the host, but volatile access to ordinary memory is
    /// well-defined, so the plumbing — width, address arithmetic, round-trip — is
    /// testable even though device semantics are not.
    #[test]
    fn round_trips_through_the_named_address() {
        let mut cell: u32 = 0;
        // SAFETY: not a device register, but a valid, aligned, live `u32` for the
        // duration of the test, which is what the accessors actually require.
        let reg = unsafe { MmioReg::<u32>::new(&raw mut cell as usize) };

        assert_eq!(reg.read(), 0);
        reg.write(0x7472_6976);
        assert_eq!(reg.read(), 0x7472_6976);
        assert_eq!(cell, 0x7472_6976, "the write must land at the named address");
    }

    /// The width is part of the register's identity — the now-removed `fw_cfg`
    /// driver alone needed u8, u16 and u64 registers, which is why the type is
    /// generic rather than u32-only.
    #[test]
    fn each_width_accesses_exactly_its_own_bytes() {
        let mut cell: u64 = 0;
        let base = &raw mut cell as usize;

        // SAFETY: `cell` is a live, 8-byte-aligned `u64`; a `u8` at its base and a
        // `u16` at its base are both in bounds and aligned.
        let (byte, half, word) = unsafe {
            (
                MmioReg::<u8>::new(base),
                MmioReg::<u16>::new(base),
                MmioReg::<u64>::new(base),
            )
        };

        // Which byte of the u64 a narrow write lands in is the host's endianness,
        // so count touched bytes rather than assume a position.
        word.write(0);
        byte.write(0xAB);
        assert_eq!(byte.read(), 0xAB);
        assert_eq!(
            word.read().to_ne_bytes().iter().filter(|&&b| b != 0).count(),
            1,
            "a u8 write must touch exactly one byte of the u64"
        );

        word.write(0);
        half.write(0xBEEF);
        assert_eq!(half.read(), 0xBEEF);
        assert_eq!(
            word.read().to_ne_bytes().iter().filter(|&&b| b != 0).count(),
            2,
            "a u16 write must touch exactly two bytes of the u64"
        );
    }

    #[repr(align(8))]
    struct Page([u8; 32]);

    fn window(p: &mut Page) -> MmioWindow {
        // SAFETY: not device memory, but live, 8-aligned and 32 bytes long for
        // the test, which is what the accessors require.
        unsafe { MmioWindow::new(p.0.as_mut_ptr() as usize, 32) }
    }

    #[test]
    fn window_round_trips_each_width() {
        let mut p = Page([0; 32]);
        let w = window(&mut p);
        w.write::<u32>(4, 0xAABB_CCDD);
        assert_eq!(w.read::<u32>(4), 0xAABB_CCDD);
        w.write::<u64>(8, 0x0102_0304_0506_0708);
        assert_eq!(w.read::<u64>(8), 0x0102_0304_0506_0708);
        w.write::<u16>(2, 0xBEEF);
        assert_eq!(w.read::<u16>(2), 0xBEEF);
        w.write::<u8>(31, 7);
        assert_eq!(w.read::<u8>(31), 7);
    }

    #[test]
    fn fill32_stores_exactly_the_span_and_refuses_the_rest() {
        let mut p = Page([0; 32]);
        let w = window(&mut p);
        assert!(w.fill32(8, 3, 0xA5A5_A5A5));
        for off in (0..32).step_by(4) {
            let expect = if (8..20).contains(&off) { 0xA5A5_A5A5 } else { 0 };
            assert_eq!(w.read::<u32>(off), expect, "dword at {off}");
        }
        assert!(w.fill32(0, 0, 1), "an empty span is in bounds");
        assert!(!w.fill32(28, 2, 1), "runs off the end");
        assert!(!w.fill32(2, 1, 1), "misaligned");
        assert!(!w.fill32(0, usize::MAX, 1), "count overflow");
        assert_eq!(w.read::<u32>(28), 0, "a refused fill writes nothing");
    }

    #[test]
    fn window_refuses_out_of_bounds_and_misaligned() {
        let mut p = Page([0; 32]);
        let w = window(&mut p);
        assert_eq!(w.try_read::<u32>(32), None);
        assert_eq!(w.try_read::<u32>(30), None, "straddles the end");
        assert_eq!(w.try_read::<u64>(28), None);
        assert_eq!(w.try_read::<u32>(2), None, "misaligned");
        assert_eq!(w.try_read::<u32>(usize::MAX), None, "offset overflow");
        assert!(!w.try_write::<u16>(1, 1));
        assert!(w.try_read::<u32>(28).is_some(), "the last dword is in bounds");
    }

    #[test]
    #[should_panic(expected = "outside its window")]
    fn window_read_panics_like_an_index() {
        let mut p = Page([0; 32]);
        let _ = window(&mut p).read::<u32>(32);
    }

    #[test]
    fn sub_windows_stay_inside_the_parent() {
        let mut p = Page([0; 32]);
        let w = window(&mut p);
        let s = w.sub(8, 16).unwrap();
        assert_eq!((s.base(), s.len()), (w.base() + 8, 16));
        assert!(w.sub(24, 16).is_none());
        assert!(w.sub(usize::MAX, 2).is_none());
        s.write::<u32>(0, 0x55);
        assert_eq!(w.read::<u32>(8), 0x55, "a sub-window addresses the parent's bytes");
        assert!(s.try_read::<u32>(16).is_none());
        assert_eq!(w.sub_from(32).unwrap().len(), 0);
        assert!(w.sub_from(33).is_none());
    }
}
