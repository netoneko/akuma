#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // This is a dummy main.rs, as the 'make' binary is built from C source.
    // The build process is handled by build.rs.
    // In a real no_std application, you would put your entry point logic here.
    loop {}
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
