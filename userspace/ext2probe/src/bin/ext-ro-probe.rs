//! QEMU integration probe: mount a disposable ext-family image at /mnt.
#![no_std]
#![no_main]

use libakuma::{close, exit, open, print, println, read, syscall, write};

#[no_mangle]
pub extern "C" fn main() {
    const MS_RDONLY: u64 = 1;
    const MS_REMOUNT: u64 = 32;
    #[cfg(target_arch = "x86_64")]
    const FSYNC: u64 = 74;
    #[cfg(target_arch = "aarch64")]
    const FSYNC: u64 = 82;
    let root_fd = open("/probe.txt", libakuma::open_flags::O_RDONLY);
    if root_fd < 0 { println("ext-ro-probe: ROOT OPEN FAILED"); exit(6); }
    let result = syscall(FSYNC, root_fd as u64, 0, 0, 0, 0, 0) as i64;
    close(root_fd);
    if result != 0 { println("ext-ro-probe: ROOT FSYNC FAILED"); exit(7); }
    let arg = libakuma::arg(1);
    let recover = arg.as_deref() == Some("ext3recover");
    let kind = if matches!(arg.as_deref(), Some("ext3" | "ext3recover")) { b"ext3\0" } else { b"ext4\0" };
    let result = syscall(libakuma::syscall::MOUNT,
        b"/dev/vdb\0".as_ptr() as u64, b"/mnt\0".as_ptr() as u64,
        kind.as_ptr() as u64, MS_RDONLY, 0, 0) as i64;
    if result != 0 { println("ext-ro-probe: MOUNT FAILED"); exit(1); }
    let fd = open("/mnt/payload", libakuma::open_flags::O_RDONLY);
    if fd < 0 { println("ext-ro-probe: OPEN FAILED"); exit(2); }
    let mut buf = [0u8; 64];
    let n = read(fd, &mut buf);
    close(fd);
    let expected = if recover { b"QEMU_EXT3_REPLAY!\n" } else { b"QEMU_EXT4_READ_OK\n" };
    if n < 0 || &buf[..n as usize] != expected {
        println("ext-ro-probe: CONTENT FAILED"); exit(3);
    }
    let fd = open("/mnt/payload", libakuma::open_flags::O_WRONLY);
    if fd >= 0 {
        let n = write(fd, b"wrong");
        close(fd);
        if n >= 0 { println("ext-ro-probe: WRITE SUCCEEDED"); exit(4); }
    }
    let result = syscall(libakuma::syscall::MOUNT, 0,
        b"/mnt\0".as_ptr() as u64, 0, MS_REMOUNT, 0, 0) as i64;
    if result == 0 { println("ext-ro-probe: REMOUNT SUCCEEDED"); exit(5); }
    print("ext-ro-probe: PASS\n");
    exit(0);
}
