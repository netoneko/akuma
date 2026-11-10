//! The amd64 kernel's entry symbols; the kernel is the `akuma_kernel_amd64` library.
//!
//! `akuma-boot-amd64`'s `boot.s` calls these by name once it reaches long mode: `kmain`
//! for a PVH boot (QEMU, Firecracker, the in-tree KVM guest) with the `hvm_start_info`
//! pointer, `kmain_mb2` for GRUB/multiboot2 (the bare-metal path) with the information
//! block's physical address. Both arrive in `%rdi` per System V.

#![no_std]
#![no_main]

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn kmain(hvm_start_info: u64) -> ! {
    akuma_kernel_amd64::kmain(hvm_start_info)
}

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(info_phys: u64) -> ! {
    akuma_kernel_amd64::kmain_mb2(akuma_primitives::addr::PhysAddr::new(info_phys))
}
