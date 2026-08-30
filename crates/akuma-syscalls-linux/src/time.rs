//! `struct timespec`, `struct timeval`, `struct itimerval`, `struct timex`.
//!
//! # The five spellings this file replaces
//!
//! [`Timespec`] existed five times before 2026-08-27, in two representations,
//! for the reason `akuma_primitives::errno` exists: the definition lived in the
//! bin crate, so `akuma-syscalls-time` — which *is* the timespec syscalls — could not
//! reach it and wrote its own, and named it `LocalTimespec` to say so.
//!
//! | was | fields | now |
//! |---|---|---|
//! | `src/syscall/mod.rs::Timespec` | `i64`/`i64` | this |
//! | `src/syscall/timerfd.rs::LocalTimespec` | `u64`/`u64` | this |
//! | `crates/akuma-syscalls-time::LocalTimespec` | `u64`/`u64` | this |
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
    /// `timespec_to_us_safe`, `akuma-syscalls-time`'s sleep and `clock_settime` paths).
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

    /// The whole struct as a microsecond count — **the** timespec-to-timeout
    /// conversion in this tree.
    ///
    /// Seven call sites spelled this by hand before 2026-08-28, in two
    /// arithmetics that differed only in overflow behaviour:
    ///
    /// | sites | spelling |
    /// |---|---|
    /// | `pselect6`, `ppoll`, `rt_sigtimedwait`, `futex`, `timerfd` | `(ts.tv_sec as u64) * 1_000_000 + …` — wraps |
    /// | `nanosleep`, `clock_nanosleep`, `clock_settime` | `sec.saturating_mul(1_000_000)…` — saturates |
    ///
    /// Both cast to `u64` first — `tv_sec as u64` and [`bits`](Self::bits) are
    /// the same reinterpretation — so the *only* difference was what a
    /// too-large value does, and the two families were one `sed` away from
    /// swapping behaviour silently. This method is the saturating one, which
    /// makes the five wrapping sites saturate too. The difference is not
    /// theoretical and does not need a negative field to reach it:
    /// `tv_sec = 18_446_744_073_710` is an ordinary positive `i64`, and
    /// `18_446_744_073_710 * 1_000_000` wraps to **448_384** — so a `ppoll`
    /// asked to wait ~584942 years returned after 0.45 seconds. Clamping to
    /// `u64::MAX` keeps "absurdly large" meaning "absurdly large".
    ///
    /// This is not the Linux `EINVAL` a negative `tv_sec` earns; none of the
    /// seven sites implemented that and adding it here would change syscall
    /// behaviour behind a conversion helper's back. `read_timeout_us`'s caller
    /// is the right place for that if it is ever wanted.
    #[must_use]
    pub const fn to_us(self) -> u64 {
        let (sec, nsec) = self.bits();
        sec.saturating_mul(1_000_000).saturating_add(nsec / 1_000)
    }
}

/// Linux `struct timeval` — `{ time_t tv_sec; suseconds_t tv_usec; }`.
///
/// Both fields 64-bit, 16 bytes total; musl passes this shape for
/// `SO_RCVTIMEO`/`SO_SNDTIMEO`. Same signed/unsigned story as [`Timespec`]:
/// `src/syscall/net.rs` spelled it `i64` (correct) and `akuma-syscalls-time` spelled it
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

    /// The whole struct as a microsecond count. See [`Timespec::to_us`] — same
    /// saturation, one fewer division because `tv_usec` is already
    /// microseconds.
    ///
    /// The one caller (`SO_RCVTIMEO`/`SO_SNDTIMEO` in `src/syscall/net.rs`)
    /// already saturated, and rejects a negative field with `EINVAL` before
    /// calling this — which is the check the timespec sites do not have.
    #[must_use]
    pub const fn to_us(self) -> u64 {
        let (sec, usec) = self.bits();
        sec.saturating_mul(1_000_000).saturating_add(usec)
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
        let ts = Timespec { tv_sec: 1, tv_nsec: 2 };
        assert_eq!(core::mem::offset_of!(Timespec, tv_sec), 0, "tv_sec is the low word");
        assert_eq!(core::mem::size_of_val(&ts.tv_sec), 8);
        assert_eq!(core::mem::offset_of!(Timespec, tv_nsec), 8);
        assert_eq!(core::mem::size_of_val(&ts.tv_nsec), 8);
        assert_eq!(core::mem::size_of::<Timespec>(), 16);
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

    /// The ordinary case, and the sub-microsecond truncation every caller
    /// relied on: `tv_nsec / 1_000` floors, so a 999 ns timeout is 0 us and
    /// polls once rather than blocking.
    #[test]
    fn to_us_converts_and_truncates_sub_microseconds() {
        assert_eq!(Timespec { tv_sec: 0, tv_nsec: 0 }.to_us(), 0);
        assert_eq!(Timespec { tv_sec: 0, tv_nsec: 999 }.to_us(), 0);
        assert_eq!(Timespec { tv_sec: 0, tv_nsec: 1_000 }.to_us(), 1);
        assert_eq!(Timespec { tv_sec: 1, tv_nsec: 500_000_000 }.to_us(), 1_500_000);
        assert_eq!(Timeval { tv_sec: 1, tv_usec: 500_000 }.to_us(), 1_500_000);
    }

    /// The behaviour change this method makes, pinned so it cannot drift back.
    ///
    /// The witness is a *positive* `tv_sec`, which is what makes the old
    /// arithmetic a real defect rather than a hostile-input curiosity: no sign
    /// reinterpretation is involved, the value is a valid `i64`, and the
    /// wrapping spelling turned a ~584942 year timeout into 0.45 seconds. The
    /// old answer is asserted alongside the new one so the test names both.
    #[test]
    fn to_us_saturates_where_the_old_arithmetic_wrapped() {
        const HUGE: i64 = 18_446_744_073_710;
        assert_eq!(HUGE.cast_unsigned().wrapping_mul(1_000_000), 448_384);
        assert_eq!(Timespec { tv_sec: HUGE, tv_nsec: 0 }.to_us(), u64::MAX);
        assert_eq!(Timeval { tv_sec: HUGE, tv_usec: 0 }.to_us(), u64::MAX);
        assert_eq!(Timespec { tv_sec: -1, tv_nsec: 0 }.to_us(), u64::MAX);
        assert_eq!(Timeval { tv_sec: -1, tv_usec: 0 }.to_us(), u64::MAX);
        assert_eq!(Timespec { tv_sec: i64::MAX, tv_nsec: i64::MAX }.to_us(), u64::MAX);
    }

    /// `to_us` and the hand-written arithmetic must agree everywhere the old
    /// spelling did not overflow — which is every value a real caller passes.
    #[test]
    fn to_us_matches_the_hand_written_arithmetic_below_overflow() {
        for sec in [0_i64, 1, 60, 86_400, 1_000_000_000] {
            for nsec in [0_i64, 1, 999, 1_000, 999_999_999] {
                let ts = Timespec { tv_sec: sec, tv_nsec: nsec };
                let by_hand = (sec as u64) * 1_000_000 + (nsec as u64) / 1_000;
                assert_eq!(ts.to_us(), by_hand, "diverged at {sec}s {nsec}ns");
            }
        }
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
