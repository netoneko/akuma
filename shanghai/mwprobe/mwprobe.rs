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

/// Six arguments, for `mmap` (fd and offset are the fifth and sixth).
#[cfg(target_arch = "x86_64")]
fn sc6(nr: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    // SAFETY: as `sc`, with r8 and r9 for the fifth and sixth argument.
    unsafe {
        asm!("syscall", inlateout("rax") nr as i64 => ret, in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
             in("r10") a[3], in("r8") a[4], in("r9") a[5], lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
fn sc6(nr: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    // SAFETY: as `sc`, with x4 and x5 for the fifth and sixth argument.
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a[0] as i64 => ret, in("x1") a[1], in("x2") a[2], in("x3") a[3],
             in("x4") a[4], in("x5") a[5], options(nostack));
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
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const EXECVE: u64 = 59;
    pub const CLONE: u64 = 56;
    pub const WAIT4: u64 = 61;
    pub const EXIT_GROUP: u64 = 231;
    pub const WRITE: u64 = 1;
    pub const MEMFD_CREATE: u64 = 319;
    pub const EXECVEAT: u64 = 322;
    pub const PTRACE: u64 = 101;
    pub const PROCESS_VM_WRITEV: u64 = 311;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETTID: u64 = 178;
    pub const OPENAT: u64 = 56;
    pub const MMAP: u64 = 222;
    pub const MPROTECT: u64 = 226;
    pub const EXECVE: u64 = 221;
    pub const CLONE: u64 = 220;
    pub const WAIT4: u64 = 260;
    pub const EXIT_GROUP: u64 = 94;
    pub const WRITE: u64 = 64;
    pub const MEMFD_CREATE: u64 = 279;
    pub const EXECVEAT: u64 = 281;
    pub const PTRACE: u64 = 117;
    pub const PROCESS_VM_WRITEV: u64 = 271;
}

const ENOSYS: i64 = -38;
const EFAULT: i64 = -14;
const EACCES: i64 = -13;
const ENOENT: i64 = -2;
const EPERM: i64 = -1;
const AT_FDCWD: u64 = -100i64 as u64;
const PROT_READ: u64 = 1;
const PROT_EXEC: u64 = 4;
const MAP_PRIVATE: u64 = 2;
const MAP_ANONYMOUS: u64 = 0x20;
const NO_FD: u64 = -1i64 as u64;
const SIGCHLD: u64 = 17;
const PROT_WRITE: u64 = 2;
#[cfg(target_arch = "x86_64")]
const LOADER: &str = "/lib/ld-musl-x86_64.so.1";
#[cfg(target_arch = "aarch64")]
const LOADER: &str = "/lib/ld-musl-aarch64.so.1";

/// The errno a child spawn failed with (`execve` refused it), or `None` if it spawned.
fn spawn_errno(path: &str, args: &[&str]) -> Option<i32> {
    match std::process::Command::new(path).args(args).env_clear().output() {
        Ok(_) => None,
        Err(e) => Some(e.raw_os_error().unwrap_or(-1)),
    }
}

/// The attempts to get code into a process that the no-dynamic-loading policy refuses (a file mapped executable, and a
/// read-only file mapping made executable); returns how many were NOT refused. Used by the children, which must be bound by
/// the same policy as the process that made them.
fn load_attempts_allowed(fd: i64) -> i64 {
    let mut allowed = 0;
    if sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]) != EACCES {
        allowed += 1;
    }
    let ro = sc6(nr::MMAP, [0, 4096, PROT_READ, MAP_PRIVATE, fd as u64, 0]);
    if sc(nr::MPROTECT, [ro as u64, 4096, PROT_READ | PROT_EXEC, 0]) != EACCES {
        allowed += 1;
    }
    allowed
}

/// Run a dynamically linked child with an empty environment; returns whether it exited 0 and the first line of what it printed
/// (stderr if it printed any, else stdout: a loader failure goes to stderr, `xz --version` to stdout).
fn run_dynamic(path: &str, arg: &str) -> (bool, String) {
    match std::process::Command::new(path).arg(arg).env_clear().output() {
        Ok(out) => {
            let text = if out.stderr.is_empty() { &out.stdout } else { &out.stderr };
            (out.status.success(), String::from_utf8_lossy(text).lines().next().unwrap_or("").to_string())
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

fn open(path: &str) -> i64 {
    let mut c = path.as_bytes().to_vec();
    c.push(0);
    sc(nr::OPENAT, [AT_FDCWD, c.as_ptr() as u64, 0, 0])
}

fn main() {
    // The image `execve` starts at the end of a good run: it proves exec still works with a clean environment, and it
    // prints the result for the whole run (argv[2] = how many cases failed before the exec).
    let args: Vec<String> = std::env::args().collect();
    // A static child, exec'd by the probe in its own process: it tries to load code and exits 0 only if every attempt was refused.
    if args.get(1).map(String::as_str) == Some("kid") {
        let n = load_attempts_allowed(open("/bin/mwprobe"));
        println!("MWPROBE kid pid={} load attempts allowed={n}", std::process::id());
        std::process::exit(i32::from(n != 0));
    }
    if args.get(1).map(String::as_str) == Some("noop") {
        println!("MWPROBE noop child ran");
        return;
    }
    if args.get(1).map(String::as_str) == Some("child") {
        println!("PASS execve with a clean environment is allowed (the new image ran)");
        let failed: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
        println!("MWPROBE RESULT {}", if failed == 0 { "PASS" } else { "FAIL" });
        return;
    }
    // `nodl` (the gate's `initargs=nodl`): this kernel was built with `--features no-dynamic-loading`, so the guards must be on.
    // Without it they must be absent: a build that silently lacks them, or has them when it should not, fails the gate.
    let nodl = args.get(1).map(String::as_str) == Some("nodl");
    // `dynelf` (`initargs=dynelf`): the kernel was built with only `--features no-dynamic-elf`, so there is NO middleware and the elf crate
    // refuses dynamic images with EPERM. That is a different expectation from both "stock" and "nodl", so it is its own branch.
    let dynelf = args.get(1).map(String::as_str) == Some("dynelf");
    let fails = std::cell::Cell::new(0);
    let check = |name: &str, got: i64, want: i64| {
        let ok = got == want;
        println!("{} {name} (got {got} want {want})", if ok { "PASS" } else { "FAIL" });
        if !ok {
            fails.set(fails.get() + 1);
        }
    };

    let hello = sc(1000, [0; 4]);
    if hello == ENOSYS && dynelf {
        // Build-time ban in the elf crate: images only, EPERM, nothing else changed.
        check("A1 execve of a dynamic program with an extra library (xz) is refused with EPERM", i64::from(spawn_errno("/usr/bin/xz", &["--version"]) == Some(1)), 1);
        check("A2 execve of a dynamic program with only libc (busybox) is refused with EPERM", i64::from(spawn_errno("/bin/busybox", &["true"]) == Some(1)), 1);
        let script = "/tmp/dyn.sh";
        let _ = std::fs::write(script, "#!/bin/busybox sh\ntrue\n");
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755));
        }
        check("C  a #! script with a dynamic interpreter is refused with EPERM", i64::from(spawn_errno(script, &[]) == Some(1)), 1);
        let noop = std::process::Command::new("/bin/mwprobe").arg("noop").env_clear().status();
        check("H  a static child still runs", i64::from(matches!(&noop, Ok(s) if s.success())), 1);
        // The scope, pinned: this feature refuses IMAGES. A kernel with no mapping policy still lets a program map a file executable
        // by hand, so the loader run directly (it has no interpreter of its own) still loads a dynamic program. Closing that is the
        // exec policy's job (`no-dynamic-loading`), not this feature's; if it ever closes, these two lines fail and say so.
        let (ok, out) = run_dynamic(LOADER, "/bin/busybox");
        println!("MWPROBE loader-direct: ok={ok} output={out}");
        check("B  (scope, pinned) the loader run directly still loads a dynamic program: images only", i64::from(ok), 1);
        let fd = open("/bin/mwprobe");
        let file_exec = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]);
        check("D1 (scope, pinned) mmap(file, PROT_EXEC) is still allowed: no mapping policy in this build", i64::from(file_exec > 0), 1);
        println!("MWPROBE RESULT DYNELF {}", if fails.get() == 0 { "PASS" } else { "FAIL" });
        return;
    }
    if hello == ENOSYS {
        // The control: a kernel with no middleware. Every call the middleware would touch must behave as stock.
        println!("MWPROBE no middleware registered (1000 -> ENOSYS); checking the stock behaviour");
        check("custom 1001 is ENOSYS", sc(1001, [0; 4]), ENOSYS);
        check("gettid is stock (== getpid)", sc(nr::GETTID, [0; 4]) - sc(nr::GETPID, [0; 4]), 0);
        check("open /mw-deny is stock (ENOENT, not EACCES)", open("/mw-deny"), ENOENT);
        check("getpid works", i64::from(sc(nr::GETPID, [0; 4]) > 0), 1);
        let fd = open("/bin/mwprobe");
        let r = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]);
        check("a file-backed PROT_EXEC mmap is stock (allowed)", i64::from(r > 0), 1);
        // Dynamic loading itself works here: a dynamic executable that needs a library besides libc loads it.
        let (ok, err) = run_dynamic("/usr/bin/xz", "--version");
        println!("MWPROBE xz (dynamic, needs liblzma.so.5): ok={ok} output={err}");
        check("a dynamic child with an extra library runs (stock)", i64::from(ok), 1);
        let (ok, _) = run_dynamic("/bin/busybox", "true");
        check("a dynamic child that needs only libc runs (stock)", i64::from(ok), 1);
        println!("MWPROBE RESULT CONTROL {}", if fails.get() == 0 { "PASS" } else { "FAIL" });
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

    // "No dynamic loading" (shanghai/src/nodl.rs): what ld.so and dlopen do is a file-backed PROT_EXEC mapping.
    let fd = open("/bin/mwprobe");
    check("open the probe's own binary", i64::from(fd >= 3), 1);
    if !nodl {
        // The guards are a Shanghai feature (`no-dynamic-loading`), off by default: with only `middleware` none is in the table.
        let file_exec = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]);
        check("(no-dynamic-loading off) mmap(file, PROT_EXEC) is allowed", i64::from(file_exec > 0), 1);
        let (ok, _) = run_dynamic("/usr/bin/xz", "--version");
        check("(no-dynamic-loading off) a dynamic child with an extra library runs", i64::from(ok), 1);
        let (ok, _) = run_dynamic("/bin/busybox", "true");
        check("(no-dynamic-loading off) a dynamic child with only libc runs (the image check is absent too)", i64::from(ok), 1);
        let bad = std::process::Command::new("/bin/mwprobe").arg("kid").env("LD_PRELOAD", "/lib/evil.so").status();
        check("(no-dynamic-loading off) a child exec with LD_PRELOAD is not refused", i64::from(bad.is_ok()), 1);
    } else {
        // The avenues. The policy is one `match` over every way code arrives (shanghai/src/nodl.rs); this tries each way we know of.
        // A. execve of a dynamically linked program, either kind, is refused before the process is touched (EACCES).
        check("A1 execve of a dynamic program with an extra library (xz) is refused", i64::from(spawn_errno("/usr/bin/xz", &["--version"]) == Some(13)), 1);
        check("A2 execve of a dynamic program with only libc (busybox) is refused: the gap is closed", i64::from(spawn_errno("/bin/busybox", &["true"]) == Some(13)), 1);
        // B. The loader run directly has no PT_INTERP (it is its own interpreter), so the image check passes it; it then has to map
        //    the program with a file-backed PROT_EXEC mmap, which the mapping check refuses.
        let (ok, out) = run_dynamic(LOADER, "/bin/busybox");
        println!("MWPROBE loader-direct: ok={ok} output={out}");
        check("B  running the loader directly on a dynamic program cannot load it", i64::from(!ok), 1);
        // C. A #! script whose interpreter is dynamic: the kernel resolves the interpreter and it goes through the same image check.
        let script = "/tmp/dyn.sh";
        let _ = std::fs::write(script, "#!/bin/busybox sh\ntrue\n");
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755));
        }
        check("C  a #! script with a dynamic interpreter is refused", i64::from(spawn_errno(script, &[]) == Some(13)), 1);
        // D. Mappings.
        let file_exec = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd as u64, 0]);
        check("D1 mmap(file, PROT_EXEC) is refused", file_exec, EACCES);
        let file_ro = sc6(nr::MMAP, [0, 4096, PROT_READ, MAP_PRIVATE, fd as u64, 0]);
        check("D2 mmap(file, PROT_READ) is still allowed", i64::from(file_ro > 0), 1);
        check("D3 mprotect(file mapping, +PROT_EXEC) is refused", sc(nr::MPROTECT, [file_ro as u64, 4096, PROT_READ | PROT_EXEC, 0]), EACCES);
        check("D4 mprotect(PROT_READ) is still allowed", sc(nr::MPROTECT, [file_ro as u64, 4096, PROT_READ, 0]), 0);
        let anon_exec = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, NO_FD, 0]);
        check("D5 mmap(anonymous, PROT_EXEC) is still allowed", i64::from(anon_exec > 0), 1);
        // E. Loading by hand: put bytes in anonymous memory, then try to make it code.
        let anon = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, NO_FD, 0]);
        // SAFETY: `anon` is the fresh 4096-byte read-write mapping just made; one byte is written inside it.
        unsafe { *(anon as *mut u8) = 0xC3 };
        check("E1 anonymous memory with bytes written, then mprotect(+PROT_EXEC), is refused", sc(nr::MPROTECT, [anon as u64, 4096, PROT_READ | PROT_EXEC, 0]), EACCES);
        check("E2 mmap(anonymous, PROT_READ|WRITE|EXEC) is refused (W^X)", sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, NO_FD, 0]), EACCES);
        // F. memfd_create: an anonymous file you can write and then map. Either not served, or a mapping of it must be refused.
        let name = b"x\0";
        let m = sc(nr::MEMFD_CREATE, [name.as_ptr() as u64, 0, 0, 0]);
        if m == ENOSYS {
            check("F  memfd_create is not served (closed by absence)", m, ENOSYS);
        } else {
            let bytes = [0xC3u8; 16];
            sc(nr::WRITE, [m as u64, bytes.as_ptr() as u64, 16, 0]);
            let r = sc6(nr::MMAP, [0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, m as u64, 0]);
            check("F  memfd_create is served, and mmap(memfd, PROT_EXEC) is refused", i64::from(r < 0), 1);
        }
        // G. Other ways to put code into a process that are not served: closed by absence (a regression here is a new avenue).
        check("G1 execveat is not served", sc(nr::EXECVEAT, [0, 0, 0, 0]), ENOSYS);
        check("G2 ptrace is not served", sc(nr::PTRACE, [0, 0, 0, 0]), ENOSYS);
        check("G3 process_vm_writev is not served", sc(nr::PROCESS_VM_WRITEV, [0, 0, 0, 0]), ENOSYS);
        check("G4 /proc/self/mem cannot be opened for writing", i64::from(std::fs::OpenOptions::new().write(true).open("/proc/self/mem").is_err()), 1);

        // H. Children. The policy is the kernel's, not the process's: every process made from this one is bound by it.
        let pid = sc(nr::CLONE, [SIGCHLD, 0, 0, 0]);
        if pid == 0 {
            let n = load_attempts_allowed(fd);
            sc(nr::EXIT_GROUP, [u64::from(n != 0), 0, 0, 0]);
            loop {}
        }
        let mut status: i32 = 0;
        let waited = sc(nr::WAIT4, [pid as u64, &raw mut status as u64, 0, 0]);
        check("H1 a forked child (no exec) is bound by the same policy", i64::from(pid > 0 && waited == pid && status == 0), 1);
        let kid = std::process::Command::new("/bin/mwprobe").arg("kid").env_clear().status();
        check("H2 an exec'd static child (another pid) runs, and is bound by the same policy", i64::from(matches!(&kid, Ok(s) if s.success())), 1);
        let bad = std::process::Command::new("/bin/mwprobe").arg("kid").env("LD_PRELOAD", "/lib/evil.so").status();
        check("H3 a child exec with LD_PRELOAD is refused (EPERM, the env guard)", i64::from(matches!(&bad, Err(e) if e.raw_os_error() == Some(1))), 1);
    }

    // execve: the environment is checked before the call. LD_PRELOAD is refused; a clean environment execs us again.
    let cstr = |s: &str| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    };
    let path = cstr("/bin/mwprobe");
    let (a0, a1, a2) = (cstr("mwprobe"), cstr("child"), cstr(&fails.get().to_string()));
    let argv = [a0.as_ptr() as u64, a1.as_ptr() as u64, a2.as_ptr() as u64, 0];
    let preload = cstr("LD_PRELOAD=/lib/evil.so");
    let bad_env = [preload.as_ptr() as u64, 0];
    if nodl {
        let refused = sc(nr::EXECVE, [path.as_ptr() as u64, argv.as_ptr() as u64, bad_env.as_ptr() as u64, 0]);
        check("execve with LD_PRELOAD in the environment is refused", refused, EPERM);
    }
    let path_var = cstr("PATH=/bin");
    let good_env = [path_var.as_ptr() as u64, 0];
    // Success never returns: the new image prints the result. Getting here means the exec was refused.
    let r = sc(nr::EXECVE, [path.as_ptr() as u64, argv.as_ptr() as u64, good_env.as_ptr() as u64, 0]);
    check("execve with a clean environment is allowed", r, 0);
    println!("MWPROBE RESULT FAIL");
}
