//! The smallest init that proves a guest ran something: print one line, exit.
//!
//! Not part of the kernel build (`amd64/build.rs` builds the loader self-check
//! `hello`, which reports through its exit status and prints nothing). This one
//! is compiled by `scripts/kvm/akuma_hello_rootfs.sh` into a 2 MiB ext2 whose only
//! file is `/bin/hello`, the whole root of the "Akuma on Akuma, hello world" run
//! (`docs/archive/AKUMA_KVM_AKUMA_GUEST.md`). The hypervisor stops the run when it
//! sees the end of the line below on the guest's console.

#![no_std]
#![no_main]

const SYS_WRITE: u64 = 1;
const SYS_EXIT_GROUP: u64 = 231;

/// The line. `kvm/linux.rs` stops the run on its last words, `run by Akuma`, so the
/// whole line reaches the console first.
const LINE: &[u8] = b"Hello, world! This is the init of a guest of Akuma, run by Akuma\n";

/// `syscall` with three arguments.
///
/// # Safety
/// The caller passes a valid Linux x86_64 syscall number and arguments.
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    // SAFETY: the Linux x86_64 syscall ABI; `rcx` and `r11` are clobbered by
    // the instruction itself.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // SAFETY: stdout is the console; the buffer is a static of the stated length.
    unsafe {
        syscall3(SYS_WRITE, 1, LINE.as_ptr() as u64, LINE.len() as u64);
        syscall3(SYS_EXIT_GROUP, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
