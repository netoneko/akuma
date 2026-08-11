//! Library half of `box`, holding the parts that are pure enough to unit-test
//! on the host.
//!
//! The binary (`main.rs`) is `no_std` + `no_main` for `aarch64-unknown-none` and
//! links `libakuma`, whose `#[panic_handler]` / `#[global_allocator]` make it
//! unlinkable against a std target — so the binary itself can never be
//! host-tested. Anything that is just logic over strings and bytes belongs here
//! instead, behind a feature gate that leaves `libakuma` out:
//!
//! ```text
//! cargo test -p box --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! The lib target is named `boxlib`, not `box`: `box` is a reserved Rust
//! keyword and cannot name a crate. The binary is still `box`.
//!
//! The split runs along the I/O line. Reading `/proc/boxes`, downloading a
//! blob, mounting an overlay and spawning a process stay in the binary; the
//! decisions those steps are driven by — which platform manifest to fetch,
//! which digests an image is made of, what argv an image config implies, which
//! box a name refers to — live here.
//!
//! `no_std` is dropped under `cfg(test)` so the test harness can link std;
//! `alloc` is available either way.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod boxes;
pub mod json;
pub mod manifest;
pub mod oci_ref;
pub mod paths;
pub mod spec;
pub mod sys;
