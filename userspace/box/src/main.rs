//! box - Container management utility
//!
//! Usage:
//!   box open <name> [--directory <dir>] <cmd>
//!   box cp <source> <name>
//!   box ps
//!   box use <name> <cmd>

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{exit, print, args, open, read_fd, close, open_flags, SpawnResult, waitpid, println};
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;

#[repr(C)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub box_id: u64,
}

const SYSCALL_SPAWN_EXT: u64 = 315;
const SYSCALL_REGISTER_BOX: u64 = 316;

fn spawn_ext(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, options: &SpawnOptions) -> Option<SpawnResult> {
    // Build null-separated args string
    let mut args_buf = Vec::new();
    if let Some(args_slice) = args {
        for arg in args_slice {
            args_buf.extend_from_slice(arg.as_bytes());
            args_buf.push(0);
        }
    }

    let args_ptr = if args_buf.is_empty() { 0 } else { args_buf.as_ptr() as u64 };
    let args_len = args_buf.len();
    
    let stdin_ptr = stdin.map(|s| s.as_ptr() as u64).unwrap_or(0);
    let stdin_len = stdin.map(|s| s.len() as u64).unwrap_or(0);

    let result = libakuma::syscall(
        SYSCALL_SPAWN_EXT,
        path.as_ptr() as u64,
        path.len() as u64,
        stdin_ptr,
        stdin_len,
        options as *const _ as u64,
        0,
    );

    // Check for error (negative value)
    if (result as i64) < 0 {
        return None;
    }

    // Extract PID (low 32 bits) and stdout_fd (high 32 bits)
    let pid = (result & 0xFFFF_FFFF) as u32;
    let stdout_fd = ((result >> 32) & 0xFFFF_FFFF) as u32;

    Some(SpawnResult { pid, stdout_fd })
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut args_iter = args();
    let _prog = args_iter.next(); // Skip program name

    let command = match args_iter.next() {
        Some(cmd) => cmd,
        None => {
            print_usage();
            exit(1);
        }
    };

    match command {
        "open" => cmd_open(args_iter),
        "cp" => cmd_cp(args_iter),
        "ps" => cmd_ps(),
        "use" => cmd_use(args_iter),
        "help" | "--help" | "-h" => {
            print_usage();
            exit(0);
        }
        _ => {
            print("box: unknown command '");
            print(command);
            print("'\n");
            print_usage();
            exit(1);
        }
    }
}

fn print_usage() {
    print("box - Container management utility\n\n");
    print("Usage:\n");
    print("  box open <name> [--directory <dir>] <cmd>  Start a new box\n");
    print("  box cp <source> <name>                     Initialize box directory\n");
    print("  box ps                                     List active boxes\n");
    print("  box use <name> <cmd>                       Run command in existing box\n");
}

fn cmd_open(mut args: libakuma::Args) -> ! {
    let name = match args.next() {
        Some(n) => n,
        None => {
            print("Usage: box open <name> [--directory <dir>] <cmd>\n");
            exit(1);
        }
    };

    let mut directory = String::from("/");
    let mut cmd_path = None;
    let mut cmd_args = Vec::new();

    while let Some(arg) = args.next() {
        if arg == "--directory" || arg == "-d" {
            directory = String::from(args.next().unwrap_or("/"));
        } else {
            cmd_path = Some(arg);
            // Remaining args are command args
            for a in args {
                cmd_args.push(a);
            }
            break;
        }
    }

    let path = match cmd_path {
        Some(p) => p,
        None => {
            print("box open: missing command\n");
            exit(1);
        }
    };

    // Generate a pseudo-random box_id based on name hash
    let mut box_id = 0u64;
    for b in name.as_bytes() {
        box_id = box_id.wrapping_mul(31).wrapping_add(*b as u64);
    }
    if box_id == 0 { box_id = 1; }

    // Register box in kernel
    libakuma::syscall(
        SYSCALL_REGISTER_BOX,
        box_id,
        name.as_ptr() as u64,
        name.len() as u64,
        directory.as_ptr() as u64,
        directory.len() as u64,
        0,
    );

    let options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64,
        cwd_len: 1,
        root_dir_ptr: directory.as_ptr() as u64,
        root_dir_len: directory.len(),
        box_id,
    };

    print("box: starting '");
    print(name);
    print("' in ");
    print(&directory);
    print(" (ID=");
    libakuma::print_dec(box_id as usize);
    print(")\n");

    let cmd_args_refs: Vec<&str> = cmd_args.iter().map(|s| *s).collect();
    let args_opt = if cmd_args_refs.is_empty() { None } else { Some(cmd_args_refs.as_slice()) };

    match spawn_ext(path, args_opt, None, &options) {
        Some(res) => {
            print("Started PID ");
            libakuma::print_dec(res.pid as usize);
            print("\n");
            
            // Wait for it
            loop {
                if let Some((_, code)) = waitpid(res.pid) {
                    print("Box exited with code ");
                    libakuma::print_dec(code as usize);
                    print("\n");
                    exit(code);
                }
                libakuma::sleep_ms(100);
            }
        }
        None => {
            print("box open: failed to spawn command\n");
            exit(1);
        }
    }
}

fn cmd_cp(mut _args: libakuma::Args) -> ! {
    print("box cp: not implemented (use kernel-side setup for now)\n");
    exit(1);
}

fn cmd_ps() -> ! {
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd < 0 {
        print("box ps: failed to open /proc/boxes (are you Box 0?)\n");
        exit(1);
    }

    let mut buf = [0u8; 1024];
    let n = read_fd(fd, &mut buf);
    if n > 0 {
        libakuma::write(libakuma::fd::STDOUT, &buf[..n as usize]);
    }
    close(fd);
    exit(0);
}

fn cmd_use(mut args: libakuma::Args) -> ! {
    let name = match args.next() {
        Some(n) => n,
        None => {
            print("Usage: box use <name> <cmd>\n");
            exit(1);
        }
    };

    let path = match args.next() {
        Some(p) => p,
        None => {
            print("box use: missing command\n");
            exit(1);
        }
    };

    let mut cmd_args = Vec::new();
    for a in args {
        cmd_args.push(a);
    }

    // Find box info from /proc/boxes
    let fd = open("/proc/boxes", open_flags::O_RDONLY);
    if fd < 0 {
        print("box use: failed to access /proc/boxes\n");
        exit(1);
    }

    let mut buf = [0u8; 2048];
    let n = read_fd(fd, &mut buf);
    close(fd);

    if n <= 0 {
        print("box use: no boxes found\n");
        exit(1);
    }

    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
    let mut target_id = None;
    let mut target_root = None;

    // Skip header and find box
    for line in content.lines().skip(1) {
        let mut parts = line.split(',');
        let id_str = parts.next().unwrap_or("");
        let bname = parts.next().unwrap_or("");
        let root = parts.next().unwrap_or("");
        
        if bname == name {
            target_id = Some(id_str);
            target_root = Some(root);
            break;
        }
    }

    let box_id_str = match target_id {
        Some(id) => id,
        None => {
            print("box use: box '");
            print(name);
            print("' not found\n");
            exit(1);
        }
    };

    // Simple parse_u64
    let mut box_id = 0u64;
    for b in box_id_str.as_bytes() {
        if *b >= b'0' && *b <= b'9' {
            box_id = box_id * 10 + (*b - b'0') as u64;
        }
    }

    let options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64,
        cwd_len: 1,
        root_dir_ptr: target_root.unwrap().as_ptr() as u64,
        root_dir_len: target_root.unwrap().len(),
        box_id,
    };

    let cmd_args_refs: Vec<&str> = cmd_args.iter().map(|s| *s).collect();
    let args_opt = if cmd_args_refs.is_empty() { None } else { Some(cmd_args_refs.as_slice()) };

    match spawn_ext(path, args_opt, None, &options) {
        Some(res) => {
            println(&format!("Injected command into box '{}' (PID {})", name, res.pid));
            exit(0);
        }
        None => {
            print("box use: injection failed\n");
            exit(1);
        }
    }
}
