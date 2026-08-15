//! ELF Loader
//!
//! Parses and loads ELF binaries into user address space.
//!
//! Four files, split along the axes the loader actually has:
//!
//! * [`source`] — *where the bytes come from* (`ElfSource`: an in-heap image or
//!   a path read a piece at a time) and all ELF parsing, which goes through the
//!   `elf` 0.7 crate.
//! * [`load`] — *how PT_LOAD segments reach memory* (`MapStrategy`: eager copy
//!   or demand-paged lazy regions), plus relocations.
//! * [`interp`] — the dynamic linker, mapped into the same address space.
//! * [`stack`] — the initial user stack (argv/envp/auxv) and the entry points
//!   `ProcessImage` calls.
//!
//! The two axes used to be one: loaded-from-a-path implied deferred mapping and
//! loaded-from-bytes implied eager, because each `X_from_path` function was a
//! copy-paste of `X`. That produced two independent ELF parsers — one vetted,
//! one hand-rolled at literal byte offsets — with which one validated your
//! dynamic linker decided by binary size. See
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3.

pub mod types;

mod interp;
mod load;
mod source;
mod stack;

pub use types::*;

pub use load::{LoadedElf, load_elf, load_elf_from_path};
pub use stack::{
    LoadedWithStack, UserStack, load_elf_with_stack, load_elf_with_stack_from_path,
    setup_linux_stack,
};
