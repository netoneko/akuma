//! The syscall front: an `svc` from EL0 carries the asm-generic number in `x8`
//! and its arguments in `x0`-`x5`, and gets its answer in `x0`.
//!
//! In order: a channel-less process's stdio to the console
//! (`akuma_runtime_tokio::console`); the calls this kernel serves itself (exit,
//! the memory arms, which are `akuma_tokio_kernel_core::mm`'s, as on amd64);
//! then `akuma-syscalls-glue`, by the number as it came. On amd64 the
//! allow-listed calls go to glue and the fd family to the x86_64 ABI layer; on
//! AArch64 glue's arms *are* the ABI layer, so it takes the rest directly.
//! `clone`, `execve` and `mremap` are `ENOSYS` until the process step (A5.6c).

use akuma_primitives::safe_print;
use akuma_syscalls_linux::nr;
use akuma_tokio_kernel_core::mm;
use akuma_vectors_aarch64::Frame;

use crate::mm::Aarch64;

const ENOSYS: u64 = 38;

const fn e(code: u64) -> u64 {
    0u64.wrapping_sub(code)
}

/// Serve the `svc` that trapped into `f`.
pub fn svc(f: &mut Frame) {
    let a = [f.x[0], f.x[1], f.x[2], f.x[3], f.x[4], f.x[5]];
    f.x[0] = dispatch(f.x[8], a);
}

fn dispatch(n: u64, a: [u64; 6]) -> u64 {
    match n {
        nr::WRITE | nr::WRITEV => {
            if let Some(r) = akuma_runtime_tokio::console::stdio_write(n == nr::WRITEV, a, crate::console_putb) {
                return r;
            }
        }
        nr::EXIT | nr::EXIT_GROUP => crate::proc::exit(a[0] & 0xFF),
        nr::BRK => return mm::brk_arm::<Aarch64>(a[0]),
        nr::MMAP => {
            let anon = a[3] & u64::from(akuma_syscalls_linux::flags::map::MAP_ANONYMOUS) != 0;
            let file = if anon { None } else { crate::proc::fd_inode(a[4]) };
            return mm::mmap_arm::<Aarch64>(a[0], a[1], a[2], a[3], file, a[5]);
        }
        nr::MPROTECT => return mm::mprotect_arm::<Aarch64>(a[0], a[1], a[2]),
        nr::MUNMAP => return mm::munmap_arm::<Aarch64>(a[0], a[1]),
        nr::MSYNC => return mm::msync_arm::<Aarch64>(a[0], a[1]),
        nr::MADVISE => return mm::madvise_arm::<Aarch64>(a[0], a[1], a[2]),
        nr::CLONE | nr::EXECVE | nr::MREMAP => {
            safe_print!(64, "[nosys] {} (aarch64: the process step)\n", n);
            return e(ENOSYS);
        }
        _ => {}
    }
    if let Some(r) = akuma_runtime_tokio::serve_shared(n, a) {
        return r;
    }
    akuma_syscalls_glue::handle_syscall(n, &a)
}
