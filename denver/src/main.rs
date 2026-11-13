//! Denver: the teaching kernel over `akuma-jvm-kernel`. The whole kernel is
//! this file plus a skyline.

#![no_std]
#![no_main]

akuma_jvm_kernel::entry!(main);

fn main(kernel: akuma_jvm_kernel::Kernel) -> ! {
    kernel
        .name("Denver")
        .banner(include_str!("banner.txt"))
        .console()
        .disk()
        .net()
        .smp()
        .run_init()
}
