//! `write(2)` from a clone-spawned thread — the minimal repro for
//! docs/archive/AMD64_SPAWNED_THREAD_NEVER_RUNS.md §0 (raft child dies
//! inside its first log write; `RAFT_STAGE` pinned it to the write).
//!
//! Uses meow's own trampoline (`userspace/meow/src/rt.rs`) byte-for-byte,
//! same flags, same TCB shape, 64 KiB stack. The child stages its progress
//! around two writes:
//!
//!   1 = entered child, 2 = survived a write to fd 1 (the ssh channel),
//!   3 = survived a write to a file fd opened by the parent,
//!   4 = done, child exits.
//!
//! The parent prints `stage=N` once a second. Whatever N stops at is the
//! syscall that never returns. Parent gives up after 8 s.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

const SYS_WRITE: u64 = 1;
const SYS_NANOSLEEP: u64 = 35;
const SYS_OPENAT: u64 = 257;
const SYS_EXIT: u64 = 60;
const SYS_EXIT_GROUP: u64 = 231;

const AT_FDCWD: i32 = -100;
const O_WRONLY: u64 = 1;
const O_APPEND: u64 = 0o2000;
const O_CREAT: u64 = 0o100;

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const THREAD_FLAGS: u64 =
    CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;

static STAGE: AtomicU64 = AtomicU64::new(0);
static LOGFD: AtomicU64 = AtomicU64::new(u64::MAX);

#[repr(C, align(16))]
struct Tcb {
    self_ptr: *mut Tcb,
    _reserved: [u64; 7],
}
static mut TCB: Tcb = Tcb { self_ptr: core::ptr::null_mut(), _reserved: [0; 7] };

#[repr(align(16))]
struct Stack([u8; 64 * 1024]);
static mut STACK: Stack = Stack([0; 64 * 1024]);

unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3,
        lateout("rcx") _, lateout("r11") _,
        options(nostack)
    );
    ret
}

unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1, in("rsi") a2, in("rdx") a3, in("r10") a4,
        lateout("rcx") _, lateout("r11") _,
        options(nostack)
    );
    ret
}

fn write_fd(fd: u64, s: &[u8]) {
    unsafe { syscall3(SYS_WRITE, fd, s.as_ptr() as u64, s.len() as u64) };
}

fn msg(s: &[u8]) {
    write_fd(1, s);
}

// ---- meow's trampoline, verbatim in shape ----
core::arch::global_asm!(
    r#"
    .section .text.litter_spawn_thread
    .global litter_spawn_thread
litter_spawn_thread:
    /* rdi = flags, rsi = child stack top, rdx = entry fn, rcx = tls */
    sub rsi, 8
    mov [rsi], rdx              /* plant the entry fn on the child stack */
    xor edx, edx                /* parent_tid = NULL */
    xor r10d, r10d              /* child_tid = NULL (detached) */
    mov r8, rcx                 /* tls — MUST be set before the syscall:
                                   `syscall` clobbers rcx with the return rip */
    mov rax, 56                 /* SYS_clone */
    syscall
    test rax, rax
    jnz 2f                      /* parent: rax = child tid, done */
    /* child */
    pop rax
    call rax
    mov eax, 60                 /* SYS_exit — NOT exit_group */
    xor edi, edi                /* status 0 */
    syscall
1:  jmp 1b
2:  ret
"#
);

unsafe extern "C" {
    fn litter_spawn_thread(flags: u64, stack_top: *mut core::ffi::c_void, entry: fn(), tls: *mut core::ffi::c_void) -> i64;
}

fn child() {
    STAGE.store(1, Ordering::Release);
    msg(b"child: about to write fd 1\n");
    msg(b"child: wrote fd 1\n");
    STAGE.store(2, Ordering::Release);
    let fd = LOGFD.load(Ordering::Acquire);
    if fd != u64::MAX {
        write_fd(fd, b"child: file write\n");
    }
    STAGE.store(3, Ordering::Release);
    let tid = unsafe { syscall3(186, 0, 0, 0) }; // gettid — known to work
    if tid > 0 {
        STAGE.store(4, Ordering::Release);
    }
    unsafe { syscall3(SYS_EXIT, 0, 0, 0) };
}

fn print_stage() {
    let mut buf = [0u8; 16];
    let s = STAGE.load(Ordering::Acquire);
    let mut n = s;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    msg(b"stage=");
    msg(&buf[i..]);
    msg(b"\n");
}

fn sleep_1s() {
    let spec = Timespec { sec: 1, nsec: 0 };
    unsafe { syscall3(SYS_NANOSLEEP, &spec as *const Timespec as u64, 0, 0) };
}

// ---- v2: the parent mimics meow's bootstrap after the spawn: bind a
// listener, connect to it, and poll-read with nobody ever accepting. If the
// child starves only in this configuration, the interplay is networking's
// BKL hold against the child's write.

const SYS_SOCKET: u64 = 41;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_CONNECT: u64 = 42;

#[repr(C)]
struct SockaddrIn {
    family: u16,
    port: u16,   // big-endian
    addr: u32,   // big-endian
    zero: [u8; 8],
}

fn net_self_abuse() {
    unsafe {
        let sfd = syscall3(SYS_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
        if sfd < 0 { msg(b"parent: socket failed\n"); return; }
        let mut sa = SockaddrIn {
            family: 2,
            port: (7799u16).to_be(),
            addr: u32::from_be(0x7F00_0001), // 127.0.0.1
            zero: [0; 8],
        };
        if syscall3(SYS_BIND, sfd as u64, &mut sa as *mut SockaddrIn as u64, 16) < 0 {
            msg(b"parent: bind failed\n"); return;
        }
        if syscall3(SYS_LISTEN, sfd as u64, 8, 0) < 0 {
            msg(b"parent: listen failed\n"); return;
        }
        msg(b"parent: listening, connecting to self\n");
        // BLOCKING connect to our own listener; nobody ever accepts, so the
        // handshake only advances if the kernel's own stack does it in the
        // connect wait — meow's exact baseline shape.
        let ret = syscall3(SYS_CONNECT, sfd as u64, &mut sa as *mut SockaddrIn as u64, 16);
        if ret < 0 {
            msg(b"parent: connect returned (failed)\n");
        } else {
            msg(b"parent: connect returned (ok)\n");
        }
    }
}

#[repr(C)]
struct Timespec { sec: i64, nsec: i64 }

#[unsafe(no_mangle)]
pub extern "C" fn rust_start(_sp: *const u64) -> ! {
    msg(b"parent: opening /tmp/wprobe.log\n");
    let path = b"/tmp/wprobe.log\0";
    let fd = unsafe {
        syscall4(
            SYS_OPENAT,
            AT_FDCWD as u64,
            path.as_ptr() as u64,
            O_WRONLY | O_APPEND | O_CREAT,
            0o644,
        )
    };
    if fd < 0 {
        msg(b"parent: open FAILED\n");
    } else {
        LOGFD.store(fd as u64, Ordering::Relaxed);
    }

    // Single-threaded here, same as meow's spawn site.
    let stack = unsafe { &mut *core::ptr::addr_of_mut!(STACK) };
    let top = stack.0.as_mut_ptr().wrapping_add(64 * 1024) as *mut core::ffi::c_void;
    let tcb = unsafe { core::ptr::addr_of_mut!(TCB) };
    unsafe { (*tcb).self_ptr = tcb };
    let tls = tcb as *mut core::ffi::c_void;
    let spawned = unsafe { litter_spawn_thread(THREAD_FLAGS, top, child, tls) };
    msg(b"parent: clone returned\n");
    net_self_abuse();
    msg(b"parent: entering stage watch\n");
    let mut buf = [0u8; 16];
    let mut n = spawned as u64;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    msg(b"parent: tid=");
    msg(&buf[i..]);
    msg(b"\n");

    for _ in 0..8 {
        sleep_1s();
        print_stage();
    }
    msg(b"parent: giving up\n");
    unsafe { syscall3(SYS_EXIT_GROUP, 0, 0, 0) };
    loop {}
}

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b
"#
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { syscall3(SYS_EXIT_GROUP, 99, 0, 0) };
    loop {}
}
