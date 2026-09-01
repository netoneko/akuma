// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Cargo cannot
// host this per-crate — `[lints] workspace = true` and crate-local lints are
// mutually exclusive — so the ban is spelled here.
#![no_std]
#![forbid(unsafe_code)]
//! The unsafe-free half of `src/`, extracted 2026-09-01 so it can carry
//! `#![forbid(unsafe_code)]`. The other half — `main.rs`, `smp_shared.rs`,
//! `console.rs`, `boot.rs`, `platform.rs` — has real `unsafe` and stays in the
//! bin crate for now (a future `akuma-kernel-glue` crate is the planned home).
//!
//! Three of the eleven modules here (`syscall.rs`, `vfs.rs`, `rump_proxy.rs`
//! in the original `src/`) were left OUT of this move even though they carry
//! no `unsafe` themselves: they call into `smp_shared`/`console`, which are on
//! the wrong side of this crate's `forbid` — moving them here would have made
//! this crate depend on the very half it exists to keep clean. They stay in
//! `src/` until that half moves too.
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
pub mod file_page_cache;
pub mod fs;
pub mod irq;
pub mod klog;
#[cfg(feature = "net-profile")]
pub mod nic_profile;
pub mod ntp_boot;
pub mod pmm;
// `timer.rs` is the ISR + hardware-probe half of `akuma-timer`'s API (the IRQ
// handler, the boot-time WFI probe, `enable_timer_interrupts`) — none of it
// exists for a host build, `akuma-timer` itself gates all of it behind the
// same cfg (`crates/akuma-timer/src/lib.rs`), so a host `cargo check`/`cargo
// test` of this crate needs the same gate, matching `akuma-exec`'s
// `kernel_tests` module (`crates/akuma-exec/src/lib.rs`).
#[cfg(target_os = "none")]
pub mod timer;
