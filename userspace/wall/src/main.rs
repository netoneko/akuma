//! `wall` — put a line on the machine's own console.
//!
//! ```text
//! wall hello from the trashcan
//! wall "DNS fix: hget https works"
//! ```
//!
//! Joins `argv[1..]` with single spaces and hands the result to
//! `libakuma::console_notify`, the Akuma-private syscall (322) that prints a
//! framed line straight to the framebuffer/serial console. On the amd64
//! bare-metal box — a display, no working keyboard — that screen is the only
//! local channel to a person in front of it; over ssh this is how a session
//! leaves a note on it.
//!
//! The kernel caps the message at 512 bytes and strips control characters, so
//! this side does not need to. It fails with a message (not silently) if the
//! kernel was built without the `console-notify` feature — `console_notify`
//! returns `ENOSYS` there.

#![no_std]
#![no_main]

use libakuma::{arg, argc, console_notify, exit, print};

/// Local assembly buffer for the joined message. Matches the kernel's own
/// 512-byte cap: anything past it is dropped here rather than sent and truncated
/// there, so the two agree on where the line ends.
const MAX: usize = 512;

#[no_mangle]
pub extern "C" fn main() {
    if argc() < 2 {
        print("usage: wall <message>\n");
        exit(2);
    }

    let mut buf = [0u8; MAX];
    let mut len = 0usize;

    for i in 1..argc() {
        let Some(word) = arg(i) else { continue };
        if i > 1 && len < MAX {
            buf[len] = b' ';
            len += 1;
        }
        let bytes = word.as_bytes();
        let take = bytes.len().min(MAX - len);
        buf[len..len + take].copy_from_slice(&bytes[..take]);
        len += take;
        if len >= MAX {
            break;
        }
    }

    // `buf[..len]` is UTF-8 by construction — it is `&str` fragments and ASCII
    // spaces — unless a multi-byte character was split by the `MAX` clamp above.
    // In that case drop the trailing partial bytes rather than send invalid
    // UTF-8 the kernel would replace wholesale.
    let msg = match core::str::from_utf8(&buf[..len]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or(""),
    };

    match console_notify(msg) {
        Ok(()) => exit(0),
        Err(errno) => {
            print("wall: console_notify failed (errno ");
            print_i32(errno);
            print(")\n");
            exit(1);
        }
    }
}

/// Print a small signed integer without pulling in `core::fmt`.
fn print_i32(mut v: i32) {
    if v == 0 {
        print("0");
        return;
    }
    if v < 0 {
        print("-");
        v = -v;
    }
    let mut digits = [0u8; 10];
    let mut n = 0;
    while v > 0 {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut out = [0u8; 10];
    for i in 0..n {
        out[i] = digits[n - 1 - i];
    }
    // SAFETY: `out[..n]` is ASCII digits.
    print(unsafe { core::str::from_utf8_unchecked(&out[..n]) });
}
