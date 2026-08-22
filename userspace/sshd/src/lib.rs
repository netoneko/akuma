//! Library half of the userspace sshd, holding the parts that are pure enough
//! to unit-test on the host.
//!
//! The binary (`main.rs`) is `no_std` + `no_main` for `aarch64-unknown-none` and
//! links `libakuma`, whose `#[panic_handler]` / `#[global_allocator]` make it
//! unlinkable against a std target — so the binary itself can never be
//! host-tested. Anything that is just logic over bytes belongs here instead,
//! behind a feature gate that leaves `libakuma` out:
//!
//! ```text
//! cargo test -p sshd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! `no_std` is dropped under `cfg(test)` so the test harness can link std;
//! `alloc` is available either way.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod client_wire;
pub mod wire;
