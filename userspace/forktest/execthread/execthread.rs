//! `execve` from a non-leader thread while other threads are alive (E7).
//!
//!   execthread ok   <program> [args..]   exec <program> from a worker thread; expect its output
//!   execthread fail                      exec a missing path from a worker; expect ENOENT, process lives on
//!
//! Build (static musl, host toolchain):
//!   rustc --edition 2021 -O --target x86_64-unknown-linux-musl -C target-feature=+crt-static execthread.rs
//!
//! Before the detach fix the first mode panicked the kernel (ring-0 #PF in `Process::with_address_space`
//! inside `sys_mmap`) once a sibling thread called `mmap`; see docs/archive/EXEC_FROM_THREAD_DETACH.md.
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map_or("ok", String::as_str).to_string();
    let stop = Arc::new(AtomicBool::new(false));
    // Siblings that keep entering the kernel and allocating, the way a JVM's GC and compiler threads do.
    let mut workers = Vec::new();
    for _ in 0..4 {
        let stop = stop.clone();
        workers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let v = vec![0u8; 256 * 1024];
                std::hint::black_box(&v);
                thread::sleep(Duration::from_millis(2));
            }
        }));
    }
    thread::sleep(Duration::from_millis(100));
    println!("EXECTHREAD-BEFORE mode={mode}");
    let target: Vec<String> = if mode == "fail" { vec!["/nonexistent/execthread-target".into()] } else { args[2..].to_vec() };
    let t = thread::spawn(move || {
        let mut c = Command::new(&target[0]);
        c.args(&target[1..]);
        let e = c.exec(); // returns only on failure
        println!("EXECTHREAD-EXEC-FAILED {e}");
    });
    let _ = t.join();
    stop.store(true, Ordering::Relaxed);
    for w in workers { let _ = w.join(); }
    println!("EXECTHREAD-MAIN-ALIVE");
}
