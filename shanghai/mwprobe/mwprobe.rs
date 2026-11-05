//! Guest-side check for Shanghai's sample middleware (`shanghai/src/middleware.rs`, B22). Runs as `init`,
//! prints `PASS`/`FAIL` per case and one `MWPROBE RESULT ...` line. Raw `syscall` instructions: this is the
//! one place that must not go through libc, which would hide an `-ENOSYS` behind `errno`.
//!
//! x86_64 and aarch64 Linux ABIs. Build (`shanghai/mw_gate.sh` does this): `rustc --target
//! <arch>-unknown-linux-musl -C linker=rust-lld -C link-self-contained=yes -C target-feature=+crt-static
//! -C relocation-model=static -O mwprobe.rs`.
use std::arch::asm;

#[cfg(target_arch = "x86_64")]
fn sc(nr: u64, a: [u64; 4]) -> i64 {
    let ret: i64;
    // SAFETY: a Linux x86_64 `syscall` with the registers the ABI names; the kernel preserves all others.
    unsafe {
        asm!("syscall", inlateout("rax") nr as i64 => ret, in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
             in("r10") a[3], lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
fn sc(nr: u64, a: [u64; 4]) -> i64 {
    let ret: i64;
    // SAFETY: a Linux aarch64 `svc #0` with x8 = number and x0..x3 = arguments; x0 returns, nothing else changes.
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a[0] as i64 => ret, in("x1") a[1], in("x2") a[2], in("x3") a[3],
             options(nostack));
    }
    ret
}

// The numbers differ per architecture; 1000 and up are the middleware's and are the same on both.
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const GETPID: u64 = 39;
    pub const GETPPID: u64 = 110;
    pub const GETTID: u64 = 186;
    pub const OPENAT: u64 = 257;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETTID: u64 = 178;
    pub const OPENAT: u64 = 56;
}

const ENOSYS: i64 = -38;
const EFAULT: i64 = -14;
const EACCES: i64 = -13;
const ENOENT: i64 = -2;
const AT_FDCWD: u64 = -100i64 as u64;

fn open(path: &str) -> i64 {
    let mut c = path.as_bytes().to_vec();
    c.push(0);
    sc(nr::OPENAT, [AT_FDCWD, c.as_ptr() as u64, 0, 0])
}

fn main() {
    let mut fails = 0;
    let mut check = |name: &str, got: i64, want: i64| {
        let ok = got == want;
        println!("{} {name} (got {got} want {want})", if ok { "PASS" } else { "FAIL" });
        if !ok {
            fails += 1;
        }
    };

    let hello = sc(1000, [0; 4]);
    if hello == ENOSYS {
        // The control: a kernel with no middleware. Every call the middleware would touch must behave as stock.
        println!("MWPROBE no middleware registered (1000 -> ENOSYS); checking the stock behaviour");
        check("custom 1001 is ENOSYS", sc(1001, [0; 4]), ENOSYS);
        check("gettid is stock (== getpid)", sc(nr::GETTID, [0; 4]) - sc(nr::GETPID, [0; 4]), 0);
        check("open /mw-deny is stock (ENOENT, not EACCES)", open("/mw-deny"), ENOENT);
        check("getpid works", i64::from(sc(nr::GETPID, [0; 4]) > 0), 1);
        println!("MWPROBE RESULT CONTROL {}", if fails == 0 { "PASS" } else { "FAIL" });
        return;
    }

    // fallback: a syscall of our own, above every number Linux assigns.
    check("fallback 1000 hello", hello, 42);
    let mut buf = 0u64;
    check("fallback 1001 writes through a user pointer", sc(1001, [&raw mut buf as u64, 0, 0, 0]), 0);
    let me = i64::from(std::process::id());
    check("  ...and wrote the caller's pid", buf as i64, me);
    check("fallback 1001 with a bad pointer is EFAULT, not a crash", sc(1001, [8, 0, 0, 0]), EFAULT);
    check("a number nobody serves is still ENOSYS", sc(1004, [0; 4]), ENOSYS);

    // kernel_pre on openat: reads the path out of user memory, may refuse before the kernel looks.
    check("kernel_pre refuses /mw-deny", open("/mw-deny"), EACCES);
    check("kernel_pre passes /mw-ok (the kernel then says ENOENT)", open("/mw-ok"), ENOENT);
    check("kernel_pre ignores a path it does not care about", open("/definitely-not-here"), ENOENT);

    // kernel_post on gettid replaces the result (for init, tid == pid).
    check("kernel_post gettid + 1000", sc(nr::GETTID, [0; 4]) - sc(nr::GETPID, [0; 4]), 1000);

    // own tier: outside the kernel lock, plain integers + private state.
    let (pre0, post0) = (sc(1002, [0; 4]), sc(1003, [0; 4]));
    for _ in 0..5 {
        sc(nr::GETPID, [0; 4]);
    }
    check("own_pre saw 5 getpids", sc(1002, [0; 4]) - pre0, 5);
    check("own_post saw 5 getpids", sc(1003, [0; 4]) - post0, 5);

    // The autolock under contention: THREADS threads each call getpid CALLS times at once. The `own_pre` hook reads,
    // spins and writes its private state, so a lost update (two hooks in at once) leaves it below the atomic count.
    const THREADS: i64 = 4;
    const CALLS: i64 = 500;
    let pre_before = sc(1002, [0; 4]);
    let hs: Vec<_> = (0..THREADS)
        .map(|_| std::thread::spawn(|| (0..CALLS).for_each(|_| {
            sc(nr::GETPID, [0; 4]);
        })))
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    check("own_pre saw every getpid from 4 threads", sc(1002, [0; 4]) - pre_before, THREADS * CALLS);
    sc(nr::GETPPID, [0; 4]); // own_post(getppid) publishes the private state
    check("own state == atomic count (no lost update under contention)", sc(1005, [0; 4]), sc(1002, [0; 4]));

    println!("MWPROBE RESULT {}", if fails == 0 { "PASS" } else { "FAIL" });
}
