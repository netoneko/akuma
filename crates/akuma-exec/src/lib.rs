#![cfg_attr(not(test), no_std)]
// `never_type` became stable in 1.100.0-nightly; the attribute is now a warning.
#![feature(allocator_api)]
#![allow(
    clippy::future_not_send,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::uninlined_format_args,
    clippy::cast_ptr_alignment,
    clippy::items_after_statements,
    clippy::significant_drop_in_scrutinee,
    clippy::too_many_lines,
    clippy::use_self,
    clippy::struct_field_names,
    clippy::struct_excessive_bools,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::unnecessary_cast,
    clippy::redundant_else,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::declare_interior_mutable_const,
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::unused_self,
    clippy::vec_init_then_push,
    clippy::pub_underscore_fields,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::needless_pass_by_value,
    clippy::if_not_else,
    clippy::manual_div_ceil,
    clippy::option_if_let_else,
    clippy::match_wildcard_for_single_variants,
    clippy::cast_possible_wrap,
    clippy::redundant_closure_for_method_calls,
    clippy::iter_without_into_iter,
    clippy::collapsible_if,
    clippy::significant_drop_tightening,
    clippy::ref_as_ptr,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::match_same_arms,
    clippy::redundant_closure,
    clippy::manual_is_variant_and,
    clippy::missing_safety_doc,
    clippy::let_and_return,
    clippy::manual_range_contains,
    clippy::empty_line_after_doc_comments,
    clippy::inline_always,
    clippy::bool_to_int_with_if,
    clippy::manual_saturating_arithmetic,
    clippy::cast_lossless,
    clippy::option_map_or_none,
    clippy::redundant_field_names,
    clippy::let_underscore_untyped,
    unused_unsafe,
    unused_mut,
    clippy::implicit_saturating_sub,
    clippy::manual_let_else,
    clippy::verbose_bit_mask,
    clippy::ptr_cast_constness,
    clippy::derive_partial_eq_without_eq,
    clippy::or_fun_call,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::identity_op,
    clippy::while_let_loop,
    clippy::collapsible_else_if,
    clippy::needless_continue,
    clippy::inherent_to_string,
    clippy::manual_find,
    clippy::manual_is_multiple_of,
    clippy::eq_op,
    clippy::doc_overindented_list_items,
    clippy::map_unwrap_or,
    clippy::used_underscore_binding,
    clippy::branches_sharing_code,
    clippy::doc_comment_double_space_linebreaks,
    clippy::no_effect_underscore_binding,
    clippy::unwrap_or_default,
    clippy::should_implement_trait,
)]

extern crate alloc;

pub mod runtime;
/// Spinlocks and the recursive Big Kernel Lock — **moved to `akuma-bkl` on
/// 2026-08-30** (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.4), together with
/// `bkl` and the host model checker that proves the protocol. Re-exported under
/// the old paths so every `crate::sync::…` / `akuma_exec::sync::…` call site
/// resolves unchanged.
pub use akuma_bkl::sync;
/// The BKL enter/leave protocol and its per-thread dropped-window ledger — see
/// [`sync`] for why this now lives in `akuma-bkl`.
pub use akuma_bkl::bkl;

/// Shared host-test scaffolding (stub `ExecRuntime`/`ExecConfig` registration).
#[cfg(test)]
pub mod test_support;
/// Page tables, `UserAddressSpace`, ASIDs, TLB maintenance and the per-core TTBR
/// free gate — **moved to `akuma-mmu` on 2026-08-30**
/// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.5). It carried 41% of this
/// crate's `unsafe` budget at its lowest test coverage; concentrating it is the
/// `akuma-net-nic` move. Re-exported under the old path so every
/// `crate::mmu::…` / `akuma_exec::mmu::…` call site resolves unchanged.
///
/// `user_access` did **not** go with it — it moved *up*, to [`process::user_access`],
/// because its eight process references were the whole of the old `mmu <-> process`
/// cycle.
pub use akuma_mmu as mmu;
/// The ELF loader — **moved to `akuma-elf` on 2026-08-30**
/// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.2). Re-exported under the old
/// name so `process::image`'s three call sites and
/// `akuma_exec::elf_loader::INTERP_BASE` in `src/exceptions.rs` are unchanged.
pub use akuma_elf as elf_loader;
pub mod threading;
pub mod alarms;
pub mod process;
/// The box (container) registry — **moved to `akuma-isolation` on 2026-08-30**
/// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.1), where it joins the mount and
/// network namespaces it has always sat on top of and gains that crate's
/// `#![forbid(unsafe_code)]`. Re-exported under the old path so every
/// `crate::box_registry::…` and `akuma_exec::process::box_*` call site resolves
/// unchanged.
pub use akuma_isolation::box_registry;
#[cfg(target_os = "none")]
pub mod kernel_tests;

pub use runtime::{ExecRuntime, ExecConfig, PhysFrame, FrameSource};

/// The tree's one heap-free print macro, re-exported so this crate's ~66
/// existing `crate::safe_print!(…)` call sites resolve unchanged. The
/// definition, and the census of the copies it replaced, are in
/// `akuma_primitives::console`.
pub use akuma_primitives::safe_print;

/// Initialize the exec subsystem.
///
/// # Arguments
/// * `rt` — Kernel runtime callbacks
/// * `cfg` — Kernel configuration constants
pub fn init(rt: ExecRuntime, cfg: ExecConfig) {
    runtime::register(rt, cfg);
    // `akuma-bkl`'s single upward dependency: the scheduler's yield entry point.
    // Registered here rather than in `threading::init` because `sync::lock_bounded`
    // is reachable long before the thread pool exists — see `akuma_bkl`'s module
    // header for why the hook degrades to a spin hint instead of panicking.
    akuma_bkl::set_yield_hook(threading::yield_now);
    // `akuma-mmu`'s whole upward surface: the two questions the TTBR free gate
    // has to ask the scheduler. Without these the gate degrades to "no saved
    // context conflicts", which is correct only before any thread has run — so
    // register them here, alongside the runtime table, not lazily.
    akuma_mmu::register_sched_hooks(akuma_mmu::SchedHooks {
        any_saved_ctx_on_l0: threading::any_saved_ctx_on_l0,
        note_current_expected_l0: threading::note_current_expected_l0,
    });
    // `akuma-elf`'s four VFS callbacks, forwarded from the same `ExecRuntime`
    // fields the loader used to read directly. These `require()` at use — a stub
    // registration turns inode-backed reads into silent zeros, which is the
    // `[FILL-SHORT/prefault]` self-host ICE.
    akuma_elf::register_vfs_hooks(akuma_elf::VfsHooks {
        read_file: rt.read_file,
        read_at: rt.read_at,
        resolve_file_id: rt.resolve_file_id,
        exec_bkl_drop_enabled: rt.exec_bkl_drop_enabled,
    });
}
