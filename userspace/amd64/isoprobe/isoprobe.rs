//! The behavioural half of the isolation test: a user process tries to read another process's memory, and what it
//! gets is `SIGSEGV`, except where a copy-on-write share makes the bytes legitimately its own.
//!
//! The boot suite's isolation tests (`process_memory_isolated_except_cow`, `isolation_except_cow_test`) read page tables and
//! frames. This runs real processes and watches what the kernel does to a real load, which is the property a user
//! relies on. Four checks, each in its own child so a fault kills only that child:
//!
//! 1. **COW share, allowed.** `fork` after writing a secret to an anonymous page: the child reads the same address and
//!    sees the secret (a private copy of it), writes a different value, and reads that back.
//! 2. **COW share, isolated.** Back in the parent the page still holds the secret: the child's write went to its own frame.
//! 3. **Unrelated process.** `fork` + `execve` of this same program as `peek <addr>`: a process with a fresh address space
//!    and no relation to the parent's memory loads the parent's secret's address. Expected: killed by `SIGSEGV`. If the
//!    load returns, it prints what it read; `SECRET` there would be a breach, and any other value means the address was
//!    mapped in the new image (an inconclusive probe, reported as a failure so it gets looked at).
//!    A harness-sanity step then aims the same flow at an address that *is* mapped in the peer (its own ELF base) and
//!    requires that it is **not** reported as killed, so the check cannot pass by always saying "killed".
//! 4. **Kernel address.** A child loads a kernel-half address. Expected: `SIGSEGV`.
//!
//! Prints `ISOPROBE PASS` and exits 0, or `ISOPROBE FAIL: <why>` and exits 1. Run it as init: `INIT=/bin/isoprobe`
//! (`scripts/run_isolation_probe.sh` does that and reads the verdict). No libc: raw syscalls, like the other probes here.

#![no_std]
#![no_main]

const SYS_WRITE: u64 = 1;
const SYS_MMAP: u64 = 9;
const SYS_FORK: u64 = 57;
const SYS_EXECVE: u64 = 59;
const SYS_WAIT4: u64 = 61;
const SYS_EXIT_GROUP: u64 = 231;

const SECRET: u64 = 0x5EC2_E75E_C2E7_5EC2;
const CHILD_VALUE: u64 = 0xC0FF_EEC0_FFEE_C0FF;
const SIGSEGV: u64 = 11;
const KERNEL_ADDR: u64 = 0xffff_8000_0000_1000;

#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
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

fn print(s: &str) {
    unsafe { syscall6(SYS_WRITE, 1, s.as_ptr() as u64, s.len() as u64, 0, 0, 0) };
}

fn print_hex(mut v: u64) {
    let mut buf = [b'0'; 18];
    buf[1] = b'x';
    for i in (2..18).rev() {
        buf[i] = b"0123456789abcdef"[(v & 0xf) as usize];
        v >>= 4;
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf) };
    print(s);
}

fn exit(code: u64) -> ! {
    unsafe { syscall6(SYS_EXIT_GROUP, code, 0, 0, 0, 0, 0) };
    loop {}
}

fn fork() -> u64 {
    unsafe { syscall6(SYS_FORK, 0, 0, 0, 0, 0, 0) }
}

/// Wait for `pid`; returns the raw wait status.
fn wait(pid: u64) -> u32 {
    let mut status: u32 = 0;
    unsafe { syscall6(SYS_WAIT4, pid, &raw mut status as u64, 0, 0, 0, 0) };
    status
}

fn exited_with(status: u32, code: u32) -> bool {
    status & 0x7f == 0 && (status >> 8) & 0xff == code
}

fn killed_by(status: u32, sig: u64) -> bool {
    u64::from(status & 0x7f) == sig
}

fn fail(why: &str) -> ! {
    print("ISOPROBE FAIL: ");
    print(why);
    print("\n");
    exit(1);
}

/// Fork + exec `/bin/isoprobe peek <addr>` and return its wait status, or `None` if the exec failed.
fn peek_status(addr: u64) -> Option<u32> {
    let c = fork();
    if c == 0 {
        let mut hex = [0u8; 19];
        hex[0] = b'0';
        hex[1] = b'x';
        let mut v = addr;
        for i in (2..18).rev() {
            hex[i] = b"0123456789abcdef"[(v & 0xf) as usize];
            v >>= 4;
        }
        let path = b"/bin/isoprobe\0";
        let a0 = b"isoprobe\0";
        let a1 = b"peek\0";
        let argv: [u64; 4] = [a0.as_ptr() as u64, a1.as_ptr() as u64, hex.as_ptr() as u64, 0];
        let envp: [u64; 1] = [0];
        unsafe { syscall6(SYS_EXECVE, path.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0) };
        exit(77); // exec failed (is /bin/isoprobe on the disk?)
    }
    let st = wait(c);
    if exited_with(st, 77) { None } else { Some(st) }
}

fn parse_hex(s: *const u8) -> u64 {
    let mut v = 0u64;
    let mut p = s;
    unsafe {
        // Optional 0x prefix.
        if *p == b'0' && *p.add(1) == b'x' {
            p = p.add(2);
        }
        while *p != 0 {
            let d = match *p {
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'f' => c - b'a' + 10,
                _ => break,
            };
            v = (v << 4) | u64::from(d);
            p = p.add(1);
        }
    }
    v
}

#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(sp: *const u64) -> ! {
    let argc = unsafe { *sp };
    let argv = unsafe { sp.add(1) as *const *const u8 };
    // `peek <addr>`: the unrelated process. Reaching the print below means the load returned.
    if argc >= 3 {
        let a1 = unsafe { *argv.add(1) };
        if unsafe { *a1 } == b'p' {
            let addr = parse_hex(unsafe { *argv.add(2) });
            let v = unsafe { core::ptr::read_volatile(addr as *const u64) };
            print("PEEK-SURVIVED value=");
            print_hex(v);
            print(if v == SECRET { " (THE PARENT'S SECRET)\n" } else { "\n" });
            exit(0);
        }
    }

    // The parent's secret, on an anonymous page of its own.
    let page = unsafe { syscall6(SYS_MMAP, 0, 4096, 3, 0x22, u64::MAX, 0) };
    if page == 0 || page > 0xffff_ffff_ffff_f000 {
        fail("mmap failed");
    }
    let cell = page as *mut u64;
    unsafe { core::ptr::write_volatile(cell, SECRET) };
    print("ISOPROBE secret at ");
    print_hex(page);
    print("\n");

    // 1 and 2: a fork child shares the page copy-on-write.
    let c = fork();
    if c == 0 {
        if unsafe { core::ptr::read_volatile(cell) } != SECRET {
            exit(10); // the child does not see its parent's bytes through the COW share
        }
        unsafe { core::ptr::write_volatile(cell, CHILD_VALUE) };
        if unsafe { core::ptr::read_volatile(cell) } != CHILD_VALUE {
            exit(11); // the child cannot read back its own write
        }
        exit(0);
    }
    let st = wait(c);
    if !exited_with(st, 0) {
        print("child status ");
        print_hex(u64::from(st));
        print("\n");
        fail("COW child: it did not see the share, or could not write its own copy");
    }
    print("ISOPROBE cow_child=ok (saw the share, wrote its own copy)\n");
    if unsafe { core::ptr::read_volatile(cell) } != SECRET {
        fail("COW leak: the child's write is visible in the parent");
    }
    print("ISOPROBE cow_isolated=ok (the parent's bytes are unchanged)\n");

    // 3: an unrelated process (fresh address space) loads the parent's address.
    match peek_status(page) {
        None => fail("could not exec /bin/isoprobe for the unrelated-process check (is it on the disk image?)"),
        Some(st) if killed_by(st, SIGSEGV) => print("ISOPROBE unrelated=ok (SIGSEGV)\n"),
        Some(st) => {
            print("peek status ");
            print_hex(u64::from(st));
            print("\n");
            fail("an unrelated process's load of the parent's address was not killed by SIGSEGV");
        }
    }
    // Harness sanity: the same flow aimed at an address that IS mapped in the peer (its own code, the ELF base) must NOT
    // read as killed. Without this a check that always reports "killed" would pass for the wrong reason.
    match peek_status(0x40_0000) {
        Some(st) if !killed_by(st, SIGSEGV) => print("ISOPROBE harness_sanity=ok (a mapped address is not reported as isolated)\n"),
        Some(_) => fail("harness: a load of a mapped address was reported as a SIGSEGV"),
        None => fail("harness: could not exec /bin/isoprobe"),
    }

    // 4: a kernel-half address.
    let c = fork();
    if c == 0 {
        let v = unsafe { core::ptr::read_volatile(KERNEL_ADDR as *const u64) };
        print("KERNEL-READ-SURVIVED value=");
        print_hex(v);
        print("\n");
        exit(0);
    }
    let st = wait(c);
    if !killed_by(st, SIGSEGV) {
        print("kernel-read status ");
        print_hex(u64::from(st));
        print("\n");
        fail("a load of a kernel address was not killed by SIGSEGV");
    }
    print("ISOPROBE kernel_addr=ok (SIGSEGV)\n");

    print("ISOPROBE PASS\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(2)
}
