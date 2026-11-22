//! exec from a non-leader thread (with busy siblings) into an image that itself spawns and joins threads
//! (the JVM launcher's shape: the primordial thread blocks in pthread_join on the thread that ran main).
//!   execpost pre   -> exec self as `post` from a worker thread
//!   execpost post  -> spawn 3 threads, join each, print POST_JOIN_OK, exit 7
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("post") => {
            println!("POST-START");
            for i in 0..3 {
                let t = thread::spawn(move || { thread::sleep(Duration::from_millis(50)); i });
                let r = t.join().unwrap();
                println!("POST-JOINED {r}");
            }
            println!("POST_JOIN_OK");
            let variant = args.get(2).map(String::as_str).unwrap_or("libc");
            match variant {
                // raw exit_group(7): no libc exit path
                "raw_group" => unsafe { core::arch::asm!("syscall", in("rax") 231, in("rdi") 7, options(noreturn)) },
                // raw exit(7) of this thread only (the last thread of the group)
                "raw_exit" => unsafe { core::arch::asm!("syscall", in("rax") 60, in("rdi") 7, options(noreturn)) },
                _ => std::process::exit(7),
            }
        }
        _ => {
            let stop = Arc::new(AtomicBool::new(false));
            let mut ws = Vec::new();
            for _ in 0..4 {
                let stop = stop.clone();
                ws.push(thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let v = vec![0u8; 256 * 1024];
                        std::hint::black_box(&v);
                        thread::sleep(Duration::from_millis(2));
                    }
                }));
            }
            thread::sleep(Duration::from_millis(100));
            let me = std::env::current_exe().unwrap();
            let t = thread::spawn(move || {
                let variant = std::env::args().nth(2).unwrap_or_else(|| "libc".into());
                let e = Command::new(me).arg("post").arg(variant).exec();
                println!("EXEC-FAILED {e}");
            });
            let _ = t.join();
            println!("PRE-MAIN-ALIVE");
        }
    }
}
