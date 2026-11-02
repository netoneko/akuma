//! Osaka: the sample kernel for `akuma-tokio-kernel`. The whole kernel is this file.

#![no_std]
#![no_main]

akuma_tokio_kernel::entry!(main);

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    kernel.name("Osaka").console().disk().net().smp().run_init()
}
