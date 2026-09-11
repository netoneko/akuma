//! `akuma_get_version` — the build identity, packed into the return register.
//!
//! # Why this exists when `uname(2)` already reports the same facts
//!
//! It is the **floor control** for the syscall boundary. `getpid` has been
//! doing that job and it is not honest at it: its arm still resolves a process,
//! so its cost is the boundary *plus* an arm. `uname` is worse — it reports the
//! same version and git SHA this call does, from the same `env!`s, and computes
//! nothing at all, but delivering ~30 bytes of static text costs a
//! `validate_user_ptr`, a 390-byte stack memset, six `copy_from_slice`s and a
//! 390-byte `copy_to_user`. Roughly 780 bytes moved and two validations. That
//! measures the user-copy path, not the boundary.
//!
//! This arm does **nothing**: no arguments are read, no user memory is touched,
//! no process is resolved. [`AKUMA_VERSION`] is a compile-time constant, so the
//! dispatch arm is one immediate into `x0`. What is left in the measurement is
//! the EL0 round trip, the `wrap` layer and `handle_syscall`'s prologue and
//! epilogue — which is exactly the thing
//! `docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md` is trying to price.
//!
//! # The packing
//!
//! One `u64`, because a register return is the only free one — writing an array
//! to a user buffer would reintroduce the `copy_to_user` this call exists to
//! avoid.
//!
//! ```text
//!  63    56 55    48 47    40 39    32 31                     0
//! +--------+--------+--------+--------+------------------------+
//! |   0    | major  | minor  | patch  |      commit (hex)      |
//! +--------+--------+--------+--------+------------------------+
//! ```
//!
//! The top byte is reserved and always zero, which is load-bearing: it keeps
//! the value below `2^56`, so no libc wrapper can mistake it for a negative
//! errno the way a value with bit 63 set would be.
//!
//! Userspace unpacks with shifts; there is no ABI struct to keep in sync.

/// **The kernel release. One literal, and everything that names a version
/// reads it**: `uname -r`, this call's packed triple, and the amd64 banner.
///
/// Deliberately not `CARGO_PKG_VERSION`, and the reason is no longer only the
/// ABI argument it used to be. `env!("CARGO_PKG_VERSION")` in a crate expands to
/// *that crate's* version, so when `src/syscall/` became `akuma-syscalls-glue`
/// on 2026-09-01 the `uname -r` arm in `proc.rs` silently stopped reporting the
/// kernel (`0.0.7`) and started reporting the glue crate (`0.1.0`) — on both
/// kernels, for ten days, with nothing to refuse it. Exactly the same class of
/// break as `AKUMA_GIT_SHA`, which that move caught because it failed to
/// compile; this one still compiled and just answered wrong.
///
/// A hand-maintained value is the right shape here (a routine `Cargo.toml` bump
/// should not be a silent ABI change) — it simply has to be a value that
/// *cannot* be a different crate's. Bump it here and nowhere else.
pub const RELEASE: &str = "0.0.8";

/// The version this call reports, as `[major, minor, patch]`.
///
/// Derived from [`RELEASE`] rather than written beside it: two hand-maintained
/// spellings of one number is how `uname -r` and `akuma_get_version` would come
/// to disagree, and the doc here used to say so out loud ("whoever changes one
/// should look at the other"). Now there is only one to change.
pub const VERSION_TRIPLE: [u8; 3] = parse_triple(RELEASE);

/// Parse `"major.minor.patch"` into its three bytes, at compile time.
///
/// Deliberately strict — a component that is not a decimal number, a missing or
/// extra dot, or a component above 255 is a **compile error**, not a `0`. This
/// runs once per build on a literal in this file; a lenient parse would turn a
/// typo into a plausible wrong version, which is the failure mode the whole
/// const is here to remove.
#[must_use]
pub const fn parse_triple(s: &str) -> [u8; 3] {
    let b = s.as_bytes();
    let mut out = [0u8; 3];
    let mut field = 0;
    let mut acc: u32 = 0;
    let mut digits = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c @ b'0'..=b'9' => {
                acc = acc * 10 + (c - b'0') as u32;
                assert!(acc <= 255, "version component above 255");
                digits += 1;
            }
            b'.' => {
                assert!(digits > 0, "empty version component");
                assert!(field < 2, "too many version components");
                out[field] = acc as u8;
                field += 1;
                acc = 0;
                digits = 0;
            }
            _ => panic!("version is not major.minor.patch"),
        }
        i += 1;
    }
    assert!(digits > 0, "empty version component");
    assert!(field == 2, "too few version components");
    out[2] = acc as u8;
    out
}

/// The commit, as the numeric value of the abbreviated git SHA.
///
/// **This crate's own `build.rs` embeds `AKUMA_GIT_SHA`**, and the binary's no
/// longer does. `rustc-env` does not propagate across crates, so when this became
/// a crate the const stopped compiling — and the fix is to move the derivation
/// down here rather than have the binary compute a value and hand it back
/// through a hook. Nothing else in the tree reads the variable, so there is still
/// exactly one `git rev-parse` in the build.
///
/// A build outside a git checkout gets the literal `"unknown"`, which parses to
/// `0` — the same answer as "no commit", which is what it means.
pub const COMMIT: u32 = parse_hex_prefix(env!("AKUMA_GIT_SHA"));

/// The packed value `akuma_get_version` returns. One immediate.
pub const AKUMA_VERSION: u64 = pack(
    VERSION_TRIPLE[0],
    VERSION_TRIPLE[1],
    VERSION_TRIPLE[2],
    COMMIT,
);

/// Pack a version triple and commit into the returned `u64`.
#[must_use]
pub const fn pack(major: u8, minor: u8, patch: u8, commit: u32) -> u64 {
    ((major as u64) << 48) | ((minor as u64) << 40) | ((patch as u64) << 32) | commit as u64
}

/// The inverse of [`pack`].
///
/// Used by the boot test, so it can assert against the layout rather than
/// against a magic number that would have to be edited in step with it — a test
/// you have to update to keep passing checks nothing.
///
/// Also used by the round-trip assertion below, which is what keeps it from
/// being dead code in a `no-tests` build. That is the better answer than a
/// `#[cfg(kernel_tests)]` gate: the check it enables costs nothing at runtime
/// and is worth having in every build, not only the ones that run the suite.
#[must_use]
pub const fn unpack(v: u64) -> (u8, u8, u8, u32) {
    (
        (v >> 48) as u8,
        (v >> 40) as u8,
        (v >> 32) as u8,
        v as u32,
    )
}

/// `pack` and `unpack` must agree, checked at compile time.
///
/// The two encode the same field layout in opposite directions, and nothing but
/// this stops them drifting — a shifted field would still produce a plausible
/// number, and the value only ever reaches userspace, where a wrong answer
/// looks like a wrong kernel rather than a wrong shift.
const _: () = {
    let (major, minor, patch, commit) = unpack(AKUMA_VERSION);
    assert!(major == VERSION_TRIPLE[0]);
    assert!(minor == VERSION_TRIPLE[1]);
    assert!(patch == VERSION_TRIPLE[2]);
    assert!(commit == COMMIT);
    // The reserved top byte, which is what keeps the value from ever reading as
    // a negative errno in a libc wrapper.
    assert!((AKUMA_VERSION >> 56) == 0);
};

/// Read up to 8 leading hex digits of `s` as a number; anything else yields 0.
///
/// `git rev-parse --short` gives 7 digits by default, and `core.abbrev` can make
/// it longer — the leading 8 are taken so a repo configured for longer SHAs
/// still produces a stable, non-truncating-to-garbage number rather than
/// overflowing the field. A non-hex byte (the `"unknown"` fallback, or a
/// `-dirty` suffix marker) stops the parse and returns 0 rather than a partial
/// value that would look like a real commit.
#[must_use]
pub const fn parse_hex_prefix(s: &str) -> u32 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0;
    }
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < b.len() && i < 8 {
        let d = match b[i] {
            c @ b'0'..=b'9' => c - b'0',
            c @ b'a'..=b'f' => c - b'a' + 10,
            c @ b'A'..=b'F' => c - b'A' + 10,
            _ => return 0,
        };
        acc = (acc << 4) | d as u32;
        i += 1;
    }
    acc
}

/// `akuma_get_version(2)`. Takes nothing, touches nothing, returns a constant.
///
/// Kept as a named function rather than inlined into the dispatch `match` so
/// the arm reads like every other one; it compiles to the same immediate.
#[inline]
pub(super) fn sys_akuma_get_version() -> u64 {
    AKUMA_VERSION
}

/// `parse_triple` is what makes [`RELEASE`] and [`VERSION_TRIPLE`] one fact
/// instead of two, so it is worth a few cases of its own.
///
/// The rejection cases cannot be tested — they are `panic!`s in a `const fn`,
/// which is a *compile* error at the one call site and not something a test can
/// observe. That is the point of them; what is tested here is that the accepting
/// path accepts what it should and puts the components in the right order, which
/// is the failure a malformed-input check would not catch.
#[cfg(test)]
mod tests {
    use super::{RELEASE, VERSION_TRIPLE, parse_triple};

    #[test]
    fn parses_the_shipped_release() {
        assert_eq!(VERSION_TRIPLE, parse_triple(RELEASE));
    }

    #[test]
    fn components_are_in_order() {
        assert_eq!(parse_triple("1.2.3"), [1, 2, 3]);
    }

    #[test]
    fn multi_digit_components() {
        assert_eq!(parse_triple("10.255.0"), [10, 255, 0]);
    }

    #[test]
    fn zeroes_are_not_special() {
        assert_eq!(parse_triple("0.0.0"), [0, 0, 0]);
    }
}
