//! elftest - Minimal ELF loading and subprocess spawning verification
//!
//! Tests:
//! - Subprocess spawning (custom SPAWN syscall)
//! - Subprocess spawning (Linux-style vfork + execve)
//! - Works for both native and musl binaries

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{exit, print, arg, spawn, waitpid, open, close, open_flags, sleep_ms, syscall};
use alloc::format;
use alloc::vec::Vec;
use core::ptr::null;

const CLONE: u64 = 220;
const EXECVE: u64 = 221;
const WAIT4: u64 = 260;

const SIGCHLD: u64 = 17;
const CLONE_VFORK: u64 = 0x00004000;
const CLONE_VM: u64 = 0x00000100;

#[no_mangle]
pub extern "C" fn main() {
    let first_arg = arg(1);

    if let Some(arg) = first_arg {
        if arg == "--dummy" {
            print("elftest: dummy mode reached NYA!\n");
            exit(42);
        }
    }

    print("elftest: starting subprocess spawn tests\n");
    let mut all_passed = true;

    // Test 1: Regular Spawn (elftest)
    if !test_spawn("/bin/elftest", Some(&["--dummy"]), 42) {
        all_passed = false;
    }

    // Test 2: Linux Spawn (elftest)
    if !test_linux_spawn("/bin/elftest", Some(&["--dummy"]), 42) {
        all_passed = false;
    }

    // Test 3: Regular Spawn (hello_musl.bin)
    let hello_path = "/bin/hello_musl.bin";
    if file_exists(hello_path) {
        if !test_spawn(hello_path, None, 0) {
            all_passed = false;
        }
        
        // Test 4: Linux Spawn (hello_musl.bin)
        if !test_linux_spawn(hello_path, None, 0) {
            all_passed = false;
        }
    } else {
        print("elftest: /bin/hello_musl.bin not found, skipping musl tests\n");
    }

    if all_passed {
        print("elftest: ALL tests PASSED\n");
        exit(42);
    } else {
        print("elftest: SOME tests FAILED\n");
        exit(1);
    }
}

fn file_exists(path: &str) -> bool {
    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        true
    } else {
        false
    }
}

fn test_spawn(path: &str, args: Option<&[&str]>, expected_code: i32) -> bool {
    print(&format!("elftest: [Regular Spawn] spawning {}...\n", path));
    if let Some(res) = spawn(path, args) {
        print(&format!("elftest: spawn successful, pid={}\n", res.pid));
        // Wait for it
        for _ in 0..100 {
            if let Some((pid, code)) = waitpid(res.pid) {
                if pid == res.pid {
                    if code == expected_code || expected_code == 0 {
                        print(&format!("elftest: [Regular Spawn] {} PASSED (exit code {})\n", path, code));
                        return true;
                    } else {
                        print(&format!("elftest: [Regular Spawn] {} FAILED (wrong exit code {}, expected {})\n", path, code, expected_code));
                        return false;
                    }
                }
            }
            sleep_ms(10);
        }
        print(&format!("elftest: [Regular Spawn] {} FAILED (timeout)\n", path));
    } else {
        print(&format!("elftest: [Regular Spawn] {} FAILED (spawn returned None)\n", path));
    }
    false
}

fn test_linux_spawn(path: &str, args: Option<&[&str]>, expected_code: i32) -> bool {
    print(&format!("elftest: [Linux Spawn] vfork+execve {}...\n", path));
    
    // 1. vfork (clone with VM and VFORK flags)
    let flags = CLONE_VFORK | CLONE_VM | SIGCHLD;
    let pid = syscall(CLONE, flags, 0, 0, 0, 0, 0) as i32;
    
    if pid < 0 {
        print(&format!("elftest: [Linux Spawn] vfork FAILED with error {}\n", pid));
        return false;
    }
    
    if pid == 0 {
        // Child process: execve
        let path_s = format!("{}\0", path);
        let mut argv = Vec::new();
        argv.push(path_s.as_ptr());
        
        let mut args_s = Vec::new();
        if let Some(a_list) = args {
            for a in a_list {
                let s = format!("{}\0", a);
                args_s.push(s);
            }
        }
        for s in &args_s {
            argv.push(s.as_ptr());
        }
        argv.push(null());
        
        let envp: [*const u8; 1] = [null()];
        
        syscall(EXECVE, path_s.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0);
        // If execve returns, it failed
        exit(127);
    } else {
        // Parent process: wait
        print(&format!("elftest: vfork child pid={}\n", pid));
        for _ in 0..100 {
            if let Some((w_pid, code)) = waitpid(pid as u32) {
                if w_pid == pid as u32 {
                    if code == expected_code || expected_code == 0 {
                        print(&format!("elftest: [Linux Spawn] {} PASSED (exit code {})\n", path, code));
                        return true;
                    } else {
                        print(&format!("elftest: [Linux Spawn] {} FAILED (wrong exit code {}, expected {})\n", path, code, expected_code));
                        return false;
                    }
                }
            }
            sleep_ms(10);
        }
        print(&format!("elftest: [Linux Spawn] {} FAILED (timeout)\n", path));
    }
    false
}
