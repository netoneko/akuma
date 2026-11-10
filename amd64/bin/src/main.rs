//! The amd64 Akuma kernel. The kernel is `akuma-kernel-amd64` (`amd64/src`); this is its
//! entry symbols, `kmain` and `kmain_mb2`, which `akuma-boot-amd64`'s `boot.s` calls.

#![no_std]
#![no_main]

// The machine half is linked for its features and its `#[global_allocator]`/`#[panic_handler]`;
// the entry symbols reach it through the front.
use akuma_kernel_amd64 as _;

akuma_kernel::entry!();
