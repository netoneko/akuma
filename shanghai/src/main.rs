//! Shanghai: a kernel built from the `akuma-tokio-kernel` README. The whole
//! kernel is this file plus a skyline.

#![no_std]
#![no_main]

#[cfg(feature = "middleware")]
mod middleware;

akuma_tokio_kernel::entry!(main);

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    let kernel = kernel
        .name("Shanghai")
        .banner(include_str!("banner.txt"))
        .console()
        .disk()
        .net()
        .smp();
    #[cfg(feature = "middleware")]
    let kernel = kernel.middleware(&middleware::MW);
    kernel.run_init()
}
