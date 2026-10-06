//! Battery and AC-adapter state, decoded from the embedded controller's memory
//! window, and the text `/proc/power` serves.
//!
//! `docs/archive/AKUMA_ACPI_POWER.md` is the design; this header is what a
//! reader of the code needs.
//!
//! # Why there is no AML interpreter
//!
//! A battery on a laptop is not a table of values. It is control methods
//! (`_BIX`, `_BST`) in the DSDT that read embedded-controller registers. Running
//! them needs an AML interpreter and the ACPI namespace under it — thousands of
//! lines whose failures are silent. The registers themselves are plain bytes: on
//! the Ryzen laptop the EC mirrors them into a memory window the DSDT names with
//! an `OperationRegion (ERAM, SystemMemory, …)`. So this crate does the two
//! small things that are enough: [`aml::find_region`] reads that one declaration
//! out of the table bytes, and [`ec::Reading::decode`] turns the window into
//! numbers. Per-machine knowledge is confined to the field layout in [`ec`].
//!
//! # No allocation, no I/O, no `unsafe`
//!
//! Everything takes slices and writes into caller buffers. The kernel owns the
//! mapping and the volatile reads; this crate cannot touch hardware, which is
//! what lets it be tested against bytes measured on the real machine.

#![no_std]
#![forbid(unsafe_code)]

pub mod aml;
pub mod ec;
pub mod render;
pub mod sim;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;
