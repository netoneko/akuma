// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Cargo cannot
// host this per-crate — `[lints] workspace = true` and crate-local lints are
// mutually exclusive — so the ban is spelled here.
#![no_std]
#![forbid(unsafe_code)]
//! The unsafe-free half of `src/`, extracted 2026-09-01 so it can carry
//! `#![forbid(unsafe_code)]`.
//!
//! `console.rs` and `platform.rs` arrived 2026-09-01 from `akuma-kernel-glue`.
//! Neither holds any `unsafe` — `console`'s three PL011 accesses became one
//! call into `akuma-uart`, and `platform` is machine constants plus FDT
//! parsing — so both were only ever on the wrong side of the ban by accident
//! of where `src/` was cut. `console.rs` in particular is why
//! `akuma-kernel-glue` used to *report* as forbidding: `scripts/cloc_akuma.py`
//! marks a crate when ANY file in it carries the attribute, and that module
//! carried its own. The genuinely unsafe half — boot assembly, the secondary
//! trampoline, the entry symbols — is `akuma-entry`, which sits above this
//! crate and below `akuma-kernel-glue`.
//!
//! `src/main.rs` re-exports every module below as `pub use
//! akuma_kernel_core::x;` so the ~thousands of `crate::x::y` call sites across
//! the bin crate (including the test files, which are staying in `src/`)
//! resolve unchanged.

extern crate alloc;

pub use akuma_primitives::{safe_print, tprint};

pub mod akuma;
#[cfg(feature = "bkl-profile")]
pub mod bkl_profile;
pub mod config;
// The console — formatting, cross-core serialisation, line input. It carried a
// module-level `#![forbid(unsafe_code)]` inside `akuma-kernel-glue`; here the
// crate root forbids, so the module attribute is gone as redundant.
// `target_os = "none"` for the same reason as `timer` below: it reaches the
// PL011 through `akuma-uart`, which does not exist for a host build.
#[cfg(target_os = "none")]
pub mod console;
pub mod file_page_cache;
pub mod fs;
pub mod irq;
pub mod klog;
#[cfg(feature = "net-profile")]
pub mod nic_profile;
pub mod ntp_boot;
#[cfg(target_os = "none")]
pub mod platform;
pub mod pmm;
// `timer.rs` is the ISR + hardware-probe half of `akuma-timer`'s API (the IRQ
// handler, the boot-time WFI probe, `enable_timer_interrupts`) — none of it
// exists for a host build, `akuma-timer` itself gates all of it behind the
// same cfg (`crates/akuma-timer/src/lib.rs`), so a host `cargo check`/`cargo
// test` of this crate needs the same gate, matching `akuma-exec`'s
// `kernel_tests` module (`crates/akuma-exec/src/lib.rs`).
#[cfg(target_os = "none")]
pub mod timer;
