// jobserver_stress — a focused stress probe for the exact primitive behind
// Failure D (docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §7).
//
// §7.5 identified `0x3cda5fc4` as jobserver-rs 0.1.35's `HelperState::for_each_request`
// untimed `self.cvar.wait(lock)` (lib.rs:583-606), woken only by `request_token()`
// (lock; requests.push; notify_one) or `drop()` (shutdown). That is a
// multi-producer / single-consumer Mutex+Condvar with an UNTIMED wait (FUTEX_WAIT|PRIVATE,
// a1=0x80 — matches the stuck threads exactly). The existing futextest.rs phase 4 is a
// 1:1 producer/consumer and does not match that shape; this probe does.
//
// §7.6 showed the wake is never *issued* for that address — not lost mid-handoff. That
// means either (a) a raw-primitive lost wake under SMP contention that this probe can
// catch cheaply, or (b) something specific to rustc's call pattern (the `f(self)`-outside-
// the-lock timing in for_each_request, or genuine resource pressure). If this probe
// reproduces a hang under real 4-core contention, it is a clean kernel repro. If it does
// NOT reproduce even under heavy stress, pivot to instrumenting rustc's own jobserver
// call sites (§7.8).
//
// Build in-guest (rustc is available — this is a self-host image):
//   rustc -O jobserver_stress.rs -o /tmp/jobserver_stress
// Or cross-compile on host:
//   rustc --target aarch64-unknown-linux-musl -C linker=aarch64-linux-musl-gcc -O \
//       userspace/selfhost_repro/jobserver_stress.rs -o userspace/forktest/c_stress/jobserver_stress
//
// Run (defaults: 4 producers, 1 consumer, 200k requests total, 30s park phase):
//   /tmp/jobserver_stress
// Env knobs:
//   JS_PRODUCERS=4  JS_REQUESTS=200000  JS_PARK_ITERS=200000  JS_BARRIER_THREADS=4
//   JS_PHASE=condvar|park|barrier|all   (run one phase)
//
// Each phase prints "[name] start" then "[name] ok N". A missing "ok" within the
// deadline (set JS_TIMEOUT_SECS — a process self-kill via alarm()) is the repro.

use std::env;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn mark(s: &str) {
    let mut o = std::io::stdout();
    let _ = writeln!(o, "{}", s);
    let _ = o.flush();
}

fn env_or(name: &str, def: u64) -> u64 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(def)
}

/// Arm an alarm() self-kill so a lost wake terminates the process with a recognizable
/// exit code instead of hanging the harness forever. Default 60s; override JS_TIMEOUT_SECS.
fn arm_alarm() {
    let secs = env_or("JS_TIMEOUT_SECS", 60) as u32;
    unsafe {
        // SIGALRM default action is process termination; exit status will reflect a
        // signal kill (142 on most shells). The harness treats "no ok line + signal
        // death" as a hung phase.
        libc_alarm(secs);
    }
}

extern "C" {
    fn alarm(secs: u32) -> u32;
}
unsafe fn libc_alarm(secs: u32) { let _ = alarm(secs); }

// ---------------------------------------------------------------------------
// Phase A: multi-producer / single-consumer Mutex+Condvar with UNTIMED wait.
// This is jobserver's HelperState::for_each_request shape exactly.
//
//   consumer (Helper thread):  loop { while q.is_empty() { g = cvar.wait(g) } ; drain(q) }
//   producer (request_token) :  { let mut g = m.lock(); q.push(req); cvar.notify_one(); }
// ---------------------------------------------------------------------------
fn phase_condvar_mpsc(producers: u64, total_requests: u64) {
    mark(&format!("[condvar-mpsc] start producers={} requests={}", producers, total_requests));

    // Shared state mirrors HelperState { requests: Mutex<VecDeque>, cvar: Condvar }.
    let q = Arc::new((Mutex::new(std::collections::VecDeque::<u64>::new()), Condvar::new()));
    let drained = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // --- single consumer: untimed cvar.wait() while empty, exactly like for_each_request ---
    let q2 = q.clone();
    let drained2 = drained.clone();
    let stop2 = stop.clone();
    let consumer = thread::spawn(move || {
        let (m, cv) = &*q2;
        let mut g = m.lock().unwrap();
        loop {
            // for_each_request's inner loop: wait while empty (UNTIMED).
            while g.is_empty() {
                if stop2.load(Ordering::Acquire) && g.is_empty() {
                    return;
                }
                g = cv.wait(g).unwrap(); // <-- the 0x3cda5fc4 primitive, untimed
            }
            // drain all pending (for_each_request pops one-at-a-time; batching is a
            // valid stronger stress and still exercises the same wake).
            while let Some(v) = g.pop_front() {
                // mimic f(req) work outside the lock would require re-lock; keep it
                // cheap and in-lock to maximize wake frequency against park boundary.
                drained2.fetch_add(1, Ordering::Relaxed);
                let _ = v;
            }
        }
    });

    // --- N producers: each pushes total_requests/producers items, notify_one each ---
    let per = total_requests / producers.max(1);
    let mut prods = Vec::new();
    for _ in 0..producers {
        let q3 = q.clone();
        prods.push(thread::spawn(move || {
            let (m, cv) = &*q3;
            for i in 0..per {
                let mut g = m.lock().unwrap();
                g.push_back(i);
                cv.notify_one(); // request_token's wake
            }
        }));
    }
    for p in prods { p.join().unwrap(); }

    // signal shutdown like HelperThread::drop: set stop, then notify_one to release wait
    stop.store(true, Ordering::Release);
    q.1.notify_one();
    consumer.join().unwrap();

    let got = drained.load(Ordering::Relaxed);
    if got == total_requests {
        mark(&format!("[condvar-mpsc] ok drained={}", got));
    } else {
        mark(&format!("[condvar-mpsc] HANG/LOSS drained={} expected={}", got, total_requests));
        std::process::exit(3);
    }
}

// ---------------------------------------------------------------------------
// Phase B: park/unpark MPSC (candidate for the *other* stuck address, §7.5).
// std::thread::park uses a raw futex (untimed). Single consumer parks; N producers
// unpark. The sticky-unpark invariant (debug-futex-lost-wakeup.md §4a) is what this
// hammers.
// ---------------------------------------------------------------------------
fn phase_park_mpsc(producers: u64, total_iters: u64) {
    mark(&format!("[park-mpsc] start producers={} iters={}", producers, total_iters));

    let per = total_iters / producers.max(1);
    let mut prods = Vec::new();
    let done = Arc::new(AtomicBool::new(false));
    for _ in 0..producers {
        let main = thread::current();
        let d = done.clone();
        prods.push(thread::spawn(move || {
            for _ in 0..per {
                // ensure the consumer re-parks between unparks by racing hard
                main.unpark();
            }
            d.store(true, Ordering::Release);
            main.unpark(); // final wake so the consumer sees done
        }));
    }

    // consumer: park in a loop until all producers done. park() is untimed futex.
    let deadline = Instant::now() + Duration::from_secs(env_or("JS_TIMEOUT_SECS", 60));
    loop {
        if done.load(Ordering::Acquire) { break; }
        if Instant::now() > deadline {
            mark("[park-mpsc] HANG/LOSS timed out waiting for producers");
            std::process::exit(3);
        }
        thread::park_timeout(Duration::from_millis(100));
    }
    for p in prods { p.join().unwrap(); }
    mark("[park-mpsc] ok");
}

// ---------------------------------------------------------------------------
// Phase C: high-fanout barrier — many threads FUTEX_WAIT, last-in FUTEX_WAKEs all.
// One-to-many wake path, repeated under contention.
//
// This is a hand-rolled re-implementation of std::sync::Barrier's exact algorithm
// (Mutex<{count, generation}> + Condvar, wait_while) rather than std::sync::Barrier
// itself, so it can be instrumented from the inside. §7.10 of the archive doc found
// that the periodic-revalidation mitigation never sees the futex *value* change on a
// hang, meaning the thread that should call notify_all() never gets there — but could
// not tell whether that's because the arrival count itself is wrong (lost increment
// under contention) or because the leader thread reaches "about to notify" and then
// the notify_all() syscall itself never returns. This tracks, per worker-thread slot,
// which of 7 steps it is in and how long it has been there, plus a global rounds-total
// heartbeat a watchdog thread polls; on a stall it dumps exactly what every thread was
// doing and for how long, without spamming a print every round (32000+ rounds would
// perturb the exact scheduling window this bug needs).
// ---------------------------------------------------------------------------
extern "C" {
    fn gettid() -> i32;
}

struct BarrierState {
    count: u64,
    generation: u64,
}

// Per-thread step, see the numeric comments at each `set_step` call site below.
struct InstrBarrier {
    state: Mutex<BarrierState>,
    cvar: Condvar,
    n: u64,
    start: Instant,
    rounds_total: AtomicU64,
    shadow_count: AtomicU64,
    shadow_gen: AtomicU64,
    last_notify_tid: AtomicU64,
    last_notify_round: AtomicU64,
    step: Vec<AtomicU8>,
    step_ts_ms: Vec<AtomicU64>,
    thread_tid: Vec<AtomicU64>,
}

impl InstrBarrier {
    fn new(n: u64) -> Self {
        InstrBarrier {
            state: Mutex::new(BarrierState { count: 0, generation: 0 }),
            cvar: Condvar::new(),
            n,
            start: Instant::now(),
            rounds_total: AtomicU64::new(0),
            shadow_count: AtomicU64::new(0),
            shadow_gen: AtomicU64::new(0),
            last_notify_tid: AtomicU64::new(0),
            last_notify_round: AtomicU64::new(0),
            step: (0..n).map(|_| AtomicU8::new(0)).collect(),
            step_ts_ms: (0..n).map(|_| AtomicU64::new(0)).collect(),
            thread_tid: (0..n).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn now_ms(&self) -> u64 { self.start.elapsed().as_millis() as u64 }

    fn set_step(&self, idx: usize, s: u8) {
        self.step[idx].store(s, Ordering::Relaxed);
        self.step_ts_ms[idx].store(self.now_ms(), Ordering::Relaxed);
    }

    fn register(&self, idx: usize) {
        self.thread_tid[idx].store(unsafe { gettid() } as u64, Ordering::Relaxed);
    }

    // Steps: 0=idle 1=locking-mutex 2=locked-evaluating 3=LEADER-about-to-notify_all
    //        4=LEADER-notify_all-returned 5=FOLLOWER-about-to-wait_while
    //        6=FOLLOWER-woken-returning
    fn wait(&self, idx: usize, round: u64) {
        self.set_step(idx, 1);
        let mut g = self.state.lock().unwrap();
        self.set_step(idx, 2);
        g.count += 1;
        self.shadow_count.store(g.count, Ordering::Relaxed);
        if g.count < self.n {
            let local_gen = g.generation;
            self.set_step(idx, 5);
            let _g2 = self.cvar.wait_while(g, |st| st.generation == local_gen).unwrap();
            self.set_step(idx, 6);
        } else {
            g.count = 0;
            g.generation = g.generation.wrapping_add(1);
            self.shadow_count.store(0, Ordering::Relaxed);
            self.shadow_gen.store(g.generation, Ordering::Relaxed);
            self.set_step(idx, 3);
            self.last_notify_tid.store(unsafe { gettid() } as u64, Ordering::Relaxed);
            self.last_notify_round.store(round, Ordering::Relaxed);
            self.cvar.notify_all();
            self.set_step(idx, 4);
            drop(g);
        }
        self.rounds_total.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" {
    fn getpid() -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn open(path: *const u8, flags: i32, mode: u32) -> i32;
}

// §7.12 found the original stall-gated watchdog an unreliable narrator: its
// `[barrier-instr] STUCK` dump never fired even across an 800+ second hang.
// §7.17 found why: `thread::sleep` (the only non-futex primitive the watchdog's
// poll loop used) panicked on its very first call, every run, because
// `clock_nanosleep` was an unimplemented syscall — the watchdog thread died
// silently before ever reaching its own dump logic. Now fixed upstream (kernel),
// but this watchdog is kept on its own dedicated fd + raw `libc::write()`
// (bypassing `io::Stdout`) anyway: it's a cheap way to sidestep `sys_write`'s
// shared-fd position race too (§7.15/CONCURRENT_WRITE_POSITION_RACE.md),
// whether or not a given build has that fix.
fn spawn_barrier_watchdog(bar: Arc<InstrBarrier>, done: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let pid = unsafe { getpid() };
        let path = format!("/root/wd_{}.log\0", pid);
        let fd = unsafe { open(path.as_ptr(), 0o1101 /* O_WRONLY|O_CREAT|O_TRUNC */, 0o666) };
        let poll_ms: u64 = 1000;
        loop {
            thread::sleep(Duration::from_millis(poll_ms));
            if done.load(Ordering::Acquire) { return; }
            let line = format!(
                "t={} rounds_total={} shadow_count={} shadow_gen={} last_notify_tid={} last_notify_round={}\n",
                bar.now_ms(),
                bar.rounds_total.load(Ordering::Relaxed),
                bar.shadow_count.load(Ordering::Relaxed),
                bar.shadow_gen.load(Ordering::Relaxed),
                bar.last_notify_tid.load(Ordering::Relaxed),
                bar.last_notify_round.load(Ordering::Relaxed),
            );
            if fd >= 0 {
                unsafe { write(fd, line.as_ptr(), line.len()) };
            }
        }
    })
}

fn phase_barrier(threads: u64, rounds: u64) {
    mark(&format!("[barrier] start threads={} rounds={}", threads, rounds));
    let bar = Arc::new(InstrBarrier::new(threads));
    let done = Arc::new(AtomicBool::new(false));
    // JS_NO_WATCHDOG: A/B control for the 2026-08-07 clock_nanosleep fix — isolates
    // whether the fix itself (or the watchdog's own periodic real-timer wake
    // activity, which could perturb SMP scheduling enough to dodge the hang
    // incidentally) is what makes the hang stop reproducing.
    let watchdog = if env_or("JS_NO_WATCHDOG", 0) == 0 {
        Some(spawn_barrier_watchdog(bar.clone(), done.clone()))
    } else {
        None
    };
    let mut hs = Vec::new();
    for idx in 0..threads as usize {
        let b = bar.clone();
        hs.push(thread::spawn(move || {
            b.register(idx);
            for round in 0..rounds { b.wait(idx, round); }
        }));
    }
    for h in hs { h.join().unwrap(); }
    done.store(true, Ordering::Release);
    if let Some(w) = watchdog { let _ = w.join(); }
    mark("[barrier] ok");
}

// ---------------------------------------------------------------------------
// Phase D: tight spawn/join churn — clone + clear_child_tid futex wake, the path
// that the already-fixed 2026-08-05 bug hid in. Re-stress it under SMP.
// ---------------------------------------------------------------------------
fn phase_spawn_join_loop(iters: u64) {
    mark(&format!("[spawn-join] start iters={}", iters));
    let c = Arc::new(AtomicU64::new(0));
    for i in 0..iters {
        let c2 = c.clone();
        let h = thread::spawn(move || { c2.fetch_add(1, Ordering::Relaxed); i });
        assert_eq!(h.join().unwrap(), i);
    }
    assert_eq!(c.load(Ordering::Relaxed), iters);
    mark("[spawn-join] ok");
}

fn main() {
    mark("=== JOBSERVER_STRESS start ===");
    arm_alarm();

    let only = env::var("JS_PHASE").ok();
    let run = |name: &str| only.as_ref().map_or(true, |p| p == name || p == "all");

    let producers = env_or("JS_PRODUCERS", 4);
    let requests = env_or("JS_REQUESTS", 200_000);
    let park_iters = env_or("JS_PARK_ITERS", 200_000);
    let barrier_threads = env_or("JS_BARRIER_THREADS", 4);
    let barrier_rounds = env_or("JS_BARRIER_ROUNDS", 5_000);
    let spawn_iters = env_or("JS_SPAWN_ITERS", 2_000);

    if run("spawn") { phase_spawn_join_loop(spawn_iters); }
    if run("condvar") { phase_condvar_mpsc(producers, requests); }
    if run("park") { phase_park_mpsc(producers, park_iters); }
    if run("barrier") { phase_barrier(barrier_threads, barrier_rounds); }

    mark("=== JOBSERVER_STRESS DONE — all phases passed ===");
}
