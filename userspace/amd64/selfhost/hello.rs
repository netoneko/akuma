//! The program the **guest's own `rustc`** compiles, to prove it can.
//!
//! Not part of any build: this file is copied into an Akuma/amd64 guest and
//! compiled there, by the nightly toolchain staged per
//! `docs/runbooks/stage-rust-toolchain-amd64.md`. It lives in the tree so the
//! demo is reproducible rather than retyped into a shell each time.
//!
//!   rustc -C linker-flavor=ld \
//!         -C linker=<sysroot>/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld \
//!         -C link-self-contained=yes -o /tmp/hello /tmp/hello.rs
//!
//! # Why `uname` by hand
//!
//! `std` has no `uname`, and pulling in the `libc` crate would need a registry
//! and a network. A raw `syscall` is three lines and tests something the
//! wrapper would hide: that a **Rust `std` program** — with musl's start-up,
//! its TLS and its signal setup already behind it — can issue an arbitrary
//! syscall on this kernel and unpack a `repr(C)` struct the kernel filled.
//!
//! `struct utsname` is six fixed 65-byte NUL-terminated fields on Linux, and
//! nothing in the ABI marks where the text ends except that NUL — so unpacking
//! it is the actual test. A field the kernel left unterminated shows up here as
//! a 65-byte string with rubbish on the end, not as an error.
use std::arch::asm;

/// x86_64 `uname(2)`. Spelled out: this file is compiled standalone, so it
/// cannot see `akuma-syscalls-abi`, which is where the number is really kept.
const SYS_UNAME: u64 = 63;

/// Linux's `struct utsname`: six fields, 65 bytes each, NUL-terminated.
#[repr(C)]
struct UtsName {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

impl UtsName {
    const fn zeroed() -> Self {
        Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
            domainname: [0; 65],
        }
    }
}

/// `uname(buf)`. Returns the kernel's raw answer: 0, or a negative errno.
///
/// # Safety
/// `buf` must point at a writable `UtsName`. `rcx` and `r11` are clobbered by
/// the `syscall` instruction itself, so both are declared `lateout`; omitting
/// them lets the compiler keep a live value in either across the call.
unsafe fn uname(buf: *mut UtsName) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_UNAME => ret,
            in("rdi") buf,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// The text of one field: everything before the first NUL.
fn field(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("<not utf-8>")
}

fn main() {
    let mut u = UtsName::zeroed();
    // SAFETY: `u` is a live, writable `UtsName` for the duration of the call.
    let rc = unsafe { uname(&raw mut u) };
    if rc < 0 {
        println!("ssh late.sh from somewhere — uname failed: errno {}", -rc);
        return;
    }

    // `uname -a`'s order, which is the order the fields are declared in.
    println!(
        "ssh late.sh from {} {} {} {} {}",
        field(&u.sysname),
        field(&u.nodename),
        field(&u.release),
        field(&u.version),
        field(&u.machine),
    );
}
