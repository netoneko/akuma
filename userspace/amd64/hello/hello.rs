//! The first program the amd64 kernel loads from an ELF image.
//!
//! Built by `amd64/build.rs` with `rustc --target x86_64-unknown-none` and
//! linked by `amd64/user/user.ld` at `0x40_0000` — a genuine static ELF64 with
//! real program headers, not a byte blob the kernel assembled for itself. That
//! distinction is the point of the stage: a loader tested only against an image
//! the same kernel built is testing its own encoder.
//!
//! # Why it reports through the exit status
//!
//! Everything this program checks is a property of *the loader*, and the loader
//! is what would be broken. Printing a verdict would go through `write`, which
//! only proves the program ran; the exit status is read by the kernel's
//! self-test and compared against a value computed there, so a wrong load fails
//! the boot rather than scrolling past.
//!
//! Each bit stands for one thing the loader had to get right:
//!
//! | bit | claim |
//! |---|---|
//! | 0 | `.data` arrived with its linked contents (file bytes copied) |
//! | 1 | `.bss` is zero (`p_memsz > p_filesz` zero-filled) |
//! | 2 | `.data` is writable (a `PF_W` segment is mapped writable) |
//! | 3 | `argc` is what the kernel put on the stack |
//! | 4 | `argv[0]` points at a readable string with the expected bytes |
//! | 5 | the auxiliary vector carries `AT_PAGESZ` = 4096 |
//!
//! Bits 3-5 test the *initial stack*, which is the half of "loading a program"
//! that has nothing to do with parsing the file and everything to do with the
//! System V ABI the program was compiled against.

#![no_std]
#![no_main]

/// Linux x86_64 syscall numbers. Spelled out rather than imported: this program
/// is compiled standalone by `rustc`, not as a member of the workspace, so it
/// cannot see `akuma-syscalls-abi`. Three constants is the whole cost, and the
/// kernel side dispatches through that crate — which is where the identity
/// actually has to be shared.
const SYS_WRITE: u64 = 1;
const SYS_EXIT_GROUP: u64 = 231;

/// `AT_PAGESZ`, the one auxv entry this program looks for.
const AT_PAGESZ: u64 = 6;
const AT_NULL: u64 = 0;

/// Lives in `.rodata`: proves a read-only, non-writable segment is readable.
static MSG: &[u8] = b"    [elf] loaded from a real ELF image\n";

/// Lives in `.data`: a non-zero initialiser, so a loader that mapped the
/// segment but never copied its file bytes reads 0 here instead.
static mut DATA_MARK: u64 = 0x5A5A_5A5A_5A5A_5A5A;

/// Lives in `.bss`: `p_memsz > p_filesz`, so a loader that copies file bytes but
/// forgets to zero the tail reads whatever the recycled frame held.
///
/// Deliberately larger than a page (4096 u64s = 32 KiB) so the zero-fill has to
/// span several pages rather than getting away with clearing one.
static mut BSS_AREA: [u64; 4096] = [0; 4096];

/// `syscall` with three arguments.
///
/// `rcx` and `r11` are clobbered by the instruction itself — the CPU puts the
/// return address in one and the flags in the other — so both are declared
/// `lateout`. Omitting them lets the compiler keep a live value in either
/// across the call and read back garbage.
#[inline(always)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
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

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    /* The System V process entry state: rsp points at argc, and there is no
     * return address — this is not a call. Hand rsp over as the first argument
     * before aligning, because the whole argv/envp/auxv block is addressed
     * relative to it. */
    mov rdi, rsp
    and rsp, -16
    call rust_start
    /* rust_start never returns; this is a guard, not a fallthrough. */
1:  jmp 1b
"#
);

/// # Safety
/// `sp` must be the System V initial stack pointer: `argc`, then `argc`
/// pointers, a NULL, the environment, a NULL, and the auxiliary vector.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(sp: *const u64) -> ! {
    let mut status: u64 = 0;

    // .data survived the load with its linked contents.
    // SAFETY: single-threaded, and this is the only accessor.
    let data = unsafe { core::ptr::read_volatile(&raw const DATA_MARK) };
    if data == 0x5A5A_5A5A_5A5A_5A5A {
        status |= 1 << 0;
    }

    // .bss is zero, all of it, not just the first page.
    // SAFETY: as above.
    let bss_clean = unsafe {
        let base = &raw const BSS_AREA as *const u64;
        (0..4096).all(|i| core::ptr::read_volatile(base.add(i)) == 0)
    };
    if bss_clean {
        status |= 1 << 1;
    }

    // .data is writable — the PF_W bit reached the page table.
    // SAFETY: as above.
    let wrote_back = unsafe {
        core::ptr::write_volatile(&raw mut DATA_MARK, 0xA5A5_A5A5_A5A5_A5A5);
        core::ptr::read_volatile(&raw const DATA_MARK) == 0xA5A5_A5A5_A5A5_A5A5
    };
    if wrote_back {
        status |= 1 << 2;
    }

    // SAFETY: the caller's obligation, discharged by the kernel's stack builder.
    let argc = unsafe { core::ptr::read_volatile(sp) };
    if argc == 1 {
        status |= 1 << 3;
    }

    if argc == 1 {
        // SAFETY: argv[0] is in bounds given argc == 1, and the kernel wrote a
        // NUL-terminated string there.
        let argv0 = unsafe { core::ptr::read_volatile(sp.add(1)) } as *const u8;
        if !argv0.is_null() {
            // SAFETY: as above; the compare stops at the first mismatch, and a
            // NUL in the expected bytes stops it before running off the string.
            let matches = unsafe {
                b"hello"
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| core::ptr::read_volatile(argv0.add(i)) == b)
                    && core::ptr::read_volatile(argv0.add(5)) == 0
            };
            if matches {
                status |= 1 << 4;
            }
        }
    }

    // Walk past argv and envp to the auxiliary vector.
    // SAFETY: the layout is the caller's obligation; each NULL terminator is
    // read before the pointer advances past it.
    let pagesz = unsafe {
        let mut p = sp.add(1 + argc as usize + 1); // envp[0]
        while core::ptr::read_volatile(p) != 0 {
            p = p.add(1);
        }
        p = p.add(1); // first auxv key
        let mut found = 0u64;
        loop {
            let key = core::ptr::read_volatile(p);
            if key == AT_NULL {
                break;
            }
            let val = core::ptr::read_volatile(p.add(1));
            if key == AT_PAGESZ {
                found = val;
            }
            p = p.add(2);
        }
        found
    };
    if pagesz == 4096 {
        status |= 1 << 5;
    }

    // SAFETY: fd 1 is the kernel's serial console, and MSG is in this image.
    unsafe {
        syscall3(SYS_WRITE, 1, MSG.as_ptr() as u64, MSG.len() as u64);
        syscall3(SYS_EXIT_GROUP, status, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: the last thing this program does.
    unsafe { syscall3(SYS_EXIT_GROUP, 0xFF, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
