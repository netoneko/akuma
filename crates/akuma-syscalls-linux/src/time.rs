//! `struct timespec`, `struct timeval`, `struct itimerval`, `struct timex`.
//!
//! # The five spellings this file replaces
//!
//! [`Timespec`] existed five times before 2026-08-27, in two representations,
//! for the reason `akuma_primitives::errno` exists: the definition lived in the
//! bin crate, so `akuma-time` — which *is* the timespec syscalls — could not
//! reach it and wrote its own, and named it `LocalTimespec` to say so.
//!
//! | was | fields | now |
//! |---|---|---|
//! | `src/syscall/mod.rs::Timespec` | `i64`/`i64` | this |
//! | `src/syscall/timerfd.rs::LocalTimespec` | `u64`/`u64` | this |
//! | `crates/akuma-time::LocalTimespec` | `u64`/`u64` | this |
//! | `src/sync_tests.rs::Timespec` | `i64`/`i64` | this |
//! | `src/process_tests.rs::Timespec` | `i64`/`i64` | this |
//!
//! **The signedness split was a real divergence, not a typo.** Linux's
//! `struct timespec` on aarch64 is `{ time_t tv_sec; long tv_nsec; }` — both
//! signed — so the `i64` spelling is the correct one and it is what this type
//! uses. The two `u64` copies were not merely mislabelled: their callers do
//! *unsigned* saturating arithmetic on the fields, which differs from the
//! signed version for any value with the top bit set. Rather than change
//! behaviour inside a refactor, those call sites keep their unsigned
//! arithmetic explicitly, through [`Timespec::bits`] — the cast is now visible
//! at the site instead of hidden in a private struct definition.

/// Linux `struct timespec` — `{ time_t tv_sec; long tv_nsec; }`, both signed
/// 64-bit on aarch64 LP64.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl Timespec {
    /// The two fields reinterpreted as unsigned, for the call sites that did
    /// unsigned arithmetic on them before this type existed (`timerfd`'s
    /// `timespec_to_us_safe`, `akuma-time`'s sleep and `clock_settime` paths).
    ///
    /// Not a conversion — the bits are the same either way, because
    /// `read_user_into` copies them verbatim from userspace. This exists so
    /// that "this path treats a negative `tv_sec` as an enormous positive one"
    /// is written down at the site that does it.
    #[must_use]
    pub const fn bits(self) -> (u64, u64) {
        (self.tv_sec.cast_unsigned(), self.tv_nsec.cast_unsigned())
    }

    /// Build from an unsigned pair, for the paths that compute seconds and
    /// nanoseconds as `u64` (clock reads divide a `u64` microsecond count).
    #[must_use]
    pub const fn from_bits(tv_sec: u64, tv_nsec: u64) -> Self {
        Self { tv_sec: tv_sec.cast_signed(), tv_nsec: tv_nsec.cast_signed() }
    }
}

/// Linux `struct timeval` — `{ time_t tv_sec; suseconds_t tv_usec; }`.
///
/// Both fields 64-bit, 16 bytes total; musl passes this shape for
/// `SO_RCVTIMEO`/`SO_SNDTIMEO`. Same signed/unsigned story as [`Timespec`]:
/// `src/syscall/net.rs` spelled it `i64` (correct) and `akuma-time` spelled it
/// `u64` (`LocalTimeval`).
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl Timeval {
    /// See [`Timespec::bits`].
    #[must_use]
    pub const fn bits(self) -> (u64, u64) {
        (self.tv_sec.cast_unsigned(), self.tv_usec.cast_unsigned())
    }

    /// See [`Timespec::from_bits`].
    #[must_use]
    pub const fn from_bits(tv_sec: u64, tv_usec: u64) -> Self {
        Self { tv_sec: tv_sec.cast_signed(), tv_usec: tv_usec.cast_signed() }
    }
}

/// Linux `struct itimerval`, the `setitimer(2)`/`getitimer(2)` buffer.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Itimerval {
    pub it_interval: Timeval,
    pub it_value: Timeval,
}

/// aarch64 Linux `struct timex` (`<linux/timex.h>`), 208 bytes.
///
/// Every `long` field is 8 bytes on this ABI, which is why
/// `modes`/`status`/`shift` each need explicit 4-byte padding to keep the
/// following `i64` 8-byte aligned — get this wrong and every field after the
/// first padding gap silently reads the wrong bytes. That is exactly the class
/// of bug the assertions in this crate's tests exist to catch, and exactly the
/// class a QEMU boot is worst at catching.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timex {
    pub modes: u32,
    pub _pad0: u32,
    pub offset: i64,
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: i32,
    pub _pad1: u32,
    pub constant: i64,
    pub precision: i64,
    pub tolerance: i64,
    pub time_sec: i64,
    pub time_usec: i64,
    pub tick: i64,
    pub ppsfreq: i64,
    pub jitter: i64,
    pub shift: i32,
    pub _pad2: u32,
    pub stabil: i64,
    pub jitcnt: i64,
    pub calcnt: i64,
    pub errcnt: i64,
    pub stbcnt: i64,
    pub tai: i32,
    pub _reserved: [i32; 11],
}

// The layout claims the doc comments above make. `sync_tests.rs` asserted the
// first two at boot; they are compile-time facts, so they are asserted here
// instead and cost nothing at runtime.
const _: () = assert!(core::mem::size_of::<Timespec>() == 16);
const _: () = assert!(core::mem::align_of::<Timespec>() == 8);
const _: () = assert!(core::mem::offset_of!(Timespec, tv_nsec) == 8);
const _: () = assert!(core::mem::size_of::<Timeval>() == 16);
const _: () = assert!(core::mem::offset_of!(Timeval, tv_usec) == 8);
const _: () = assert!(core::mem::size_of::<Itimerval>() == 32);
const _: () = assert!(core::mem::offset_of!(Itimerval, it_value) == 16);
const _: () = assert!(core::mem::size_of::<Timex>() == 208);
// The three padding words, pinned individually: a missing `_pad` is invisible
// in a size check on some of them but shifts every later field.
const _: () = assert!(core::mem::offset_of!(Timex, offset) == 8);
const _: () = assert!(core::mem::offset_of!(Timex, constant) == 48);
const _: () = assert!(core::mem::offset_of!(Timex, stabil) == 120);
const _: () = assert!(core::mem::offset_of!(Timex, tai) == 160);

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte pattern `sync_tests.rs`'s boot test wrote and read back, now a
    /// host test: the field order is what puts `tv_sec` in the low 8 bytes.
    #[test]
    fn timespec_field_order_is_sec_then_nsec() {
        let ts = Timespec {
            tv_sec: 0x1234_5678_9ABC_DEF0_u64.cast_signed(),
            tv_nsec: 0xFEDC_BA98_7654_3210_u64.cast_signed(),
        };
        let raw: [u8; 16] = unsafe { core::mem::transmute(ts) };
        assert_eq!(u64::from_le_bytes(raw[0..8].try_into().unwrap()), 0x1234_5678_9ABC_DEF0);
        assert_eq!(u64::from_le_bytes(raw[8..16].try_into().unwrap()), 0xFEDC_BA98_7654_3210);
    }

    /// `bits`/`from_bits` are a reinterpretation, not a conversion: a negative
    /// `tv_sec` must come back as the same 64 bits, which is precisely the
    /// behaviour the `u64` copies of this struct had.
    #[test]
    fn bits_round_trips_through_the_sign_boundary() {
        let ts = Timespec { tv_sec: -1, tv_nsec: i64::MIN };
        assert_eq!(ts.bits(), (u64::MAX, 1 << 63));
        assert_eq!(Timespec::from_bits(u64::MAX, 1 << 63), ts);
        let tv = Timeval { tv_sec: -1, tv_usec: -2 };
        assert_eq!(tv.bits(), (u64::MAX, u64::MAX - 1));
        assert_eq!(Timeval::from_bits(u64::MAX, u64::MAX - 1), tv);
    }

    /// The reason `Timex` carries three explicit pad words. Written as a test
    /// rather than only as a `const _` so a failure names the field.
    #[test]
    fn timex_long_fields_stay_eight_byte_aligned() {
        for (name, off) in [
            ("offset", core::mem::offset_of!(Timex, offset)),
            ("constant", core::mem::offset_of!(Timex, constant)),
            ("time_sec", core::mem::offset_of!(Timex, time_sec)),
            ("stabil", core::mem::offset_of!(Timex, stabil)),
        ] {
            assert_eq!(off % 8, 0, "{name} is not 8-byte aligned within timex");
        }
        assert_eq!(core::mem::size_of::<Timex>(), 208);
    }
}
