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
//! # What it returns
//!
//! The abbreviated git SHA as a number, and nothing else. One `u64`, because a
//! register return is the only free one — writing a struct to a user buffer
//! would reintroduce the `copy_to_user` this call exists to avoid.
//!
//! It used to pack a hand-maintained `[major, minor, patch]` alongside the
//! commit, under a layout with its own `pack`/`unpack` pair and a compile-time
//! round-trip assert. That triple is gone (2026-09-11): **nothing in userspace
//! ever read this value** — it is floor control for a benchmark, not an ABI —
//! and the triple's only real effect was to be a second version number that
//! disagreed with the kernel's package version and with [`RELEASE`]. The commit
//! identifies a build exactly; a hand-maintained triple identifies nothing.
//!
//! A build outside a git checkout returns **0**, which is the honest "no
//! commit". Bit 63 can never be set (the value is a `u32`), so no libc wrapper
//! can read it as a negative errno — which the packed form had to reserve a
//! whole byte to guarantee.

/// **The kernel release — `uname -r`, and the amd64 banner.** One literal.
///
/// **It comes from `Cargo.toml`**, via this crate's `build.rs`, which reads the
/// `[package] version` of the workspace root — the manifest `cargo` builds the
/// kernel from. Nothing here is hand-maintained: a `cargo` bump moves `uname -r`
/// and nobody has to remember to.
///
/// Deliberately not `env!("CARGO_PKG_VERSION")`. That macro expands to *the
/// crate it is written in*, so when `src/syscall/` became `akuma-syscalls-glue`
/// on 2026-09-01 the `uname -r` arm in `proc.rs` silently stopped reporting the
/// kernel (`0.0.7`) and started reporting the glue crate (`0.1.0`) — on both
/// kernels, for ten days, with nothing to refuse it. Exactly the same class of
/// break as `AKUMA_GIT_SHA`, which that move caught only because it failed to
/// compile, and fixed in the same place for the same reason: `rustc-env` does
/// not propagate across crates, so a crate cannot otherwise see the version of
/// the binary linking it.
///
/// A literal here would have been wrong too, and not only in principle — one
/// that happens to match the manifest is indistinguishable from one that has
/// drifted, which is the failure being closed. Verified by moving the manifest
/// and watching `uname -r` follow, not by reading the two and agreeing they
/// look alike.
///
/// The amd64 kernel is its own package (`amd64/Cargo.toml`) carrying the same
/// number; this reads the root either way, so the two cannot report different
/// releases — and if they are ever meant to, that is a decision to make here
/// rather than a drift to discover.
pub const RELEASE: &str = env!("AKUMA_KERNEL_VERSION");

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

/// The value `akuma_get_version` returns. One immediate.
pub const AKUMA_VERSION: u64 = COMMIT as u64;

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
