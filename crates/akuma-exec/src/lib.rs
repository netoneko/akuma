#![no_std]
#![feature(never_type)]
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
pub mod mmu;
pub mod elf_loader;
pub mod threading;
pub mod process;
pub mod box_registry;

pub use runtime::{ExecRuntime, ExecConfig, PhysFrame, FrameSource};

/// Initialize the exec subsystem.
///
/// # Arguments
/// * `rt` — Kernel runtime callbacks
/// * `cfg` — Kernel configuration constants
pub fn init(rt: ExecRuntime, cfg: ExecConfig) {
    runtime::register(rt, cfg);
}
