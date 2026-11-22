//! PTY descriptor ownership across dup and fork.
//!
//! rustc --edition 2021 -O --target x86_64-unknown-linux-musl ptyrefs.rs -o ptyrefs
use std::ffi::{c_char, c_int, c_void};
use std::ptr::null_mut;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn openpty(
        master: *mut c_int,
        slave: *mut c_int,
        name: *mut c_char,
        term: *const c_void,
        win: *const c_void,
    ) -> c_int;
    fn dup(fd: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn fork() -> c_int;
    fn pipe(fds: *mut c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn poll(fds: *mut PollFd, n: usize, timeout: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn _exit(status: c_int) -> !;
}
fn pair() -> (c_int, c_int) {
    let (mut m, mut s) = (-1, -1);
    assert_eq!(
        unsafe { openpty(&mut m, &mut s, null_mut(), null_mut(), null_mut()) },
        0,
        "openpty: {}",
        std::io::Error::last_os_error()
    );
    (m, s)
}
fn live_master(m: c_int) {
    let mut p = PollFd {
        fd: m,
        events: 1,
        revents: 0,
    };
    assert!(unsafe { poll(&mut p, 1, 0) } >= 0);
    assert_eq!(p.revents & 0x10, 0, "premature POLLHUP");
}
fn hung_up(m: c_int) {
    let mut p = PollFd {
        fd: m,
        events: 1,
        revents: 0,
    };
    assert_eq!(unsafe { poll(&mut p, 1, 0) }, 1);
    assert_ne!(p.revents & 0x10, 0, "last slave close must report POLLHUP");
}
fn main() {
    unsafe {
        let (m, s) = pair();
        let alias = dup(s);
        assert!(alias >= 0);
        close(s);
        live_master(m);
        assert_eq!(write(alias, b"D".as_ptr().cast(), 1), 1);
        let mut b = [0u8; 1];
        assert_eq!(read(m, b.as_mut_ptr().cast(), 1), 1);
        assert_eq!(b, *b"D");
        close(alias);
        hung_up(m);
        close(m);

        let (m, s) = pair();
        let alias = dup(m);
        assert!(alias >= 0);
        close(m);
        assert_eq!(write(s, b"M".as_ptr().cast(), 1), 1);
        let mut b = [0u8; 1];
        assert_eq!(read(alias, b.as_mut_ptr().cast(), 1), 1);
        assert_eq!(b, *b"M");
        close(s);
        close(alias);

        let (m, s) = pair();
        let mut sync = [0; 2];
        assert_eq!(pipe(sync.as_mut_ptr()), 0);
        let child = fork();
        assert!(child >= 0);
        if child == 0 {
            close(m);
            close(sync[1]);
            let mut b = [0u8; 1];
            if read(sync[0], b.as_mut_ptr().cast(), 1) != 1
                || write(s, b"F".as_ptr().cast(), 1) != 1
            {
                _exit(1);
            }
            _exit(0);
        }
        close(s);
        close(sync[0]);
        live_master(m);
        assert_eq!(write(sync[1], b"!".as_ptr().cast(), 1), 1);
        let mut b = [0u8; 1];
        assert_eq!(read(m, b.as_mut_ptr().cast(), 1), 1);
        assert_eq!(b, *b"F");
        let mut status = 0;
        assert_eq!(waitpid(child, &mut status, 0), child);
        assert_eq!(status, 0);
        hung_up(m);
        close(sync[1]);
        close(m);
    }
    println!("PTYREFS PASS: dup and fork keep both ends alive");
}
