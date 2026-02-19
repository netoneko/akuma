//! cat - Concatenate and print files
//!
//! A simple cat utility for Akuma OS isolation tests.

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{args, exit, open, read_fd, write_fd, close, fd, open_flags, print};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut args = args();
    let _prog_name = args.next(); // Skip program name

    let mut files = 0;
    for path in args {
        files += 1;
        cat_file(path);
    }

    if files == 0 {
        // Cat from stdin
        cat_fd(fd::STDIN as i32);
    }

    exit(0);
}

fn cat_file(path: &str) {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        print("cat: ");
        print(path);
        print(": No such file or directory
");
        return;
    }

    cat_fd(fd);
    close(fd);
}

fn cat_fd(fd_num: i32) {
    let mut buf = [0u8; 1024];
    loop {
        let n = read_fd(fd_num, &mut buf);
        if n <= 0 {
            break;
        }
        write_fd(fd::STDOUT as i32, &buf[..n as usize]);
    }
}
