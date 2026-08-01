#![no_std]
#[panic_handler] fn p(_: &core::panic::PanicInfo) -> ! { loop {} }
pub fn hello() -> u32 { 42 }
