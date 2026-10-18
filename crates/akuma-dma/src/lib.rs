#![cfg_attr(not(test), no_std)]
//! Statically allocated DMA memory, and the only place its `unsafe` lives.
//!
//! # What this is
//!
//! A driver that gives a device a buffer — an xHCI ring, an HDA ring, a virtio
//! frame — keeps it in a `static mut` and used to reach it with
//! `from_raw_parts_mut((&raw mut BUF).cast(), N)` at a dozen sites, each one
//! vouching for the same two facts: *this is a live static of N elements* and
//! *nothing else holds it right now*. Neither is a property of the line.
//!
//! * [`DmaArray`] states the first by construction: it **is** the static, so a
//!   length and an alignment are the type's, and an index is bounds-checked.
//! * It enforces the second at run time with a borrow word, the way
//!   [`frames::FrameArena`] always has for the network frames: a second
//!   overlapping `&mut` is a panic naming the buffer's bug, not a silent alias.
//!
//! # What this does not do
//!
//! It does not model **device** ownership. While a device has an address you
//! gave it, it may write the memory at any moment, and no Rust borrow expresses
//! that. Two consequences, both deliberate:
//!
//! * Cells the *device* writes (a completion ring, a position buffer, an input
//!   report) are read with [`DmaArray::get`], a volatile read of one cell, never
//!   through a [`DmaArray::view`] slice the optimizer may keep in a register.
//! * It does not know what an address means to the device. [`DmaArray::addr`]
//!   hands out the **virtual** address; translating it is the caller's, because
//!   the translation (`virt_to_phys`) belongs to a layer this leaf must not
//!   depend on.
//!
//! # The element types
//!
//! Only integers and `[u32; 4]` (a TRB): types where **every bit pattern is a
//! valid value**, which is what makes a slice over memory a device is writing
//! sound to form at all. The set is sealed.

pub mod frames;

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
    impl Sealed for [u32; 4] {}
}

/// A cell type a [`DmaArray`] may hold: plain integers, any bit pattern valid.
pub trait DmaElem: Copy + sealed::Sealed {
    /// The all-zero value a static starts as, so the array lands in BSS.
    const ZERO: Self;
}
impl DmaElem for u8 {
    const ZERO: Self = 0;
}
impl DmaElem for u16 {
    const ZERO: Self = 0;
}
impl DmaElem for u32 {
    const ZERO: Self = 0;
}
impl DmaElem for u64 {
    const ZERO: Self = 0;
}
impl DmaElem for [u32; 4] {
    const ZERO: Self = [0; 4];
}

/// Alignment markers for [`DmaArray`]'s third parameter.
///
/// A zero-sized type with `repr(align(N))`, used as `[A; 0]` in front of the
/// data, is the stable way to give a generic struct a chosen alignment.
pub mod align {
    /// 64 bytes — one cache line; TRBs, xHCI contexts, scratch tables.
    #[repr(align(64))]
    #[derive(Clone, Copy)]
    pub struct A64;
    /// 128 bytes — HDA's CORB/RIRB/BDL/position buffer.
    #[repr(align(128))]
    #[derive(Clone, Copy)]
    pub struct A128;
    /// 256 bytes — Realtek RTL8169 descriptor rings.
    #[repr(align(256))]
    #[derive(Clone, Copy)]
    pub struct A256;
    /// 4 KiB — a page; rings the device addresses by page, and bounce buffers.
    #[repr(align(4096))]
    #[derive(Clone, Copy)]
    pub struct A4K;
}

#[repr(C)]
struct Storage<A, E, const N: usize>([A; 0], [E; N]);

/// Borrow word: `0` free, `WRITER` exclusively held, anything else a reader count.
const WRITER: u32 = u32::MAX;

/// `N` cells of `E`, aligned to `A`, living in a `static`.
///
/// All-zero, so it sits in BSS. `Sync` because every access goes through the
/// borrow word.
pub struct DmaArray<E: DmaElem, const N: usize, A> {
    cells: UnsafeCell<Storage<A, E, N>>,
    state: AtomicU32,
}

// SAFETY: the storage is reached only through `get`/`set` (volatile, one cell,
// admitted only when no conflicting guard is live) and `view`/`view_mut`
// (guards that hold the borrow word for their lifetime). The word's CAS makes
// "at most one writer, or any number of readers" hold across cores. `E` has no
// invalid bit patterns, so a concurrent *device* write cannot produce one.
unsafe impl<E: DmaElem, const N: usize, A> Sync for DmaArray<E, N, A> {}

impl<E: DmaElem, const N: usize, A> DmaArray<E, N, A> {
    /// A zeroed array. `const`, for `static`.
    #[must_use]
    pub const fn new() -> Self {
        assert!(N > 0, "a DmaArray must hold at least one cell");
        Self {
            cells: UnsafeCell::new(Storage([], [E::ZERO; N])),
            state: AtomicU32::new(0),
        }
    }

    /// Cells in the array.
    #[must_use]
    pub const fn len(&self) -> usize {
        N
    }

    /// Always `false`; `new` rejects `N == 0`. Here for the `len`/`is_empty` pair.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Size in bytes.
    #[must_use]
    pub const fn size_bytes(&self) -> usize {
        N * core::mem::size_of::<E>()
    }

    /// **Virtual** address of the first cell. The caller translates it for the
    /// device; see the crate docs.
    #[must_use]
    pub fn addr(&self) -> usize {
        self.cells.get() as usize
    }

    /// Virtual address of cell `i` (for a device that is handed a sub-range:
    /// a ring's dequeue pointer, a scratchpad page). Bounds-checked, `i == N`
    /// allowed (one past the end).
    #[must_use]
    #[track_caller]
    pub fn addr_of(&self, i: usize) -> usize {
        assert!(i <= N, "DMA cell index out of range");
        self.addr() + i * core::mem::size_of::<E>()
    }

    fn cell_ptr(&self, i: usize) -> *mut E {
        assert!(i < N, "DMA cell index out of range");
        // SAFETY: `i < N`, so the offset stays inside the array.
        unsafe { self.cells.get().cast::<E>().add(i) }
    }

    fn acquire_read(&self) -> bool {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            if s >= WRITER - 1 {
                return false; // writer held, or the reader count would saturate
            }
            match self.state.compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => s = now,
            }
        }
    }

    fn release_read(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }

    fn acquire_write(&self) -> bool {
        self.state.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    fn release_write(&self) {
        self.state.store(0, Ordering::Release);
    }

    /// Volatile read of cell `i`: the way to read what a **device** wrote.
    ///
    /// # Panics
    /// If `i >= N`, or a [`view_mut`](Self::view_mut) guard is live.
    #[must_use]
    #[track_caller]
    pub fn get(&self, i: usize) -> E {
        assert!(self.acquire_read(), "DMA buffer read while mutably borrowed");
        // SAFETY: `cell_ptr` is in bounds and aligned; the read lock excludes a
        // live `&mut` slice; a device writing concurrently is allowed for `E`.
        let v = unsafe { self.cell_ptr(i).read_volatile() };
        self.release_read();
        v
    }

    /// Volatile write of cell `i`.
    ///
    /// # Panics
    /// If `i >= N`, or any guard on this array is live.
    #[track_caller]
    pub fn set(&self, i: usize, v: E) {
        assert!(self.acquire_write(), "DMA buffer written while borrowed");
        // SAFETY: as `get`, with the write lock held.
        unsafe { self.cell_ptr(i).write_volatile(v) };
        self.release_write();
    }

    /// Shared view of the whole array, for CPU-side reads of memory the device is
    /// **not** concurrently writing (a command the CPU built, a buffer the device
    /// finished). Use [`get`](Self::get) for device-written cells.
    ///
    /// # Panics
    /// If a [`view_mut`](Self::view_mut) guard is live.
    #[track_caller]
    pub fn view(&self) -> DmaRef<'_, E, N, A> {
        assert!(self.acquire_read(), "DMA buffer read while mutably borrowed");
        DmaRef { arr: self }
    }

    /// Exclusive view of the whole array.
    ///
    /// # Panics
    /// If any guard on this array is live.
    #[track_caller]
    pub fn view_mut(&self) -> DmaMut<'_, E, N, A> {
        assert!(self.acquire_write(), "DMA buffer mutably borrowed while already borrowed");
        DmaMut { arr: self }
    }

    /// Run `f` over cells `[off, off + len)` as a shared slice, holding the read
    /// borrow for the call.
    ///
    /// For a buffer the device is **still writing elsewhere**: a receive ring whose
    /// entry the CPU may read once the device has moved past it while later entries
    /// fill. [`view`](Self::view) would hand out `&[E]` over the whole array,
    /// including cells the device is writing right now; this makes the reference
    /// cover only the range the caller has established the device is done with.
    ///
    /// # Panics
    /// If the range is outside the array, or a [`view_mut`](Self::view_mut) guard
    /// (or a [`leak_mut`](Self::leak_mut)) is live.
    #[track_caller]
    pub fn read_range<R>(&self, off: usize, len: usize, f: impl FnOnce(&[E]) -> R) -> R {
        assert!(off.checked_add(len).is_some_and(|e| e <= N), "DMA range outside the array");
        assert!(self.acquire_read(), "DMA buffer read while mutably borrowed");
        // Released on unwind too, so a panicking `f` cannot leave the array
        // read-locked.
        struct Release<'a, E: DmaElem, const N: usize, A>(&'a DmaArray<E, N, A>);
        impl<E: DmaElem, const N: usize, A> Drop for Release<'_, E, N, A> {
            fn drop(&mut self) {
                self.0.release_read();
            }
        }
        let _release = Release(self);
        // SAFETY: `off + len <= N`, so the slice is `len` initialised cells of an
        // any-bit-pattern type; the read lock excludes any `&mut` for the call.
        // A device writing outside `[off, off + len)` is not covered by this
        // reference, and the caller vouches the device is done with the inside.
        let cells = unsafe { core::slice::from_raw_parts(self.cells.get().cast::<E>().add(off), len) };
        f(cells)
    }

    /// Run `f` over cells `[off, off + len)` as an exclusive slice, holding the
    /// write borrow for the call.
    ///
    /// The mirror of [`read_range`](Self::read_range), for a buffer the CPU fills
    /// before it hands one **entry** to the device (a transmit frame slot) while
    /// the device may still be reading *other* entries. [`view_mut`](Self::view_mut)
    /// would make the `&mut [E]` cover those too; this covers only the range the
    /// caller has established the device does not own.
    ///
    /// # Panics
    /// If the range is outside the array, or any guard on this array is live.
    #[track_caller]
    pub fn write_range<R>(&self, off: usize, len: usize, f: impl FnOnce(&mut [E]) -> R) -> R {
        assert!(off.checked_add(len).is_some_and(|e| e <= N), "DMA range outside the array");
        assert!(self.acquire_write(), "DMA buffer mutably borrowed while already borrowed");
        // Released on unwind too, so a panicking `f` cannot leave the array
        // write-locked.
        struct Release<'a, E: DmaElem, const N: usize, A>(&'a DmaArray<E, N, A>);
        impl<E: DmaElem, const N: usize, A> Drop for Release<'_, E, N, A> {
            fn drop(&mut self) {
                self.0.release_write();
            }
        }
        let _release = Release(self);
        // SAFETY: `off + len <= N`, so the slice is `len` initialised cells of an
        // any-bit-pattern type; the write lock makes it the only access for the
        // call. A device reading outside `[off, off + len)` is not covered by this
        // reference, and the caller vouches the device does not own the inside.
        let cells = unsafe { core::slice::from_raw_parts_mut(self.cells.get().cast::<E>().add(off), len) };
        f(cells)
    }

    /// Take the array **for good**: an exclusive `&'static mut` over every cell,
    /// for a driver that parks the slice in its own state (a card that owns its
    /// transmit memory from `init` on). The write borrow is never released, so
    /// afterwards every other access - [`get`](Self::get), [`set`](Self::set),
    /// a view, a second `leak_mut` - panics instead of aliasing it. [`addr`](Self::addr)
    /// and [`addr_of`](Self::addr_of) still work: they name the memory without
    /// touching it, which is what the device needs.
    ///
    /// # Panics
    /// If any guard on this array is live, or it was already taken.
    // `&self -> &mut` is the shape clippy warns about because it is usually a way
    // to alias; here the write borrow taken below is never released, which is what
    // makes the returned reference the only one.
    #[allow(clippy::mut_from_ref)]
    #[track_caller]
    pub fn leak_mut(&'static self) -> &'static mut [E] {
        assert!(self.acquire_write(), "DMA buffer taken while already borrowed");
        // SAFETY: the write borrow just acquired is never released, so no guard,
        // `get`/`set` or second `leak_mut` can ever be admitted: this is the only
        // reference to the cells for the rest of the program.
        unsafe { core::slice::from_raw_parts_mut(self.cells.get().cast::<E>(), N) }
    }

    /// Write back and evict the cache lines covering bytes `[off, off + len)`,
    /// then fence — for platforms where device DMA is not snooped (HDA's
    /// no-snoop bit). amd64 only: an AArch64 equivalent would be a different
    /// instruction and a different obligation, and nothing needs it.
    ///
    /// # Panics
    /// If the range is outside the array.
    #[cfg(target_arch = "x86_64")]
    #[track_caller]
    pub fn flush(&self, off: usize, len: usize) {
        const { assert!(core::mem::align_of::<Storage<A, E, N>>() >= 64, "flush needs line-aligned storage") };
        assert!(off.checked_add(len).is_some_and(|e| e <= self.size_bytes()), "DMA flush range outside the array");
        let start = self.addr() + off;
        let end = start + len;
        let mut a = start & !63;
        while a < end {
            // SAFETY: `a` is a cache line inside this array (storage is 64-byte
            // aligned, so rounding down cannot leave it) and `clflush` writes
            // no memory the program can observe.
            unsafe { core::arch::x86_64::_mm_clflush(a as *const u8) };
            a += 64;
        }
        // SAFETY: a fence has no memory-safety requirements.
        unsafe { core::arch::x86_64::_mm_mfence() };
    }
}

impl<E: DmaElem, const N: usize, A> Default for DmaArray<E, N, A> {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared guard over a [`DmaArray`]; derefs to `[E]`.
pub struct DmaRef<'a, E: DmaElem, const N: usize, A> {
    arr: &'a DmaArray<E, N, A>,
}

impl<E: DmaElem, const N: usize, A> Deref for DmaRef<'_, E, N, A> {
    type Target = [E];
    fn deref(&self) -> &[E] {
        // SAFETY: the read lock this guard holds excludes any `&mut`; the
        // pointer is to `N` initialised cells of an any-bit-pattern type.
        unsafe { core::slice::from_raw_parts(self.arr.cells.get().cast::<E>(), N) }
    }
}

impl<E: DmaElem, const N: usize, A> Drop for DmaRef<'_, E, N, A> {
    fn drop(&mut self) {
        self.arr.release_read();
    }
}

/// Exclusive guard over a [`DmaArray`]; derefs to `[E]`.
pub struct DmaMut<'a, E: DmaElem, const N: usize, A> {
    arr: &'a DmaArray<E, N, A>,
}

impl<E: DmaElem, const N: usize, A> Deref for DmaMut<'_, E, N, A> {
    type Target = [E];
    fn deref(&self) -> &[E] {
        // SAFETY: the write lock this guard holds makes it the only access.
        unsafe { core::slice::from_raw_parts(self.arr.cells.get().cast::<E>(), N) }
    }
}

impl<E: DmaElem, const N: usize, A> DerefMut for DmaMut<'_, E, N, A> {
    fn deref_mut(&mut self) -> &mut [E] {
        // SAFETY: as `deref`, and `&mut self` makes the slice unique.
        unsafe { core::slice::from_raw_parts_mut(self.arr.cells.get().cast::<E>(), N) }
    }
}

impl<E: DmaElem, const N: usize, A> Drop for DmaMut<'_, E, N, A> {
    fn drop(&mut self) {
        self.arr.release_write();
    }
}

#[cfg(test)]
mod tests {
    use super::align::{A128, A4K, A64};
    use super::DmaArray;

    #[test]
    fn read_range_covers_only_the_range_and_releases() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        a.view_mut().copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let sum = a.read_range(4, 3, |c| {
            assert_eq!(c.len(), 3);
            c.iter().map(|&b| u32::from(b)).sum::<u32>()
        });
        assert_eq!(sum, 4 + 5 + 6);
        // The read borrow is gone: a writer is admitted again.
        a.set(0, 9);
        assert_eq!(a.read_range(16, 0, |c| c.len()), 0);
    }

    #[test]
    fn write_range_covers_only_the_range_and_releases() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        a.write_range(4, 3, |c| {
            assert_eq!(c.len(), 3);
            c.copy_from_slice(&[1, 2, 3]);
        });
        assert_eq!(a.view().iter().map(|&b| u32::from(b)).sum::<u32>(), 6);
        assert_eq!((a.get(3), a.get(4), a.get(6), a.get(7)), (0, 1, 3, 0));
        // The write borrow is gone: another is admitted.
        a.write_range(0, 0, |_| ());
    }

    #[test]
    #[should_panic(expected = "outside the array")]
    fn write_range_refuses_a_range_past_the_end() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        a.write_range(10, 7, |_| ());
    }

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn write_range_refuses_while_a_reader_is_live() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        let _r = a.view();
        a.write_range(0, 1, |_| ());
    }

    #[test]
    #[should_panic(expected = "outside the array")]
    fn read_range_refuses_a_range_past_the_end() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        a.read_range(10, 7, |_| ());
    }

    #[test]
    #[should_panic(expected = "mutably borrowed")]
    fn read_range_refuses_while_a_writer_is_live() {
        let a: DmaArray<u8, 16, A64> = DmaArray::new();
        let _w = a.view_mut();
        a.read_range(0, 1, |_| ());
    }

    #[test]
    fn leak_mut_is_the_only_access_afterwards() {
        static LEAKED: DmaArray<u8, 8, A64> = DmaArray::new();
        let s = LEAKED.leak_mut();
        s.fill(3);
        // The address is still nameable for the device...
        assert_eq!(LEAKED.addr(), s.as_ptr() as usize);
        // ...but any other access is refused rather than aliasing `s`.
        let r = std::panic::catch_unwind(|| LEAKED.get(0));
        assert!(r.is_err());
        let r = std::panic::catch_unwind(|| LEAKED.leak_mut().len());
        assert!(r.is_err());
        assert_eq!(s[7], 3);
    }

    #[test]
    fn alignment_comes_from_the_marker() {
        static P: DmaArray<u8, 64, A4K> = DmaArray::new();
        static S: DmaArray<u32, 8, super::align::A256> = DmaArray::new();
        assert_eq!(S.addr() % 256, 0);
        static Q: DmaArray<u32, 256, A128> = DmaArray::new();
        static R: DmaArray<[u32; 4], 16, A64> = DmaArray::new();
        assert_eq!(P.addr() % 4096, 0);
        assert_eq!(Q.addr() % 128, 0);
        assert_eq!(R.addr() % 64, 0);
        assert_eq!(R.size_bytes(), 256);
        assert_eq!(R.addr_of(2) - R.addr(), 32);
    }

    #[test]
    fn get_set_and_views_see_the_same_cells() {
        let a: DmaArray<u64, 8, A64> = DmaArray::new();
        a.set(3, 0xDEAD_BEEF);
        assert_eq!(a.get(3), 0xDEAD_BEEF);
        assert_eq!(a.view()[3], 0xDEAD_BEEF);
        a.view_mut().fill(7);
        assert_eq!(a.get(0), 7);
        assert_eq!(a.get(7), 7);
        a.view_mut()[1..3].copy_from_slice(&[1, 2]);
        assert_eq!((a.get(1), a.get(2)), (1, 2));
    }

    #[test]
    fn readers_share_and_a_writer_is_exclusive() {
        let a: DmaArray<u8, 4, A64> = DmaArray::new();
        let r1 = a.view();
        let r2 = a.view();
        assert_eq!(r1.len() + r2.len(), 8);
        assert_eq!(a.get(0), 0, "a volatile read coexists with readers");
        drop((r1, r2));
        let w = a.view_mut();
        assert_eq!(w.len(), 4);
        drop(w);
        a.set(0, 1);
    }

    #[test]
    #[should_panic(expected = "while already borrowed")]
    fn second_writer_panics() {
        let a: DmaArray<u8, 4, A64> = DmaArray::new();
        let _w = a.view_mut();
        let _x = a.view_mut();
    }

    #[test]
    #[should_panic(expected = "while mutably borrowed")]
    fn reader_under_a_writer_panics() {
        let a: DmaArray<u8, 4, A64> = DmaArray::new();
        let _w = a.view_mut();
        let _r = a.view();
    }

    #[test]
    #[should_panic(expected = "while borrowed")]
    fn set_under_a_reader_panics() {
        let a: DmaArray<u8, 4, A64> = DmaArray::new();
        let _r = a.view();
        a.set(0, 1);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn index_past_the_end_panics() {
        let a: DmaArray<u32, 4, A64> = DmaArray::new();
        let _ = a.get(4);
    }

    #[test]
    fn dropping_a_guard_releases_the_borrow() {
        let a: DmaArray<u8, 4, A64> = DmaArray::new();
        for _ in 0..3 {
            let mut w = a.view_mut();
            w[0] += 1;
        }
        assert_eq!(a.get(0), 3);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn flush_accepts_the_whole_array_and_rejects_past_it() {
        let a: DmaArray<u8, 256, A64> = DmaArray::new();
        a.flush(0, 256);
        a.flush(255, 1);
        a.flush(256, 0);
        assert!(std::panic::catch_unwind(|| {
            let b: DmaArray<u8, 256, A64> = DmaArray::new();
            b.flush(200, 100);
        })
        .is_err());
    }
}
