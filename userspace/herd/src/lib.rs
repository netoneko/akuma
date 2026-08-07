//! Library half of the herd supervisor, holding the parts that are pure enough
//! to unit-test on the host.
//!
//! The binary (`main.rs`) is `no_std` + `no_main` for `aarch64-unknown-none` and
//! links `libakuma`, whose `#[panic_handler]` / `#[global_allocator]` make it
//! unlinkable against a std target — so the binary itself can never be
//! host-tested. Anything that is just a decision over values belongs here
//! instead, behind a feature gate that leaves `libakuma` out:
//!
//! ```text
//! cargo test -p herd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! `no_std` is dropped under `cfg(test)` so the test harness can link std.
//!
//! Same arrangement as `userspace/sshd/src/lib.rs`, for the same reason.

#![cfg_attr(not(test), no_std)]

pub mod exit;
