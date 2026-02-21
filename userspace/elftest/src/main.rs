//! elftest - Minimal ELF loading and subprocess spawning verification
//!
//! Tests:
//! - Entry point is reached
//! - Subprocess spawning (self-spawn with --dummy)
//! - Subprocess spawning (musl binary if found)
//! - waitpid() works

#![no_std]
#![no_main]

use libakuma::{exit, print, arg, argc, spawn, waitpid, open, close, open_flags};

#[no_mangle]
pub extern "C" fn main() {
    let argc = argc();
    let first_arg = arg(1);

    if let Some(arg) = first_arg {
        if arg == "--dummy" {
            print("elftest: dummy mode reached NYA!\n");
            exit(42);
        }
    }

    print("elftest: starting subprocess spawn tests\n");

    // 1. Spawn itself
    print("elftest: spawning /bin/elftest --dummy\n");
    let self_args = ["--dummy"];
    if let Some(res) = spawn("/bin/elftest", Some(&self_args)) {
        // Wait for it
        let mut success = false;
        for _ in 0..100 {
            if let Some((pid, code)) = waitpid(res.pid) {
                if pid == res.pid && code == 42 {
                    print("elftest: self-spawn test PASSED\n");
                    success = true;
                } else {
                    print("elftest: self-spawn test FAILED (wrong exit code)\n");
                }
                break;
            }
            libakuma::sleep_ms(10);
        }
        if !success {
            print("elftest: self-spawn test FAILED (timeout or spawn error)\n");
            exit(1);
        }
    } else {
        print("elftest: FAILED to spawn /bin/elftest\n");
        exit(1);
    }

    // 2. Spawn hello_musl.bin if found
    let hello_path = "/bin/hello_musl.bin";
    let hello_fd = open(hello_path, open_flags::O_RDONLY);
    if hello_fd >= 0 {
        close(hello_fd);
        print("elftest: spawning /bin/hello_musl.bin\n");
        if let Some(res) = spawn(hello_path, None) {
             // Wait for it
             let mut success = false;
             for _ in 0..100 {
                 if let Some((pid, _code)) = waitpid(res.pid) {
                     if pid == res.pid {
                         print("elftest: hello_musl test PASSED\n");
                         success = true;
                     }
                     break;
                 }
                 libakuma::sleep_ms(10);
             }
             if !success {
                 print("elftest: hello_musl test FAILED (timeout)\n");
                 exit(1);
             }
        } else {
            print("elftest: FAILED to spawn /bin/hello_musl.bin\n");
            exit(1);
        }
    } else {
        print("elftest: /bin/hello_musl.bin not found, skipping second half of test\n");
    }

    print("elftest: all tests PASSED\n");
    // Exit with 42 so kernel test test_elftest() passes
    exit(42);
}
