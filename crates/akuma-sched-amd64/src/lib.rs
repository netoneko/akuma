//! The machine half of the amd64 scheduler.
//!
//! `akuma-threading` is the scheduler (state machine, picker, park/wake, `ON_CPU`
//! gating). What it is not allowed to know is performed here, once, for every amd64
//! kernel: which page-table root a thread runs in, where its ring-3 trap stack is,
//! userspace's `FS`/`GS` bases, its `xsave` area, which core it may run on - the
//! things an [`akuma_threading::X86ArchHooks`] switch has to install on the CPU.
//!
//! This crate was `amd64/src/sched.rs`'s hook section (`docs/archive/AKUMA_AMD64_SCHED_
//! CRATE.md`). It is generic over a [`Platform`] because the two kernels differ in exactly
//! the things a switch touches: the per-thread user-context record (`UserCtx` on the
//! metal kernel, the syscall bands on Taoxichuan), how a page-table root is made current,
//! and which per-CPU block holds "the slot running now". The hook *bodies* are shared;
//! the `Platform` is the seam, and its diagnostic methods default to nothing.
//!
//! **Never** put kernel-specific state in this crate: if a change wants a `Process` or a
//! `UserCtx` layout, the seam is wrong - add a `Platform` method instead.

#![no_std]
// amd64 only: the asm below does not exist elsewhere, and a host build gets an empty crate.
#![cfg(target_arch = "x86_64")]
// asm lives here and nowhere else in the scheduler; the unsafe budget counts this crate.
#![allow(clippy::too_long_first_doc_paragraph)]

extern crate alloc;

mod fx;
mod machine;
mod platform;

pub use fx::{FxArea, XCR0_ENABLED, XSAVE_ON, fpu_reset_live, fpu_restore_live, fpu_save_live, mxcsr_mask};
pub use machine::{Machine, Stacks};
pub use platform::{Platform, can_run, mark_idle, prepare_task_slot, reserve_cores, reserved_cores, switch_to, NO_CPU};
