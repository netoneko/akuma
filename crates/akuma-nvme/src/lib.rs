//! Pure NVMe logic for the amd64 bare-metal target — everything the driver
//! needs that is not an MMIO access or a DMA buffer.
//!
//! # Why this exists
//!
//! ryzen (`overlays/ryzen/`, `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`)
//! has one disk, an SK hynix NVMe SSD (`1c5c:1d59`), holding Pop!_OS, the ESP
//! and the partition Akuma is to own. A driver for it is two halves, the same
//! split `akuma-xhci` made: the `unsafe` register file and DMA memory stay in
//! `amd64/src/nvme.rs`, and everything that can be decided from bytes lives
//! here, under `forbid(unsafe_code)`, with host tests:
//!
//! | module | decides |
//! |---|---|
//! | [`regs`] | `CAP`/`CC`/`CSTS`/`AQA` decode and encode, doorbell offsets |
//! | [`cmd`] | the 64-byte submission entries, completion decode, the phase bit |
//! | [`identify`] | Identify Controller / Namespace: LBA size, size, transfer limit |
//! | [`prp`] | PRP1/PRP2/PRP-list for a physically contiguous buffer |
//! | [`chunk`] | byte `(offset, len)` → whole-block commands, read-modify-write ends |
//! | [`window`] | the partition bound **every** access is checked against |
//! | [`gpt`] | the GUID partition table, both CRCs verified |
//!
//! # The bound is the point
//!
//! This driver writes to the disk that holds the user's operating system. The
//! partition [`window::Window`] is not a convenience: every read and write the
//! kernel issues is translated through it, and an access that does not fit is
//! refused before a command is built. The GPT it comes from is accepted only
//! with a valid header CRC **and** a valid entry-array CRC, so a misread
//! sector cannot become a window over someone else's data.

#![no_std]
#![forbid(unsafe_code)]

pub mod chunk;
pub mod cmd;
pub mod gpt;
pub mod identify;
pub mod prp;
pub mod regs;
pub mod window;

#[cfg(test)]
mod tests;
