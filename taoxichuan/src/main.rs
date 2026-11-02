//! Taoxichuan (淘熙川): the kernel the minimal-self-hosting experiment built, now one file over
//! `akuma-tokio-kernel` (its machine moved to `akuma-tokio-kernel-amd64` in B15).

#![no_std]
#![no_main]

akuma_tokio_kernel::entry!(main);

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    kernel
        .name("Taoxichuan")
        .banner(include_str!("banner.txt"))
        .console()
        .disk()
        .net()
        .smp()
        .run_init()
}
