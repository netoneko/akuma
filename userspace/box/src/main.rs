//! box - Container management utility
//!
//! Usage:
//!   box open <name> [--directory <dir>] [--interactive|-i] [cmd]
//!   box run <name> [-i] [-d <dir>] [cmd]
//!   box cp <source> <destination>
//!   box ps
//!   box use <name> [-i] <cmd>
//!   box exec <name> [-i] <cmd>
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
    let mut args_buf = Vec::new();
    if let Some(args_slice) = args {
        for arg in args_slice {
            args_buf.extend_from_slice(arg.as_bytes());
            args_buf.push(0);
        }
    }

    if !args_buf.is_empty() {
        options.args_ptr = args_buf.as_ptr() as u64;
        options.args_len = args_buf.len();
    }
    
    if let Some(s) = stdin {
        options.stdin_ptr = s.as_ptr() as u64;
        options.stdin_len = s.len();
    }

    let result = libakuma::syscall(
        SYSCALL_SPAWN_EXT,
        path.as_ptr() as u64,
        path.len() as u64,
        options as *const _ as u64,
        0, 0, 0,
    );

    if (result as i64) < 0 { return None; }
    Some(SpawnResult { pid: (result & 0xFFFF_FFFF) as u32, stdout_fd: ((result >> 32) & 0xFFFF_FFFF) as u32 })
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
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
    print("  box open <name> [-i] [-d <dir>] [cmd]     Start a new box (or empty box)\n");
    print("  box cp <source> <dest>                    Copy directory recursively\n");
    print("  box ps                                    List active boxes\n");
    print("  box use <name> [-i] <cmd>                 Run command in existing box\n");
    print("  box close <name|id>                       Stop all processes in a box\n");
    print("  box show <name|id>                        Display box details\n");
    print("  box test                                  Run isolation tests\n");
}

fn cmd_open(mut args: libakuma::Args) -> ! {
    let name = match args.next() {
        Some(n) => n,
        None => { print("Usage: box open <name> [-i] [-d <dir>] [cmd]\n"); exit(1); }
    };

    let mut directory = String::from("/");
    let mut interactive = false;
    let mut cmd_path = None;
    let mut cmd_args = Vec::new();

    while let Some(arg) = args.next() {
        if arg == "--directory" || arg == "-d" {
            directory = String::from(args.next().unwrap_or("/"));
        } else if arg == "--interactive" || arg == "-i" {
            interactive = true;
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

        let cmd_args_refs: Vec<&str> = cmd_args.iter().map(|s| *s).collect();
        let args_opt = if cmd_args_refs.is_empty() { None } else { Some(cmd_args_refs.as_slice()) };

        match spawn_ext(path, args_opt, None, &mut options) {
            Some(res) => {
                print("Started PID "); libakuma::print_dec(res.pid as usize); print("\n");
                
                if interactive {
                    if libakuma::reattach(res.pid) != 0 {
                        print("box: reattach failed\n");
                        exit(1);
                    }
                }

                loop {
                    if let Some((_, code)) = waitpid(res.pid) {
                        exit(code);
                    }
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
            success = (total_read == size);
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
        println("  ID            NAME        ROOT        CREATOR");
        println("  ---------------------------------------------");
        for line in content.lines().skip(1) {
            let mut parts = line.split(',');
            let id_str = parts.next().unwrap_or("");
            let name = parts.next().unwrap_or("");
            let root = parts.next().unwrap_or("");
            let creator = parts.next().unwrap_or("");
            
            let mut id_val = 0u64;
            for b in id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { id_val = id_val * 10 + (*b - b'0') as u64; } }
            let id_hex = if id_val == 0 { String::from("0") } else { format!("{:08x}", id_val) };

            println(&format!("  {:<12}  {:<10}  {:<10}  {}", id_hex, name, root, creator));
        }
    } else {
        println("No active boxes found.");
    }
    exit(0);
}

fn cmd_use(mut args: libakuma::Args) -> ! {
    let mut interactive = false;
    let mut target_name = None;

    while let Some(arg) = args.next() {
        if arg == "--interactive" || arg == "-i" { interactive = true; }
        else { target_name = Some(arg); break; }
    }

    let name = match target_name { Some(n) => n, None => { print("Usage: box use [-i] <name> <cmd>\n"); exit(1); } };
    let path = match args.next() { Some(p) => p, None => { print("box use: missing command\n"); exit(1); } };
    let mut cmd_args = Vec::new();
    for a in args { cmd_args.push(a); }

    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd < 0 { print("box use: failed to access /proc/boxes\n"); exit(1); }
    let mut buf = [0u8; 2048];
    let n = read_fd(fd, &mut buf);
    close(fd);
    
    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
    let mut target_id = None;
    let mut target_root = None;
    for line in content.lines().skip(1) {
        let mut parts = line.split(',');
        let id_str = parts.next().unwrap_or("");
        let bname = parts.next().unwrap_or("");
        let root = parts.next().unwrap_or("");
        if bname == name { target_id = Some(id_str); target_root = Some(root); break; }
    }

    let box_id_str = target_id.unwrap_or_else(|| { print("box use: box not found\n"); exit(1); });
    let mut box_id = 0u64;
    for b in box_id_str.as_bytes() { if *b >= b'0' && *b <= b'9' { box_id = box_id * 10 + (*b - b'0') as u64; } }

    let mut options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64, cwd_len: 1,
        root_dir_ptr: target_root.unwrap().as_ptr() as u64, root_dir_len: target_root.unwrap().len(),
        args_ptr: 0, args_len: 0, stdin_ptr: 0, stdin_len: 0, box_id,
    };

    let cmd_args_refs: Vec<&str> = cmd_args.iter().map(|s| *s).collect();
    let args_opt = if cmd_args_refs.is_empty() { None } else { Some(cmd_args_refs.as_slice()) };

    match spawn_ext(path, args_opt, None, &mut options) {
        Some(res) => {
            if interactive {
                if libakuma::reattach(res.pid) != 0 {
                    print("box use: reattach failed\n");
                    exit(1);
                }
                loop {
                    if let Some((_, code)) = waitpid(res.pid) {
                        exit(code);
                    }
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

fn cmd_close(mut args: libakuma::Args) -> ! {
    let target = match args.next() { Some(t) => t, None => { print("Usage: box close <name|id>\n"); exit(1); } };
    let mut box_id = 0u64;
    let mut is_numeric = true;
    for b in target.as_bytes() { if *b < b'0' || *b > b'9' { is_numeric = false; break; } box_id = box_id * 10 + (*b - b'0') as u64; }

    if !is_numeric {
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
                    if parts.next().unwrap_or("") == target {
                        box_id = 0;
                        for b in id_str.as_bytes() { box_id = box_id * 10 + (*b - b'0') as u64; }
                        break;
                    }
                }
            }
        }
    }

    if box_id == 0 { print("box close: box not found or Box 0\n"); exit(1); }
    if libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0) == 0 { print("Closed box "); libakuma::print_dec(box_id as usize); print("\n"); exit(0); }
    else { print("box close: failed\n"); exit(1); }
}

fn cmd_show(mut args: libakuma::Args) -> ! {
    let target = match args.next() { Some(t) => t, None => { print("Usage: box show <name|id>\n"); exit(1); } };
    let mut box_id = 0u64;
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
                if bname == target || id_str == target {
                    box_id = 0; for b in id_str.as_bytes() { box_id = box_id * 10 + (*b - b'0') as u64; }
                    box_name = String::from(bname);
                    box_root = String::from(root);
                    box_creator = String::from(creator);
                    break;
                }
            }
        }
    }

    if box_id == 0 && target != "0" && target != "host" { print("box show: box not found\n"); exit(1); }
    
    println(&format!("Box ID: {:08x}", box_id));
    println(&format!("Name:   {}", box_name));
    println(&format!("Root:   {}", box_root));
    println(&format!("Creator PID: {}", box_creator));
    println("\nMembers:");
    
    let mut stats: [ThreadCpuStat; 64] = [ThreadCpuStat::default(); 64];
    let count = get_cpu_stats(&mut stats);
    let mut found = false;
    for i in 0..count {
        if stats[i].state != 0 && stats[i].box_id == box_id {
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
