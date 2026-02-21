//! box - Container management utility
//!
//! Usage:
//!   box open <name> [--root <dir>] [-i] [-d] [cmd] [args...]
//!   box cp <source> <destination>
//!   box ps
//!   box use <name|id> [-i] [-d] <cmd> [args...]
//!   box grab <name|id> [pid]
//!   box close <name|id>
//!   box stop <name|id>
//!   box show <name|id>
//!   box inspect <name|id>
//!   box test

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{exit, print, args, open, read_fd, write_fd, close, open_flags, SpawnResult, waitpid, println, read_dir, mkdir, fstat, mkdir_p, get_cpu_stats, ThreadCpuStat};
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use core::iter::Peekable;

#[repr(C)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
}

const SYSCALL_SPAWN_EXT: u64 = 315;
const SYSCALL_REGISTER_BOX: u64 = 316;
const SYSCALL_KILL_BOX: u64 = 317;

fn spawn_ext(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, options: &mut SpawnOptions) -> Option<SpawnResult> {
    let mut argv = Vec::new();
    let path_terminated = format!("{}\0", path);
    argv.push(path_terminated.as_ptr());
    
    let mut args_terminated = Vec::new();
    if let Some(slice) = args {
        for a in slice {
            let s = format!("{}\0", a);
            args_terminated.push(s);
        }
    }
    for s in &args_terminated {
        argv.push(s.as_ptr());
    }
    argv.push(core::ptr::null());

    options.args_ptr = argv.as_ptr() as u64;
    options.args_len = argv.len();
    
    if let Some(s) = stdin {
        options.stdin_ptr = s.as_ptr() as u64;
        options.stdin_len = s.len();
    }

    let result = libakuma::syscall(
        SYSCALL_SPAWN_EXT,
        path_terminated.as_ptr() as u64,
        options as *const _ as u64,
        0, 0, 0, 0,
    );

    if (result as i64) < 0 { return None; }
    Some(SpawnResult { pid: (result & 0xFFFF_FFFF) as u32, stdout_fd: ((result >> 32) & 0xFFFF_FFFF) as u32 })
}

#[no_mangle]
pub extern "C" fn main() {
    let mut args_iter = args();
    let _prog = args_iter.next();

    let command = match args_iter.next() {
        Some(cmd) => cmd,
        None => { print_usage(); exit(0); }
    };

    match command {
        "open" | "run" => cmd_open(args_iter),
        "cp" => cmd_cp(args_iter),
        "ps" => cmd_ps(),
        "use" | "exec" => cmd_use(args_iter),
        "grab" | "attach" => cmd_grab(args_iter),
        "close" | "stop" | "rm" => cmd_close(args_iter),
        "show" | "inspect" => cmd_show(args_iter),
        "test" => cmd_test(),
        "help" | "--help" | "-h" => { print_usage(); exit(0); }
        _ => {
            print("box: unknown command '"); print(command); print("'\n");
            print_usage(); exit(1);
        }
    }
}

fn print_usage() {
    print("box - Container management utility\n\n");
    print("Usage:\n");
    print("  box open <name> [-i] [-d] [--root <dir>] [cmd] [args...]  Start a box\n");
    print("  box use <name|id> [-i] [-d] <cmd> [args...]               Run in box\n");
    print("  box grab <name|id> [pid]                                  Reattach to process\n");
    print("  box cp <source> <dest>                                    Copy directory\n");
    print("  box ps                                                    List active boxes\n");
    print("  box close <name|id>                                       Stop a box\n");
    print("  box show <name|id>                                        Display details\n");
    print("  box test                                                  Run isolation tests\n");
}

fn resolve_target_id(target: &str) -> Option<u64> {
    // 1. Try numeric/hex ID first
    let mut id_val = 0u64;
    let mut is_hex = false;
    let mut s = target;
    if target.starts_with("0x") {
        is_hex = true;
        s = &target[2..];
    }

    if is_hex {
        for b in s.as_bytes() {
            let digit = match *b {
                b'0'..=b'9' => *b - b'0',
                b'a'..=b'f' => *b - b'a' + 10,
                b'A'..=b'F' => *b - b'A' + 10,
                _ => return None,
            };
            id_val = (id_val << 4) | digit as u64;
        }
        return Some(id_val);
    } else {
        // Try parsing as decimal, but only if all chars are digits
        let mut all_digits = true;
        for b in s.as_bytes() {
            if *b < b'0' || *b > b'9' { all_digits = false; break; }
            id_val = id_val * 10 + (*b - b'0') as u64;
        }
        if all_digits && !target.is_empty() { return Some(id_val); }
    }

    // 2. Try lookup by name in /proc/boxes
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd >= 0 {
        let mut buf = [0u8; 2048];
        let n = read_fd(fd, &mut buf);
        close(fd);
        if n > 0 {
            let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
            for line in content.lines().skip(1) {
                let mut parts = line.split(',');
                let id_str = parts.next().unwrap_or("");
                let bname = parts.next().unwrap_or("");
                if bname == target {
                    let mut found_id = 0u64;
                    for b in id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { found_id = found_id * 10 + (*b - b'0') as u64; } }
                    return Some(found_id);
                }
            }
        }
    }
    None
}

fn get_target_root(target_id: u64) -> Option<String> {
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd >= 0 {
        let mut buf = [0u8; 2048];
        let n = read_fd(fd, &mut buf);
        close(fd);
        if n > 0 {
            let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
            for line in content.lines().skip(1) {
                let mut parts = line.split(',');
                let id_str = parts.next().unwrap_or("");
                let _bname = parts.next().unwrap_or("");
                let root = parts.next().unwrap_or("");
                // ... skip creator, primary
                
                let mut found_id = 0u64;
                for b in id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { found_id = found_id * 10 + (*b - b'0') as u64; } }
                if found_id == target_id { return Some(String::from(root)); }
            }
        }
    }
    None
}

fn cmd_open(args: libakuma::Args) -> ! {
    let mut args = args.peekable();
    let name = match args.next() {
        Some(n) => n,
        None => { print("Usage: box open <name> [-i] [-d] [--root <dir>] [cmd] [args...]\n"); exit(1); }
    };

    let mut directory = String::from("/");
    let mut interactive = false;
    let mut detached = false;
    let mut cmd_path = None;
    let mut cmd_args = Vec::new();

    while let Some(arg) = args.next() {
        if arg == "--root" || arg == "-r" {
            directory = String::from(args.next().unwrap_or("/"));
        } else if arg == "-i" || arg == "--interactive" {
            interactive = true;
        } else if arg == "-d" || arg == "--detached" {
            detached = true;
        } else {
            cmd_path = Some(arg);
            for a in args { cmd_args.push(a); }
            break;
        }
    }

    let mut box_id = 0u64;
    for b in name.as_bytes() { box_id = box_id.wrapping_mul(31).wrapping_add(*b as u64); }
    if box_id == 0 { box_id = 1; }

    libakuma::syscall(SYSCALL_REGISTER_BOX, box_id, name.as_ptr() as u64, name.len() as u64, directory.as_ptr() as u64, directory.len() as u64, 0);

    if let Some(path) = cmd_path {
        let mut options = SpawnOptions {
            cwd_ptr: "/".as_ptr() as u64, cwd_len: 1,
            root_dir_ptr: directory.as_ptr() as u64, root_dir_len: directory.len(),
            args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0, box_id,
        };

        print("box: starting '"); print(name); print("' in "); print(&directory); print(" (ID="); libakuma::print_hex(box_id as usize); print(")\n");

        let args_opt = if cmd_args.is_empty() { None } else { Some(cmd_args.as_slice()) };

        match spawn_ext(path, args_opt, None, &mut options) {
            Some(res) => {
                // Update registry with real primary PID
                libakuma::syscall(SYSCALL_REGISTER_BOX, box_id, name.as_ptr() as u64, name.len() as u64, directory.as_ptr() as u64, directory.len() as u64, res.pid as u64);

                if detached {
                    println(&format!("Started PID {} in detached mode. (Log persistence TBD)", res.pid));
                    exit(0);
                }

                if interactive {
                    if libakuma::reattach(res.pid) != 0 {
                        print("box: reattach failed\n");
                        exit(1);
                    }
                }

                loop {
                    if let Some((_, code)) = waitpid(res.pid) { exit(code); }
                    libakuma::sleep_ms(100);
                }
            }
            None => { print("box open: failed to spawn\n"); exit(1); }
        }
    } else {
        print("box: created empty box '"); print(name); print("' (ID="); libakuma::print_hex(box_id as usize); print(")\n");
        exit(0);
    }
}

fn cmd_use(args: libakuma::Args) -> ! {
    let mut args = args.peekable();
    let target = match args.next() {
        Some(t) => t,
        None => { print("Usage: box use <name|id> [-i] [-d] <cmd> [args...]\n"); exit(1); }
    };

    let target_id = resolve_target_id(target).unwrap_or_else(|| {
        print("box use: target not found\n"); exit(1);
    });

    let target_root = get_target_root(target_id).unwrap_or_else(|| {
        String::from("/")
    });

    let mut interactive = false;
    let mut detached = false;
    let mut cmd_path = None;
    let mut cmd_args = Vec::new();

    while let Some(arg) = args.next() {
        if arg == "-i" || arg == "--interactive" {
            interactive = true;
        } else if arg == "-d" || arg == "--detached" {
            detached = true;
        } else {
            cmd_path = Some(arg);
            for a in args { cmd_args.push(a); }
            break;
        }
    }

    let path = cmd_path.unwrap_or_else(|| {
        print("box use: missing command\n"); exit(1);
    });

    let mut options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64, cwd_len: 1,
        root_dir_ptr: target_root.as_ptr() as u64, root_dir_len: target_root.len(),
        args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0, box_id: target_id,
    };

    let args_opt = if cmd_args.is_empty() { None } else { Some(cmd_args.as_slice()) };

    match spawn_ext(path, args_opt, None, &mut options) {
        Some(res) => {
            if detached {
                println(&format!("Injected PID {} (detached)", res.pid));
                exit(0);
            }

            if interactive {
                if libakuma::reattach(res.pid) != 0 {
                    print("box use: reattach failed\n");
                    exit(1);
                }
                loop {
                    if let Some((_, code)) = waitpid(res.pid) { exit(code); }
                    libakuma::sleep_ms(100);
                }
            } else {
                println(&format!("Injected PID {}", res.pid));
                exit(0);
            }
        }
        None => { print("box use: failed\n"); exit(1); }
    }
}

fn cmd_grab(mut args: libakuma::Args) -> ! {
    let target = match args.next() {
        Some(t) => t,
        None => { print("Usage: box grab <name|id> [pid]\n"); exit(1); }
    };

    let target_id = resolve_target_id(target).unwrap_or_else(|| {
        print("box grab: target box not found\n"); exit(1);
    });

    let specific_pid = args.next().and_then(|s| {
        let mut p = 0u32;
        for b in s.as_bytes() { if *b >= b'0' && *b <= b'9' { p = p * 10 + (*b - b'0') as u32; } }
        if p > 0 { Some(p) } else { None }
    });

    let pid_to_grab = if let Some(p) = specific_pid {
        p
    } else {
        // Find first process in this box from top stats
        let mut stats: [ThreadCpuStat; 64] = [ThreadCpuStat::default(); 64];
        let count = get_cpu_stats(&mut stats);
        let mut found = None;
        for i in 0..count {
            if stats[i].state != 0 && stats[i].box_id == target_id && stats[i].pid > 1 {
                found = Some(stats[i].pid);
                break;
            }
        }
        found.unwrap_or_else(|| {
            print("box grab: no processes found in box\n"); exit(1);
        })
    };

    print("box: grabbing PID "); libakuma::print_dec(pid_to_grab as usize); print("\n");
    if libakuma::reattach(pid_to_grab) == 0 {
        loop {
            if let Some((_, code)) = waitpid(pid_to_grab) { exit(code); }
            libakuma::sleep_ms(100);
        }
    } else {
        print("box grab: failed to reattach\n");
        exit(1);
    }
}

fn copy_file(src: &str, dst: &str) -> bool {
    let sfd = open(src, open_flags::O_RDONLY);
    if sfd < 0 { return false; }
    let mut success = false;
    if let Ok(stat) = fstat(sfd) {
        let size = stat.st_size as usize;
        let mut buf = Vec::with_capacity(size);
        buf.resize(size, 0);
        
        let mut total_read = 0;
        while total_read < size {
            let n = read_fd(sfd, &mut buf[total_read..]);
            if n <= 0 { break; }
            total_read += n as usize;
        }

        let dfd = open(dst, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if dfd >= 0 {
            if total_read > 0 { let _ = write_fd(dfd, &buf[..total_read]); }
            close(dfd);
            success = total_read == size;
        }
    }
    close(sfd);
    success
}

fn copy_recursive(src: &str, dst: &str) {
    if let Some(entries) = read_dir(src) {
        for entry in entries {
            let src_path = format!("{}/{}", src, entry.name);
            let dst_path = format!("{}/{}", dst, entry.name);
            if entry.is_dir {
                let _ = mkdir(&dst_path);
                copy_recursive(&src_path, &dst_path);
            } else {
                copy_file(&src_path, &dst_path);
            }
        }
    }
}

fn cmd_cp(mut args: libakuma::Args) -> ! {
    let src = match args.next() { Some(s) => s, None => { print("Usage: box cp <src> <dest>\n"); exit(1); } };
    let dst = match args.next() { Some(d) => d, None => { print("Usage: box cp <src> <dest>\n"); exit(1); } };
    print("box: copying "); print(src); print(" to "); print(dst); print("...\n");
    let _ = mkdir_p(dst);
    copy_recursive(src, dst);
    exit(0);
}

fn cmd_ps() -> ! {
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd < 0 { print("box ps: failed to open /proc/boxes\n"); exit(1); }
    let mut buf = [0u8; 2048];
    let n = read_fd(fd, &mut buf);
    close(fd);

    if n > 0 {
        let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
        println("  ID            NAME        ROOT        CREATOR     PRIMARY");
        println("  ---------------------------------------------------------");
        for line in content.lines().skip(1) {
            let mut parts = line.split(',');
            let id_str = parts.next().unwrap_or("");
            let name = parts.next().unwrap_or("");
            let root = parts.next().unwrap_or("");
            let creator = parts.next().unwrap_or("");
            let primary = parts.next().unwrap_or("-");
            
            let mut id_val = 0u64;
            for b in id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { id_val = id_val * 10 + (*b - b'0') as u64; } }
            let id_hex = if id_val == 0 { String::from("0") } else { format!("{:08x}", id_val) };

            println(&format!("  {:<12}  {:<10}  {:<10}  {:<10}  {}", id_hex, name, root, creator, primary));
        }
    } else {
        println("No active boxes found.");
    }
    exit(0);
}

fn cmd_close(mut args: libakuma::Args) -> ! {
    let target = match args.next() { Some(t) => t, None => { print("Usage: box close <name|id>\n"); exit(1); } };
    let box_id = resolve_target_id(target).unwrap_or_else(|| {
        print("box close: box not found\n"); exit(1);
    });

    if box_id == 0 { print("box close: cannot kill Box 0 (Host)\n"); exit(1); }
    if libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0) == 0 { 
        print("Closed box "); libakuma::print_hex(box_id as usize); print("\n"); 
        exit(0); 
    } else { 
        print("box close: failed\n"); exit(1); 
    }
}

fn cmd_show(mut args: libakuma::Args) -> ! {
    let target = match args.next() { Some(t) => t, None => { print("Usage: box show <name|id>\n"); exit(1); } };
    let target_id = resolve_target_id(target).unwrap_or_else(|| {
        print("box show: box not found\n"); exit(1);
    });

    let mut box_name = String::new();
    let mut box_root = String::new();
    let mut box_creator = String::new();

    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd >= 0 {
        let mut buf = [0u8; 2048];
        let n = read_fd(fd, &mut buf);
        close(fd);
        if n > 0 {
            let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
            for line in content.lines().skip(1) {
                let mut parts = line.split(',');
                let id_str = parts.next().unwrap_or("");
                let bname = parts.next().unwrap_or("");
                let root = parts.next().unwrap_or("");
                let creator = parts.next().unwrap_or("");
                
                let mut found_id = 0u64;
                for b in id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { found_id = found_id * 10 + (*b - b'0') as u64; } }
                if found_id == target_id {
                    box_name = String::from(bname);
                    box_root = String::from(root);
                    box_creator = String::from(creator);
                    break;
                }
            }
        }
    }

    println(&format!("Box ID: {:08x}", target_id));
    println(&format!("Name:   {}", box_name));
    println(&format!("Root:   {}", box_root));
    println(&format!("Creator PID: {}", box_creator));
    println("\nMembers:");
    
    let mut stats: [ThreadCpuStat; 64] = [ThreadCpuStat::default(); 64];
    let count = get_cpu_stats(&mut stats);
    let mut found = false;
    for i in 0..count {
        if stats[i].state != 0 && stats[i].box_id == target_id {
            let mut name_len = 0;
            while name_len < 16 && stats[i].name[name_len] >= 32 && stats[i].name[name_len] < 127 { name_len += 1; }
            let name = core::str::from_utf8(&stats[i].name[..name_len]).unwrap_or("?");
            println(&format!("  PID {:>3}  {}", stats[i].pid, name));
            found = true;
        }
    }
    if !found { println("  (none)"); }
    exit(0);
}

fn cmd_test() -> ! {
    println("--- Running Box Isolation Tests (Userspace) ---");

    print("[Test 1] Blind Root Redirection... ");
    let test_dir = "/tmp/boxtest";
    let _ = mkdir_p(test_dir);
    let _ = mkdir(&format!("{}/bin", test_dir));
    
    if !copy_file("/bin/cat", &format!("{}/bin/cat", test_dir)) {
        println("FAILED: Could not copy /bin/cat to test dir"); exit(1);
    }

    let test_file = format!("{}/test.txt", test_dir);
    let test_content = "Akuma Container Test 123";
    let fd = open(&test_file, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd >= 0 { let _ = write_fd(fd, test_content.as_bytes()); close(fd); }

    let box_id = 0x7E571;
    let mut options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64, cwd_len: 1,
        root_dir_ptr: test_dir.as_ptr() as u64, root_dir_len: test_dir.len(),
        args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0, box_id,
    };

    let args = ["/test.txt"];
    match spawn_ext("/bin/cat", Some(&args), None, &mut options) {
        Some(res) => {
            let mut output = Vec::new();
            loop {
                let mut buf = [0u8; 256];
                let n = read_fd(res.stdout_fd as i32, &mut buf);
                if n > 0 { output.extend_from_slice(&buf[..n as usize]); }
                if let Some((_, _)) = waitpid(res.pid) { 
                    loop {
                        let n = read_fd(res.stdout_fd as i32, &mut buf);
                        if n <= 0 { break; }
                        output.extend_from_slice(&buf[..n as usize]);
                    }
                    break; 
                }
                libakuma::sleep_ms(10);
            }
            if core::str::from_utf8(&output).unwrap_or("").contains(test_content) { println("PASSED"); }
            else {
                println("FAILED: Output did not match");
                print("Got: "); println(core::str::from_utf8(&output).unwrap_or("")); exit(1);
            }
        }
        None => { println("FAILED: Could not spawn test process"); exit(1); }
    }

    print("[Test 2] ProcFS Isolation... ");
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd >= 0 { println("PASSED"); close(fd); }
    else { println("FAILED: Cannot read /proc/boxes from host"); exit(1); }

    print("[Test 3] Stdin/Stdout Pipeline... ");
    let pipe_test_dir = "/tmp/pipetest";
    let _ = mkdir_p(pipe_test_dir);
    let _ = mkdir(&format!("{}/bin", pipe_test_dir));
    if !copy_file("/bin/cat", &format!("{}/bin/cat", pipe_test_dir)) {
        println("FAILED: Could not copy /bin/cat"); exit(1);
    }

    let mut pipe_options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64, cwd_len: 1,
        root_dir_ptr: pipe_test_dir.as_ptr() as u64, root_dir_len: pipe_test_dir.len(),
        args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0,
        box_id: 0x919E,
    };

    let pipe_input = "Hello through the pipe!";
    match spawn_ext("/bin/cat", None, Some(pipe_input.as_bytes()), &mut pipe_options) {
        Some(res) => {
            let mut output = Vec::new();
            let start_time = libakuma::uptime();
            loop {
                let mut buf = [0u8; 256];
                let n = read_fd(res.stdout_fd as i32, &mut buf);
                if n > 0 { output.extend_from_slice(&buf[..n as usize]); }
                
                if let Some((_, _)) = waitpid(res.pid) { 
                    loop {
                        let n = read_fd(res.stdout_fd as i32, &mut buf);
                        if n <= 0 { break; }
                        output.extend_from_slice(&buf[..n as usize]);
                    }
                    break; 
                }

                if libakuma::uptime() - start_time > 2_000_000 {
                    println("FAILED: Timeout (2s)");
                    libakuma::kill(res.pid);
                    exit(1);
                }
                libakuma::sleep_ms(10);
            }
            if core::str::from_utf8(&output).unwrap_or("").contains(pipe_input) { println("PASSED"); }
            else {
                println("FAILED: Output did not match");
                print("Got: "); println(core::str::from_utf8(&output).unwrap_or("")); exit(1);
            }
        }
        None => { println("FAILED: Could not spawn"); exit(1); }
    }

    println("--- All Tests Passed ---");
    exit(0);
}
