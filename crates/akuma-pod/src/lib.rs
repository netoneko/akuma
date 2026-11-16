//! Plain-old-data: the types whose bytes may cross the user boundary.
//!
//! A user copy moves raw bytes. Reading user bytes into a `T` is only sound if
//! **every bit pattern is a valid `T`**, and handing a `T`'s bytes to user space
//! is only sound (and only free of a kernel-memory leak) if **`T` has no
//! padding**. `T: Copy` promises neither: `bool`, a field-less enum, `char`,
//! `NonZeroU64` and `&'static u8` are all `Copy`, and a `repr(C)` struct of a
//! `u32` and a `u64` is `Copy` with four uninitialised bytes in it. Until this
//! crate the user-copy helpers took `T: Copy`, so safe code could fill a
//! reference with user-chosen bits, or copy stack padding out to user space.
//!
//! [`Pod`] states both properties, and the type system carries them to every
//! `read_user_into` / `write_user_val` call.
//!
//! # Admitting a type
//!
//! * integers and arrays of `Pod` are admitted here;
//! * a struct is admitted with [`pod!`], which **checks** what the `unsafe impl`
//!   claims, at compile time:
//!   1. every listed field has the listed type (a coercion that fails to compile
//!      otherwise),
//!   2. every field type is itself `Pod`,
//!   3. the fields tile the struct exactly — no two overlap and their sizes sum to
//!      `size_of` the struct — so there is no padding and no unlisted field.
//!
//!   Together those make the claim true for the struct, which is why invoking
//!   the macro needs no `unsafe` at the call site and works inside a
//!   `#![forbid(unsafe_code)]` crate. The macro needs the fields to be visible
//!   where it is invoked, so only code that can already build the struct from
//!   arbitrary field values can admit it — a type with a private invariant stays
//!   out unless its own module says otherwise.
//!
//! The only hand-written `unsafe impl`s are the ones in this file.

#![cfg_attr(not(test), no_std)]

/// A `Copy` type whose every bit pattern is valid and which has no padding.
///
/// # Safety
/// Implement it only for a type where (a) any `size_of::<Self>()` initialised
/// bytes are a valid value, and (b) every byte of every value is initialised
/// (no padding). Prefer [`pod!`], which checks both for a struct.
pub unsafe trait Pod: Copy + 'static {}

macro_rules! pod_primitive {
    ($($t:ty),*) => {
        // SAFETY: integers have no padding and no invalid bit patterns.
        $( unsafe impl Pod for $t {} )*
    };
}
pod_primitive!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize);

// SAFETY: an array has no padding between elements (its stride is the element
// size) and no bytes of its own, so it is `Pod` exactly when the element is.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

/// The bytes of a `Pod` slice.
#[must_use]
pub fn as_bytes<T: Pod>(vals: &[T]) -> &[u8] {
    // SAFETY: `size_of_val(vals)` bytes of a live slice, read-only; `T: Pod` makes
    // every one of them initialised, and `u8` has no alignment requirement.
    unsafe { core::slice::from_raw_parts(vals.as_ptr().cast::<u8>(), core::mem::size_of_val(vals)) }
}

/// The bytes of a `Pod` slice, writable: any bytes stored through it leave a
/// valid `[T]`, because `T: Pod` accepts every bit pattern.
#[must_use]
pub fn as_bytes_mut<T: Pod>(vals: &mut [T]) -> &mut [u8] {
    let len = core::mem::size_of_val(vals);
    // SAFETY: `len` bytes of a live, exclusively borrowed slice for the returned
    // lifetime; `T: Pod` makes any bytes written a valid `T`.
    unsafe { core::slice::from_raw_parts_mut(vals.as_mut_ptr().cast::<u8>(), len) }
}

/// The bytes of one `Pod` value.
#[must_use]
pub fn bytes_of<T: Pod>(v: &T) -> &[u8] {
    as_bytes(core::slice::from_ref(v))
}

/// The bytes of one `Pod` value, writable.
#[must_use]
pub fn bytes_of_mut<T: Pod>(v: &mut T) -> &mut [u8] {
    as_bytes_mut(core::slice::from_mut(v))
}

/// A `Pod` value read from the front of `bytes`, or `None` if `bytes` is shorter.
#[must_use]
pub fn read_from<T: Pod + Default>(bytes: &[u8]) -> Option<T> {
    let mut v = T::default();
    let n = core::mem::size_of::<T>();
    bytes_of_mut(&mut v).copy_from_slice(bytes.get(..n)?);
    Some(v)
}

#[doc(hidden)]
pub const fn assert_pod<T: Pod>() {}

/// Whether `fields` (`(offset, size)` pairs) cover `[0, size)` exactly: pairwise
/// disjoint and summing to `size`. Disjoint intervals inside a struct whose
/// lengths sum to its size leave no byte uncovered.
#[doc(hidden)]
#[must_use]
pub const fn tiles(size: usize, fields: &[(usize, usize)]) -> bool {
    let mut sum = 0usize;
    let mut i = 0;
    while i < fields.len() {
        let (a, al) = fields[i];
        sum += al;
        let mut j = i + 1;
        while j < fields.len() {
            let (b, bl) = fields[j];
            if al != 0 && bl != 0 && a < b + bl && b < a + al {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    sum == size
}

/// Admit a struct as [`Pod`], checking the claim at compile time.
///
/// List **every** field with its type: `pod!(Timespec { tv_sec: i64, tv_nsec: i64 });`,
/// and a tuple struct's fields by index: `pod!(Words { 0: [u64; 4] });`.
/// Several structs may be listed, separated by `;`. A struct with padding, an
/// unlisted field, a field listed twice, a wrong field type or a non-`Pod` field
/// fails to compile.
#[macro_export]
macro_rules! pod {
    ($($t:ty { $($f:tt : $ft:ty),* $(,)? });+ $(;)?) => {$(
        const _: () = {
            $(
                let _: fn(&$t) -> $ft = |v| v.$f;
                $crate::assert_pod::<$ft>();
            )*
            assert!(
                $crate::tiles(
                    ::core::mem::size_of::<$t>(),
                    &[$((::core::mem::offset_of!($t, $f), ::core::mem::size_of::<$ft>())),*],
                ),
                concat!("pod!: ", stringify!($t), " has padding, or a field is missing or repeated"),
            );
        };
        // SAFETY: the `const` block above proved every field `Pod` and the fields
        // a padding-free tiling of the struct, which is the trait's whole contract.
        unsafe impl $crate::Pod for $t {}
    )+};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    #[repr(C)]
    struct Wire {
        a: u32,
        b: u32,
        c: u64,
        d: [u16; 4],
    }
    pod!(Wire { a: u32, b: u32, c: u64, d: [u16; 4] });

    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Pair {
        x: i32,
        y: i32,
    }
    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Nested {
        p: Pair,
        n: [Pair; 2],
    }
    pod!(Pair { x: i32, y: i32 }; Nested { p: Pair, n: [Pair; 2] });

    /// `repr(packed)`: no padding by construction, and fields that cannot be
    /// borrowed - the macro's type check reads them by value for this reason.
    #[derive(Clone, Copy)]
    #[repr(C, packed)]
    struct Packed {
        a: u32,
        b: u64,
    }
    pod!(Packed { a: u32, b: u64 });

    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Tuple(u32, [u8; 4]);
    pod!(Tuple { 0: u32, 1: [u8; 4] });

    #[test]
    fn tiling_accepts_exact_cover_only() {
        assert!(tiles(16, &[(0, 4), (4, 4), (8, 8)]));
        assert!(!tiles(16, &[(0, 4), (8, 8)]), "padding at 4..8");
        assert!(!tiles(16, &[(0, 4), (0, 4), (8, 8)]), "a repeated field must not paper over padding");
        assert!(!tiles(16, &[(0, 8), (4, 8)]), "overlap");
        assert!(tiles(0, &[]));
    }

    #[test]
    fn byte_views_round_trip() {
        let w = Wire { a: 1, b: 2, c: 3, d: [4, 5, 6, 7] };
        let bytes = bytes_of(&w);
        assert_eq!(bytes.len(), 24);
        let back: Wire = read_from(bytes).unwrap();
        assert_eq!(back, w);
        assert!(read_from::<Wire>(&bytes[..23]).is_none());

        let mut v = Wire::default();
        bytes_of_mut(&mut v).fill(0xff);
        assert_eq!(v.c, u64::MAX);

        let n = Nested { p: Pair { x: 1, y: -1 }, n: [Pair { x: 2, y: 3 }; 2] };
        assert_eq!(bytes_of(&n).len(), 24);
        let mut arr = [0u32; 3];
        as_bytes_mut(&mut arr)[4] = 9;
        assert_eq!(arr, [0, 9, 0]);
        assert_eq!(as_bytes(&arr).len(), 12);
        assert_eq!(bytes_of(&Packed { a: 1, b: 2 }).len(), 12);
        assert_eq!(bytes_of(&Tuple(1, [2; 4])), &[1, 0, 0, 0, 2, 2, 2, 2]);
    }
}
