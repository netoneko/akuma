//! The Akuma kernel binary, for both architectures.
//!
//! One package, two machine halves: the target selects which one this bin links
//! (`Cargo.toml`'s per-target dependency tables), and each half's module holds only what a
//! crate root must declare. Everything else is a crate.
//!
//! - AArch64 (`aarch64-unknown-none`, the default target): `aarch64.rs`, over `akuma-kernel-glue`.
//! - amd64 (`x86_64-unknown-none`): `amd64.rs`, over `akuma-kernel-amd64`.

#![no_std]
#![no_main]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), feature(alloc_error_handler))]

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod amd64;
