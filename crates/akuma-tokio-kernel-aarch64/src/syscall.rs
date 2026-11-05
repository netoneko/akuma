//! The syscall front: an `svc` from EL0 carries the asm-generic number in `x8`
//! and its arguments in `x0`-`x5`, and gets its answer in `x0`.
//!
//! In order: a channel-less process's stdio to the console
//! (`akuma_runtime_tokio::console`); the calls this kernel serves itself (exit,
//! the memory arms, which are `akuma_tokio_kernel_core::mm`'s, as on amd64);
//! then `akuma-syscalls-glue`, by the number as it came. On amd64 the
//! allow-listed calls go to glue and the fd family to the x86_64 ABI layer; on
//! AArch64 glue's arms *are* the ABI layer, so it takes the rest directly.
//! `clone`/`execve`/`exit` are `akuma_tokio_kernel_core::proc`'s, signal
//! delivery `akuma_tokio_kernel_core::signal`'s at the return.

use akuma_syscalls_linux::nr;
use akuma_tokio_kernel_core::{mm, proc};
use akuma_vectors_aarch64::Frame;

use crate::mm::Aarch64;

/// Serve the `svc` that trapped into `f`. A thread `exit_group` asked to end
/// leaves at entry or on the way back, rather than running on in a dying
/// process (`proc::kill_siblings`).
pub fn svc(f: &mut Frame) {
    if proc::kill_requested::<Aarch64>() {
        proc::finish_killed::<Aarch64>();
    }
    let n = f.x[8];
    let a = [f.x[0], f.x[1], f.x[2], f.x[3], f.x[4], f.x[5]];
    f.x[0] = if n == nr::RT_SIGRETURN { crate::signal::sigreturn(f) } else { dispatch(f, n, a) };
    if proc::kill_requested::<Aarch64>() {
        proc::finish_killed::<Aarch64>();
    }
    // Pending signals are taken on the way back: a handler's frame, an ignore,
    // or a default action that ends the group. After an `rt_sigreturn` too, as
    // on Linux: that is how the next pending signal gets its turn.
    crate::signal::at_return(f, n, a[0]);
}

/// The kernel's own arms, then the tokio surface; with `middleware`, wrapped by its `kernel` tier (B22).
fn dispatch(f: &Frame, n: u64, a: [u64; 6]) -> u64 {
    #[cfg(feature = "middleware")]
    {
        let mut args = a;
        akuma_tokio_kernel_core::middleware::run_kernel(n, &mut args, |a| dispatch_arms(f, n, *a))
    }
    #[cfg(not(feature = "middleware"))]
    {
        dispatch_arms(f, n, a)
    }
}

fn dispatch_arms(f: &Frame, n: u64, a: [u64; 6]) -> u64 {
    match n {
        nr::WRITE | nr::WRITEV => {
            if let Some(r) = akuma_runtime_tokio::console::stdio_write(n == nr::WRITEV, a, crate::console_putb) {
                return r;
            }
        }
        nr::EXIT | nr::EXIT_GROUP => proc::exit::<Aarch64>(n == nr::EXIT_GROUP, a[0]),
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
        // The child's register file is the caller's, as of this call.
        nr::CLONE => {
            crate::sched::capture(f);
            return proc::clone::<Aarch64>(a[0], a[1], a[2], a[3], a[4]);
        }
        nr::EXECVE => return crate::proc::exec(a[0], a[1], a[2]),
        nr::MREMAP => return mm::mremap_arm::<Aarch64>(a[0], a[1], a[2], a[3]),
        _ => {}
    }
    if let Some(r) = akuma_runtime_tokio::serve_shared(n, a) {
        return r;
    }
    akuma_syscalls_glue::handle_syscall(n, &a)
}
