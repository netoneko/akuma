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


//! # Why this is a crate
//!
//! Extracted from `akuma-exec` on 2026-08-30
//! (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.2). It was already one-way — it
//! referenced nothing in `process` or `threading` — and had exactly three
//! internal callers, all in `process::image`. What it buys is testability of the
//! two open bugs that live here: the lazy segment-boundary zeroing
//! (`docs/archive/LAZY_ELF_SEGMENT_BOUNDARY_ZEROING.md`) and the RELR shared-page
//! accumulation (`docs/archive/INSTR_ABORT_RELR_WEDGE.md`, still open). Both are
//! pure functions of a byte buffer and a VA plan, and neither could be exercised
//! without booting a VM.
//!
//! # The four hooks
//!
//! [`VfsHooks`] is the whole of this crate's upward surface: three ways to read a
//! file, and the shared-SMP flag that decides whether the interpreter's whole-file
//! read drops the BKL. Registered once by `akuma_exec::init`.
//!
//! A stub registration here turns inode-backed reads into silent zeros — that is
//! the `[FILL-SHORT/prefault]` signature of the self-host ICE — so these
//! **`require()`, not `get()`**: an unregistered VFS at load time is a boot-order
//! bug, not a condition to degrade through.

#![cfg_attr(not(test), no_std)]
// Zero `unsafe` as of 2026-08-30. The crate had six raw frame writes — a PT_LOAD
// segment copy, an SHT_RELA value, and four `UserStack` pushes — all of them
// `phys_to_virt` + a pointer store into a page that `UserAddressSpace` had just
// mapped. They are now `UserAddressSpace::write_page_bytes`, where `&mut self` on
// the address space is a real exclusivity proof, and the page/offset arithmetic
// they open-coded is `akuma_mmap::span`, host-tested. `forbid`, not `deny`, so no
// module can opt back in. See `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §8.7-8.8.
#![forbid(unsafe_code)]
// Inherited verbatim from `akuma-exec`'s crate-root `allow` list. This code did
// not change when it moved out on 2026-08-30, so its lint posture must not
// either — a split that silently turns 20 warnings on is not behaviour-preserving,
// and fixing them in the same commit would hide the move in the diff. Tighten
// these deliberately, later, one lint at a time.
#![allow(
    clippy::too_long_first_doc_paragraph,
    clippy::must_use_candidate,
    clippy::redundant_pub_crate,
    clippy::unnecessary_cast,
    clippy::ptr_as_ptr,
    clippy::verbose_bit_mask,
    clippy::single_match_else,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default,
    clippy::manual_div_ceil,
    clippy::cast_lossless,
    clippy::vec_init_then_push,
    clippy::unused_self,
    clippy::semicolon_if_nothing_returned,
    clippy::needless_continue,
    clippy::manual_is_multiple_of,
    clippy::identity_op,
    clippy::collapsible_if,
    clippy::cast_possible_wrap,
    clippy::missing_safety_doc,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::cast_ptr_alignment,
    clippy::items_after_statements,
    clippy::redundant_else,
    clippy::option_if_let_else,
    clippy::needless_range_loop,
    clippy::collapsible_else_if,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::implicit_saturating_sub,
    clippy::manual_let_else,
    clippy::let_and_return,
    clippy::use_self,
    clippy::missing_const_for_fn,
    clippy::struct_field_names,
    clippy::needless_pass_by_value,
    clippy::if_not_else,
    clippy::match_same_arms,
    dead_code,
    unused_unsafe,
    unused_mut,
)]

extern crate alloc;

/// The tree's one heap-free print macro.
pub use akuma_primitives::safe_print;

use akuma_primitives::Registered;

/// The file-reading and BKL-policy callbacks the loader needs from the kernel.
///
/// Mirrors the four `ExecRuntime` fields it used to read directly. See the module
/// header for why these `require()` rather than degrade.
#[derive(Clone, Copy)]
pub struct VfsHooks {
    /// Read a whole file into a fresh `Vec`.
    pub read_file: fn(&str) -> Result<alloc::vec::Vec<u8>, i32>,
    /// Read `buf.len()` bytes at `offset`. Returns bytes read (may be short).
    pub read_at: fn(&str, usize, &mut [u8]) -> Result<usize, i32>,
    /// Resolve a path to `(mount id, inode)` — the pair that actually names a
    /// file. An inode alone is ambiguous across two mounted filesystems, which is
    /// what makes it unsafe as a page-cache key.
    pub resolve_file_id: fn(&str) -> Result<(u32, u32), i32>,
    /// Whether the shared-kernel SMP execve/ELF-load BKL drop is enabled. Always
    /// `false` off `smp-shared`, where the `bkl` calls are no-ops anyway.
    pub exec_bkl_drop_enabled: fn() -> bool,
}

static VFS: Registered<VfsHooks> = Registered::new(
    "akuma-elf: VfsHooks not registered — call akuma_exec::init() first",
);

/// Register the VFS callbacks. Call once, from `akuma_exec::init`.
pub fn register_vfs_hooks(h: VfsHooks) {
    VFS.register(h);
}

/// The VFS table. Panics if unregistered — see the module header.
#[inline]
pub(crate) fn vfs() -> VfsHooks {
    VFS.require()
}

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
