// Userspace futex / thread stress test for Akuma.
//
// rustc hangs on Akuma with multiple threads parked in WAITING at a single user
// PC (docs §7g) — a suspected futex/thread-park missed wakeup. This binary
// exercises the same primitives rustc/std use (clone + futex via pthread) in
// progressively harder patterns, each with its own progress marker, so a hang
// pinpoints WHICH pattern breaks. Build in-VM for the host target:
//
//   rustc -O futextest.rs -o /tmp/futextest && /tmp/futextest
//
// Each phase prints "[N] start ..." then "[N] ok" — a missing "ok" is the
// culprit. Set FUTEXTEST_PHASE=N to run a single phase.

use std::env;
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Condvar, Barrier};
use std::time::Duration;

fn mark(s: &str) {
    let mut o = std::io::stdout();
    let _ = writeln!(o, "{}", s);
    let _ = o.flush();
}

// (1) Spawn one thread and join it. The simplest pthread_create + pthread_join,
// which on musl is clone + a futex wait on the child's clear_child_tid.
fn phase_spawn_join() {
    mark("[1] spawn+join single thread: start");
    let h = std::thread::spawn(|| 42u64);
    let v = h.join().unwrap();
    assert_eq!(v, 42);
    mark("[1] ok");
}

// (2) Tight spawn/join loop — stresses clone + exit + clear_child_tid futex wake
// (the path that wakes a joiner). A lost wake here hangs join().
fn phase_spawn_join_loop() {
    mark("[2] 200x spawn/join loop: start");
    for i in 0..200u64 {
        let h = std::thread::spawn(move || i * 2);
        assert_eq!(h.join().unwrap(), i * 2);
        if i % 50 == 0 { mark(&format!("[2]   iter {}", i)); }
    }
    mark("[2] ok");
}

// (3) Fan-out: spawn N threads at once, join them all. Stresses N concurrent
// clear_child_tid futex wakes landing on the main thread's joins.
fn phase_fanout(n: usize) {
    mark(&format!("[3] fan-out {} threads + join all: start", n));
    let counter = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for _ in 0..n {
        let c = counter.clone();
        hs.push(std::thread::spawn(move || {
            for _ in 0..1000 { c.fetch_add(1, Ordering::Relaxed); }
        }));
    }
    for h in hs { h.join().unwrap(); }
    assert_eq!(counter.load(Ordering::Relaxed), (n as u64) * 1000);
    mark("[3] ok");
}

// (4) Mutex + Condvar producer/consumer — the core futex WAIT/WAKE path. The
// consumer parks on the condvar (FUTEX_WAIT); the producer signals (FUTEX_WAKE).
// A lost wake hangs the consumer.
fn phase_condvar(rounds: u64) {
    mark(&format!("[4] mutex+condvar {} rounds: start", rounds));
    let pair = Arc::new((Mutex::new(0u64), Condvar::new()));
    let p2 = pair.clone();
    let prod = std::thread::spawn(move || {
        let (m, cv) = &*p2;
        for i in 1..=rounds {
            let mut g = m.lock().unwrap();
            *g = i;
            cv.notify_one();
        }
    });
    {
        let (m, cv) = &*pair;
        let mut g = m.lock().unwrap();
        while *g < rounds {
            g = cv.wait(g).unwrap();
        }
    }
    prod.join().unwrap();
    mark("[4] ok");
}

// (5) Barrier across N threads, repeated — every thread FUTEX_WAITs until the
// last arrives and FUTEX_WAKEs them all (a one-to-many wake).
fn phase_barrier(n: usize, rounds: usize) {
    mark(&format!("[5] barrier {} threads x {} rounds: start", n, rounds));
    let bar = Arc::new(Barrier::new(n));
    let mut hs = Vec::new();
    for _ in 0..n {
        let b = bar.clone();
        hs.push(std::thread::spawn(move || {
            for _ in 0..rounds { b.wait(); }
        }));
    }
    for h in hs { h.join().unwrap(); }
    mark("[5] ok");
}

// (6) Wake-before-wait race: the waker may fire before the waiter parks. The
// kernel's sticky-wake flag must make schedule_blocking return immediately.
fn phase_wake_before_wait(iters: usize) {
    mark(&format!("[6] wake-before-wait race x {}: start", iters));
    for _ in 0..iters {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let p2 = pair.clone();
        let waker = std::thread::spawn(move || {
            // Fire immediately — likely before the main thread reaches wait().
            let (m, cv) = &*p2;
            let mut g = m.lock().unwrap();
            *g = true;
            cv.notify_one();
        });
        let (m, cv) = &*pair;
        let mut g = m.lock().unwrap();
        while !*g {
            g = cv.wait(g).unwrap();
        }
        drop(g);
        waker.join().unwrap();
    }
    mark("[6] ok");
}

// (7) park/unpark — std::thread::park uses a futex directly (not via pthread
// mutex), including the unpark-before-park sticky case.
fn phase_park_unpark(iters: usize) {
    mark(&format!("[7] park/unpark x {}: start", iters));
    let flag = Arc::new(AtomicBool::new(false));
    for _ in 0..iters {
        let f = flag.clone();
        let main = std::thread::current();
        f.store(false, Ordering::SeqCst);
        let h = std::thread::spawn(move || {
            f.store(true, Ordering::SeqCst);
            main.unpark();
        });
        while !flag.load(Ordering::SeqCst) {
            std::thread::park_timeout(Duration::from_millis(50));
        }
        h.join().unwrap();
    }
    mark("[7] ok");
}

fn main() {
    mark("=== FUTEXTEST start ===");
    let only: Option<usize> = env::var("FUTEXTEST_PHASE").ok().and_then(|s| s.parse().ok());
    let run = |n: usize| only.map_or(true, |p| p == n);

    if run(1) { phase_spawn_join(); }
    if run(2) { phase_spawn_join_loop(); }
    if run(3) { phase_fanout(8); }
    if run(4) { phase_condvar(2000); }
    if run(5) { phase_barrier(6, 100); }
    if run(6) { phase_wake_before_wait(500); }
    if run(7) { phase_park_unpark(500); }

    mark("=== FUTEXTEST DONE — all phases passed ===");
}
