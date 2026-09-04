//! Does the kernel's file and memory syscall surface actually work from ring 3?
//!
//! Stage O's probe, and the reason it exists before the `libakuma` port: the
//! kernel's own `fd::smoke_test` calls those functions **from ring 0**, where a
//! user pointer is just a pointer and `SMAP` is not a question. This runs the
//! same operations across the privilege boundary, through the `syscall`
//! instruction, with the real x86_64 numbers — which is the only place a wrong
//! argument register or a missing `r10` shows up.
//!
//! It reports through the exit status, one bit per claim, for the reason
//! `hello.rs` does: the kernel's self-test compares the status against a value
//! computed there, so a wrong answer fails the boot rather than scrolling past.
//!
//! | bit | claim |
//! |---|---|
//! | 0 | `openat` returns a descriptor for a file that exists |
//! | 1 | `read` returns the file's first bytes |
//! | 2 | `lseek(SEEK_SET)` rewinds and the same bytes come back |
//! | 3 | `lseek(SEEK_END)` reports the size the kernel reported |
//! | 4 | reading at EOF returns 0, not an error |
//! | 5 | `fstat` reports that size too |
//! | 6 | `close` succeeds and the descriptor then fails |
//! | 7 | opening a missing path is `-ENOENT` |
//! | 8 | `mmap` returns usable, **zeroed** memory |
//! | 9 | that memory survives a write and read-back |
//! | 10 | `munmap` succeeds |
//! | 11 | a file-backed `mmap` is refused rather than served as zeroes |

#![no_std]
#![no_main]

// Linux x86_64 syscall numbers. Spelled out because this program is compiled
// standalone by `rustc`, not as a workspace member, so it cannot see
// `akuma-syscalls-abi` — the crate the *kernel* dispatches through.
const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_FSTAT: u64 = 5;
const SYS_LSEEK: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_OPENAT: u64 = 257;
const SYS_EXIT_GROUP: u64 = 231;

const AT_FDCWD: u64 = (-100i64) as u64;
const SEEK_SET: u64 = 0;
const SEEK_END: u64 = 2;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;
const NO_FD: u64 = (-1i64) as u64;

/// The file `amd64/mkdisk.sh` writes, and its exact size.
const PROBE_PATH: &[u8] = b"/probe.txt\0";
const PROBE_HEAD: &[u8] = b"AKUMA/amd64 ext2 probe";
const PROBE_SIZE: u64 = 6623;

/// `syscall` with six arguments.
///
/// `r10`, not `rcx`, for the fourth: the `syscall` instruction destroys `rcx`
/// (it puts the return address there), which is exactly why the Linux ABI
/// diverges from System V at that position. Getting this wrong is the classic
/// way a four-argument syscall reads garbage.
///
/// # Safety
/// Performs a syscall; the caller owns whatever it does.
#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a1, in("rsi") a2, in("rdx") a3,
            in("r10") a4, in("r8") a5, in("r9") a6,
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// # Safety
/// As [`syscall6`].
#[inline(always)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    unsafe { syscall6(nr, a1, a2, a3, 0, 0, 0) }
}

/// Is this return value an errno rather than a result?
///
/// The kernel returns errors as small negatives. Checking the top of the range
/// rather than `as i64) < 0` matters for `mmap`, whose successful return is an
/// address with the high bit clear but which could in principle be large.
fn is_err(ret: u64) -> bool {
    ret > (-4096i64) as u64
}

fn errno_of(ret: u64) -> i64 {
    -(ret as i64)
}

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b
"#
);

/// # Safety
/// `sp` must be the System V initial stack pointer.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(_sp: *const u64) -> ! {
    let mut status: u64 = 0;
    let mut buf = [0u8; 64];

    // SAFETY: every call below is a syscall this kernel implements; the buffers
    // are this program's own stack and static memory.
    unsafe {
        let fd = syscall6(SYS_OPENAT, AT_FDCWD, PROBE_PATH.as_ptr() as u64, 0, 0, 0, 0);
        if !is_err(fd) {
            status |= 1 << 0;

            let n = syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, PROBE_HEAD.len() as u64);
            if n == PROBE_HEAD.len() as u64
                && (0..PROBE_HEAD.len()).all(|i| buf[i] == PROBE_HEAD[i])
            {
                status |= 1 << 1;
            }

            // Rewind and re-read: the cursor must belong to the descriptor.
            if syscall3(SYS_LSEEK, fd, 0, SEEK_SET) == 0 {
                let n = syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, 5);
                if n == 5 && buf[0] == b'A' && buf[4] == b'A' {
                    status |= 1 << 2;
                }
            }

            if syscall3(SYS_LSEEK, fd, 0, SEEK_END) == PROBE_SIZE {
                status |= 1 << 3;
                // At EOF a read returns 0. Not an error: a reader that treated
                // it as one would never terminate.
                if syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, 16) == 0 {
                    status |= 1 << 4;
                }
            }

            let mut st = [0u8; 144];
            if syscall3(SYS_FSTAT, fd, st.as_mut_ptr() as u64, 0) == 0 {
                // st_size lives at offset 48 in the x86_64 layout.
                let size = u64::from_le_bytes([
                    st[48], st[49], st[50], st[51], st[52], st[53], st[54], st[55],
                ]);
                if size == PROBE_SIZE {
                    status |= 1 << 5;
                }
            }

            if syscall3(SYS_CLOSE, fd, 0, 0) == 0
                && is_err(syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, 4))
            {
                status |= 1 << 6;
            }
        }

        let missing = b"/no-such-file\0";
        let r = syscall6(SYS_OPENAT, AT_FDCWD, missing.as_ptr() as u64, 0, 0, 0, 0);
        if is_err(r) && errno_of(r) == 2 {
            status |= 1 << 7;
        }

        // Anonymous memory, which is what a userspace allocator needs.
        let len = 8192u64;
        let p = syscall6(
            SYS_MMAP, 0, len, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, NO_FD, 0,
        );
        if !is_err(p) {
            let mem = p as *mut u8;
            // Fresh anonymous memory must be zero. A recycled frame handed over
            // without zeroing is an information leak from whoever had it last.
            if (0..len).all(|i| core::ptr::read_volatile(mem.add(i as usize)) == 0) {
                status |= 1 << 8;
            }
            // Write across a page boundary and read back, so the check covers
            // more than the first page's mapping.
            core::ptr::write_volatile(mem, 0xA5);
            core::ptr::write_volatile(mem.add(4096), 0x5A);
            core::ptr::write_volatile(mem.add(len as usize - 1), 0x3C);
            if core::ptr::read_volatile(mem) == 0xA5
                && core::ptr::read_volatile(mem.add(4096)) == 0x5A
                && core::ptr::read_volatile(mem.add(len as usize - 1)) == 0x3C
            {
                status |= 1 << 9;
            }
            if syscall3(SYS_MUNMAP, p, len, 0) == 0 {
                status |= 1 << 10;
            }
        }

        // A file-backed mapping must be refused, not served as zeroed anonymous
        // memory — that would look like a working call and hand back a file
        // full of zeros.
        let r = syscall6(SYS_MMAP, 0, 4096, PROT_READ, MAP_PRIVATE, 0, 0);
        if is_err(r) {
            status |= 1 << 11;
        }

        let msg = b"    [fdprobe] file and memory syscalls exercised from ring 3\n";
        syscall3(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
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
