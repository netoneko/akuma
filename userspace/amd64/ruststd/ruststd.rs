//! The Rust `std` probe: how far does a real `std` program get on this kernel?
//!
//! Unlike `hello` and `fdprobe` next door, this is **not** a `#![no_std]`
//! program against `x86_64-unknown-none`. It is an ordinary Rust binary for
//! `x86_64-unknown-linux-musl`, linked static-PIE against a real musl, and that
//! is the entire point: the interesting code runs *before* `main`, in musl's
//! `__libc_start_main` and `std::rt::init`, and no hand-rolled probe reproduces
//! it. See `docs/archive/AKUMA_AMD64_RUST_STD.md`.
//!
//! # It reports by printing, not only by exiting
//!
//! Every other program in this directory reports through its exit status,
//! because the kernel's self-test compares that status against a constant. This
//! one is a bring-up instrument: the question it answers is *which stage stops
//! it*, and a program that dies at stage 3 has no exit status to report. So each
//! stage announces itself on stdout **before** doing the work, and the exit
//! status is the number of stages completed. A truncated log names the wall;
//! the status confirms it when there is one.
//!
//! ```text
//!   [rs] N <what>     printed before stage N runs
//!   exit(STAGES)      every stage completed
//! ```
//!
//! Stages are ordered by how early Rust needs them, so the first missing thing
//! is the first thing that stops the program:
//!
//! | stage | exercises | the syscalls it needs |
//! |---|---|---|
//! | (pre) | musl `__init_tp`, `std::rt::init` | `set_tid_address`, `sigaltstack`, `rt_sigaction`, `poll`/`fcntl` on 0-2, `mmap` |
//! | 1 | `println!` | `write`, and `Stdout`'s `OnceLock`/`ReentrantLock` |
//! | 2 | heap | `brk` or `mmap` under musl's mallocng |
//! | 3 | `env::args` / `env::vars` | none — reads the initial stack, so it checks the *loader* |
//! | 4 | `Instant::now` | `clock_gettime(CLOCK_MONOTONIC)` |
//! | 5 | `fs::read_to_string` | `openat`, `statx`/`fstat`, `read`, `close` |
//! | 6 | `thread::spawn` + `join` | `clone(CLONE_VM|CLONE_THREAD|…)`, `mmap` for the stack, `futex` |
//!
//! Stage 6 is the one blocker §11.2 predicts, and it is last so that everything
//! cheaper than it is already measured by the time it fails.

use std::io::Write;

/// How many stages a complete run finishes. The exit status on success.
///
/// Deliberately small and not 0: a `0` from this program is indistinguishable
/// from a kernel that ran nothing and reported success, which is a failure mode
/// this target has actually had.
const STAGES: i32 = 6;

/// Announce a stage before running it.
///
/// `stdout` is line-buffered onto a pipe and block-buffered elsewhere, and a
/// program that dies mid-stage must not take its own announcement with it — so
/// this flushes. Without the flush the log names the stage *before* the one that
/// actually died, which is worse than no log.
fn stage(n: u32, what: &str) {
    println!("[rs] {n} {what}");
    let _ = std::io::stdout().flush();
}

fn main() {
    // Stage 1 is `println!` itself, so its announcement *is* the stage. If
    // nothing at all appears below, the wall is before `main`: musl's thread
    // pointer setup or `std::rt::init`, and the syscall trace is the only
    // witness.
    stage(1, "println");

    stage(2, "heap");
    let v: Vec<u64> = (1..=64).collect();
    let sum: u64 = v.iter().sum();
    assert_eq!(sum, 2080, "heap arithmetic is wrong, which is not a syscall bug");
    println!("[rs]   vec len={} sum={sum}", v.len());

    stage(3, "args+env");
    let args: Vec<String> = std::env::args().collect();
    println!("[rs]   argc={} argv0={:?}", args.len(), args.first());
    println!("[rs]   PATH={:?}", std::env::var("PATH").ok());

    stage(4, "clock");
    let t0 = std::time::Instant::now();
    let mut spin = 0u64;
    for i in 0..100_000u64 {
        spin = spin.wrapping_add(i);
    }
    println!("[rs]   elapsed={:?} (spin={spin})", t0.elapsed());

    stage(5, "fs");
    // `/bin/hello` is the one file this image is guaranteed to have: `mkdisk.sh`
    // stages it unconditionally and exits if it cannot find it. Reading it as
    // bytes rather than as text because it is an ELF.
    match std::fs::read("/bin/hello") {
        Ok(b) => println!("[rs]   /bin/hello {} bytes, magic={:02x?}", b.len(), &b[..4.min(b.len())]),
        Err(e) => println!("[rs]   /bin/hello FAILED: {e}"),
    }

    stage(6, "thread");
    let h = std::thread::spawn(|| {
        // Touching the heap from the child proves the address space is shared
        // rather than forked: a `fork`ed child would get its own copy and the
        // parent would see nothing. The return value carries that back.
        let child: Vec<u64> = (1..=8).collect();
        child.iter().sum::<u64>()
    });
    match h.join() {
        Ok(n) => println!("[rs]   thread returned {n}"),
        Err(_) => println!("[rs]   thread PANICKED"),
    }

    println!("[rs] all {STAGES} stages complete");
    let _ = std::io::stdout().flush();
    std::process::exit(STAGES);
}
