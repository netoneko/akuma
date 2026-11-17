//! Disposable QEMU ext3 write-path integration probe.
#![no_std]
#![no_main]
use libakuma::{close, exit, mkdir, open, println, read, rename, syscall, unlink, write};
use libakuma::open_flags::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};

fn fail(message: &str) -> ! { println(message); exit(1) }

#[no_mangle]
pub extern "C" fn main() {
    let mounted = syscall(libakuma::syscall::MOUNT,
        b"/dev/vdb\0".as_ptr() as u64, b"/mnt\0".as_ptr() as u64,
        b"ext3test\0".as_ptr() as u64, 0, 0, 0) as i64;
    if mounted != 0 { fail("ext-rw-probe: MOUNT FAILED"); }
    if unlink("/mnt/missing") == 0 { fail("ext-rw-probe: MISSING UNLINK SUCCEEDED"); }
    if mkdir("/mnt/work") != 0 { fail("ext-rw-probe: MKDIR FAILED"); }
    let fd = open("/mnt/work/a", O_CREAT | O_WRONLY);
    if fd < 0 { fail("ext-rw-probe: CREATE FAILED"); }
    if write(fd, b"journaled guest data\n") != 21 { fail("ext-rw-probe: WRITE FAILED"); }
    if close(fd) != 0 { fail("ext-rw-probe: CLOSE FAILED"); }
    if rename("/mnt/work/a", "/mnt/work/b") != 0 { fail("ext-rw-probe: RENAME FAILED"); }
    let fd = open("/mnt/work/b", O_RDONLY);
    if fd < 0 { fail("ext-rw-probe: OPEN FAILED"); }
    let mut data = [0u8; 64];
    let n = read(fd, &mut data);
    if n != 21 || &data[..21] != b"journaled guest data\n" { fail("ext-rw-probe: CONTENT FAILED"); }
    if close(fd) != 0 { fail("ext-rw-probe: READ CLOSE FAILED"); }
    let fd = open("/mnt/work/b", O_WRONLY | O_TRUNC);
    if fd < 0 { fail("ext-rw-probe: TRUNCATE FAILED"); }
    if write(fd, b"new contents\n") != 13 { fail("ext-rw-probe: REWRITE FAILED"); }
    if close(fd) != 0 { fail("ext-rw-probe: WRITE CLOSE FAILED"); }
    let fd = open("/mnt/work/b", O_RDONLY);
    if fd < 0 { fail("ext-rw-probe: REOPEN FAILED"); }
    let n = read(fd, &mut data);
    if n != 13 || &data[..13] != b"new contents\n" { fail("ext-rw-probe: REWRITE CONTENT FAILED"); }
    if close(fd) != 0 { fail("ext-rw-probe: FINAL CLOSE FAILED"); }
    if unlink("/mnt/work/b") != 0 { fail("ext-rw-probe: UNLINK FAILED"); }
    let fd = open("/mnt/result", O_CREAT | O_WRONLY);
    if fd < 0 || write(fd, b"persistent guest result\n") != 24 { fail("ext-rw-probe: RESULT WRITE FAILED"); }
    #[cfg(target_arch = "x86_64")]
    const FSYNC: u64 = 74;
    #[cfg(target_arch = "aarch64")]
    const FSYNC: u64 = 82;
    if syscall(FSYNC, fd as u64, 0, 0, 0, 0, 0) as i64 != 0 { fail("ext-rw-probe: FSYNC FAILED"); }
    if close(fd) != 0 { fail("ext-rw-probe: RESULT CLOSE FAILED"); }
    println("ext-rw-probe: PASS");
    exit(0);
}
