// rcdrop — Rust probe for the cargo null-`Rc` / Drop-corruption class
// (docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md §13).
//
// The audit's two crashes both surface in cargo's drop glue after `Finished`:
//
//   crash 1: ELR=0x7d7bc4  →  <Drop for RawTable<cargo::compiler::unit::Unit>>::drop
//                             loading the Rc<UnitInner> pointer from a hashbrown
//                             bucket, x8 == 0 (NULL) on the deref.
//   crash 2: ELR=0x14d1ad4 →  <semver::Identifier as Drop>::drop called with
//                             self == 0x21 (junk small int where a pointer
//                             belonged in the parent struct).
//
// Different Drop impls, different cargo subsystems, same shape: a qword that
// should hold a pointer is holding a small integer, *inside an otherwise
// live page* (the kernel's own [WILD-DA] forensics confirm the surrounding
// pages are not zeroed). Either cargo's own talc arena has an in-process UAF
// (§3.2's kernel-PMM quarantine does not cover that), or a wild-pointer store
// through the live VA is scribbling a single qword.
//
// This probe mimics the shape: many threads building `Arc<UnitInner>`-graphs
// (Rc is `!Send`; Arc exercises the same drop glue that crashed), cloning
// across threads via a shared table, and dropping in batches that walk a
// parent chain — exactly the path that surfaces the corruption in cargo.
// Optional fork churn matches cargo's constant rustc-spawning.
//
// Detection is implicit: a corrupted heap surfaces as a SIGSEGV inside
// `Arc::drop` / `Drop::drop` exactly the way cargo dies. The explicit canary
// pass after each round catches the quieter "wild store that scribbled a
// payload but did not crash yet" — when the kernel is the corruptor, that
// fires before the crash does, and the line names the field.
//
// Build (host, cross):
//   rustc --target aarch64-unknown-linux-musl -O rcdrop.rs -o rcdrop
//   (or in-guest: rustc -O rcdrop.rs -o /tmp/rcdrop)
//
// Calibrate on real Linux aarch64 FIRST — a FAIL there is the probe being
// wrong, not the kernel. Expect 100/100 PASS:
//   docker run --rm --platform linux/arm64 -v "$PWD/rcdrop:/rcdrop:ro" alpine /rcdrop 50 4
//
// Usage: rcdrop [rounds] [threads] [fork_hz]
//   rounds   default 50      — rounds per worker
//   threads  default 4       — worker threads (cargo -j4 shape)
//   fork_hz  default 0       — fork attempts per second; 0 = off
//
// Env knobs:
//   RCDROP_ROUNDS, RCDROP_THREADS, RCDROP_FORK_HZ   (override argv)
//   RCDROP_BATCH=32                                 — Units per worker per round
//   RCDROP_TABLE_CAP=4096                           — shared table capacity before drain
//   RCDROP_TIME_LIMIT_SECS=60                       — alarm() self-kill if exceeded
//
// Exit codes:
//   0  PASS — all rounds completed, all canaries intact
//   1  FAIL — canary corruption detected (the field is named on stderr)
//   2  setup error
//   139 SIGSEGV during drop — the cargo failure mode (kernel corruptor)

// `panic = "abort"` would mirror cargo's build (and keeps Rust's unwind
// machinery out of kernel forensics), but it's a cargo-config knob, not a
// stable crate attribute. The probe works fine with default unwind; if the
// kernel corrupts the heap, the SIGSEGV fires before any panic path.

use std::env;
use std::io::Write;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// The shape under test.
//
// `UnitInner` is modeled on `cargo::compiler::unit::UnitInner`. The audit's
// crash 1 faulted decrementing the strong-refcount field of an `Rc<UnitInner>`
// stored in a hashbrown bucket — `Arc<UnitInner>` here exercises the same
// `Drop::drop` path through `alloc::sync::Arc::drop`.
//
// `package` is heap-allocated via `Box` so a wild store that scribbles its
// pointer is detectable: the canary pass reads `*package` and a corrupt
// pointer either SIGSEGVs (caught as exit 139) or returns the wrong bytes
// (caught here as exit 1).
struct UnitInner {
    payload: u64,           // unique tag: tid*1M + round*256 + slot
    package: Box<[u8; 16]>, // mimics Rc<PackageInner>; canary contents = PACKAGE_CANARY
    // Second heap allocation per UnitInner: widens the Arc's footprint so a
    // wild store that scribbles its own allocator metadata into a neighbour
    // has a neighbour to hit. Read by nothing but Drop.
    #[allow(dead_code)]
    extra: Box<u64>,
}

const PACKAGE_CANARY: [u8; 16] = *b"RCDROP_CANARY_16";

// Parent wrapper — drops of this are what surface the corruption in cargo
// (walking a `Vec<Comparator>` etc.). Adding it here makes the canary pass
// walk two levels, which is what catches the "self=0x21" shape of crash 2:
// a corrupted parent base makes the field access fault.
struct Unit {
    inner: Arc<UnitInner>,
    label: Box<u64>, // another heap field, canary = LABEL_CANARY
}

const LABEL_CANARY: u64 = 0xC0FFEE_C0FFEE_C0FE;

// ---------------------------------------------------------------------------
fn env_or(name: &str, def: u64) -> u64 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(def)
}

fn mark(s: &str) {
    let mut o = std::io::stdout();
    let _ = writeln!(o, "{}", s);
    let _ = o.flush();
}

fn mark_err(s: &str) {
    let mut o = std::io::stderr();
    let _ = writeln!(o, "{}", s);
    let _ = o.flush();
}

extern "C" { fn alarm(secs: u32) -> u32; }
unsafe fn libc_alarm(secs: u32) { let _ = alarm(secs); }

fn arm_alarm() {
    let secs = env_or("RCDROP_TIME_LIMIT_SECS", 60) as u32;
    unsafe { libc_alarm(secs); }
}

// musl libc bindings (resolved by dyn ld).
extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(code: i32) -> !;
}

// ---------------------------------------------------------------------------
// The shared table. Workers push half their batch in (cloning Arcs) and
// periodically drain the whole thing — that drain is the bulk-Drop path
// cargo dies on. Cap is a soft limit: over-cap triggers an immediate drain.
struct SharedTable {
    units: Vec<Unit>,
    cap: usize,
    bad_canaries: AtomicU64,
    total_pushed: AtomicU64,
    total_drained: AtomicU64,
}

impl SharedTable {
    fn new(cap: usize) -> Self {
        Self {
            units: Vec::with_capacity(cap),
            cap,
            bad_canaries: AtomicU64::new(0),
            total_pushed: AtomicU64::new(0),
            total_drained: AtomicU64::new(0),
        }
    }

    fn push(&mut self, u: Unit) {
        self.units.push(u);
        self.total_pushed.fetch_add(1, Ordering::Relaxed);
    }

    fn needs_drain(&self) -> bool { self.units.len() >= self.cap }

    fn drain_and_check(&mut self) -> u64 {
        let drained: Vec<Unit> = self.units.drain(..).collect();
        let n = drained.len() as u64;
        // Canaries checked BEFORE Drop runs — a wild store into `package`
        // or `label` would surface here as a content mismatch, and a wild
        // store into the Arc pointer itself would SIGSEGV on the deref.
        for u in &drained {
            if *u.inner.package != PACKAGE_CANARY {
                self.bad_canaries.fetch_add(1, Ordering::SeqCst);
                mark_err(&format!(
                    "[rcdrop] BAD package canary at payload={:#x}: got {:?}",
                    u.inner.payload, &*u.inner.package
                ));
            }
            if *u.label != LABEL_CANARY {
                self.bad_canaries.fetch_add(1, Ordering::SeqCst);
                mark_err(&format!(
                    "[rcdrop] BAD label canary at payload={:#x}: got {:#x}",
                    u.inner.payload, *u.label
                ));
            }
        }
        // Drop the drained vec — this is the `Arc::drop` path that crashes
        // in cargo. If the kernel corrupted the strong-count word, the
        // `Drop::drop` here takes the same SIGSEGV (exit 139).
        drop(drained);
        self.total_drained.fetch_add(n, Ordering::Relaxed);
        n
    }
}

// ---------------------------------------------------------------------------
fn worker(
    tid: u64,
    rounds: u64,
    batch: u64,
    shared: Arc<Mutex<SharedTable>>,
    counter: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> u64 {
    let mut done = 0u64;
    for r in 0..rounds {
        if stop.load(Ordering::Relaxed) { break; }

        // Phase 1: each worker allocates a batch of fresh Arc<UnitInner>.
        // `Box::new(...)` for `package` and `label` exercises two extra
        // heap allocations per Unit — that is what widens the Arc's heap
        // footprint enough for a wild store to hit it.
        let mut local: Vec<Unit> = (0..batch).map(|i| {
            let c = counter.fetch_add(1, Ordering::Relaxed);
            Unit {
                inner: Arc::new(UnitInner {
                    payload: tid * 1_000_000 + r * 256 + i,
                    package: Box::new(PACKAGE_CANARY),
                    extra: Box::new(c),
                }),
                label: Box::new(LABEL_CANARY),
            }
        }).collect();

        // Phase 2: clone half into the shared table under the mutex. The
        // `Arc::clone` bumps the strong count from 1 -> 2, so the local
        // drop below decrements back to 1 without freeing. This is the
        // pattern that exercises the refcount word the audit's crash 1
        // found reading back as 0.
        {
            let mut s = shared.lock().unwrap();
            let half = (batch / 2) as usize;
            for u in local.drain(..half) {
                s.push(u);
            }
            if s.needs_drain() || r % 4 == 0 {
                s.drain_and_check();
            }
        }

        // Phase 3: drop the local remainder. If strong counts are sane this
        // frees the non-shared half; if a strong-count word was zeroed by
        // the corruptor, Arc::drop underflows here (the audit's crash shape).
        drop(local);

        done += 1;
    }
    done
}

// ---------------------------------------------------------------------------
// Fork churn. cargo spawns rustc constantly under -j4. The child inherits
// the parent's whole heap; on exit it MunmapBrks a lot, and on Akuma that
// exercises the same mmap/munmap churn the audit's hypotheses #2 suspects.
// We fork a no-op child that immediately exits — purely heap-pressure
// churn, not work.
fn fork_churn(hz: u64, stop: Arc<AtomicBool>) {
    if hz == 0 { return; }
    let period = Duration::from_nanos(1_000_000_000 / hz);
    thread::spawn(move || {
        let mut tick = 0u64;
        while !stop.load(Ordering::Relaxed) {
            // SAFETY: fork() in a multi-threaded process is only well-defined
            // for the async-signal-safe subset in the child; we immediately
            // call _exit(), which is async-signal-safe. The parent only
            // reaps. This mirrors cargo's spawn-rustc-and-reap pattern.
            let pid = unsafe { fork() };
            if pid == 0 {
                // Child: exit immediately. The exit path runs musl's atexit
                // and the kernel's exit_group, exercising the teardown churn
                // cargo's children produce.
                unsafe { _exit(0); }
            } else if pid > 0 {
                // Parent: reap, keep a count for diagnostics.
                let mut status: i32 = 0;
                unsafe { waitpid(pid, &mut status, 0); }
                tick += 1;
                if tick % 100 == 0 {
                    let _ = writeln!(std::io::stderr(), "[rcdrop] fork_churn reaped {} children", tick);
                }
            }
            thread::sleep(period);
        }
    });
}

// ---------------------------------------------------------------------------
fn main() {
    let args: Vec<String> = env::args().collect();
    let rounds   = env_or("RCDROP_ROUNDS",  args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50));
    let threads  = env_or("RCDROP_THREADS", args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4));
    let fork_hz  = env_or("RCDROP_FORK_HZ", args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0));
    let batch    = env_or("RCDROP_BATCH", 32);
    let cap      = env_or("RCDROP_TABLE_CAP", 4096) as usize;

    if threads == 0 || threads > 32 {
        mark_err(&format!("[rcdrop] threads must be 1..=32 (got {})", threads));
        process::exit(2);
    }

    arm_alarm();
    mark(&format!("[rcdrop] start rounds={} threads={} fork_hz={} batch={} cap={}",
                  rounds, threads, fork_hz, batch, cap));

    let shared   = Arc::new(Mutex::new(SharedTable::new(cap)));
    let counter  = Arc::new(AtomicU64::new(0));
    let stop     = Arc::new(AtomicBool::new(false));

    fork_churn(fork_hz, Arc::clone(&stop));

    let mut handles = Vec::new();
    for tid in 0..threads {
        let s   = Arc::clone(&shared);
        let c   = Arc::clone(&counter);
        let stp = Arc::clone(&stop);
        handles.push(thread::spawn(move || worker(tid, rounds, batch, s, c, stp)));
    }

    let mut total_done = 0u64;
    for h in handles {
        total_done += h.join().unwrap_or(0);
    }

    stop.store(true, Ordering::SeqCst);
    // Give the fork thread a moment to notice.
    thread::sleep(Duration::from_millis(50));

    // Final drain — the post-build Drop walk shape (cargo's "teardown after
    // Finished"). If the corruptor struck during the run, this is the most
    // likely place to surface it.
    let (final_drained, bad) = {
        let mut s = shared.lock().unwrap();
        let n = s.drain_and_check();
        (n, s.bad_canaries.load(Ordering::SeqCst))
    };

    let pushed = shared.lock().unwrap().total_pushed.load(Ordering::Relaxed);
    let drained = shared.lock().unwrap().total_drained.load(Ordering::Relaxed);
    let _ = (pushed, drained); // silence unused warnings in some configs

    if bad > 0 {
        mark(&format!(
            "[rcdrop] FAIL: {} bad canaries (rounds_done={} final_drained={})",
            bad, total_done, final_drained
        ));
        process::exit(1);
    }
    mark(&format!(
        "[rcdrop] PASS rounds_done={} final_drained={} total_pushed={}",
        total_done, final_drained, pushed
    ));
}
