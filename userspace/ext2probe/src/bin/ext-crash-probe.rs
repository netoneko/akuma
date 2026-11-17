//! One overwrite for the QEMU journal crash-boundary test.
#![no_std]
#![no_main]
use libakuma::{close, exit, open, println, syscall, write};
use libakuma::open_flags::O_WRONLY;
#[no_mangle]
pub extern "C" fn main() {
    let mounted = syscall(libakuma::syscall::MOUNT, b"/dev/vdb\0".as_ptr() as u64,
        b"/mnt\0".as_ptr() as u64, b"ext3test\0".as_ptr() as u64, 0, 0, 0) as i64;
    if mounted != 0 { println("ext-crash-probe: MOUNT FAILED"); exit(1); }
    let fd = open("/mnt/payload", O_WRONLY);
    if fd < 0 { println("ext-crash-probe: OPEN FAILED"); exit(2); }
    if write(fd, b"CRASH_TEST_NEW!!!\n") != 18 { println("ext-crash-probe: WRITE FAILED"); exit(3); }
    if close(fd) != 0 { println("ext-crash-probe: CLOSE FAILED"); exit(4); }
    println("ext-crash-probe: PASS");
    exit(0);
}
