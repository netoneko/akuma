//! `lock-ab` — what the recoverable lock costs per acquire, against the
//! `spinning_top::RwSpinlock` it replaced in `akuma-ext2`.
//!
//! This exists because the end-to-end probe cannot answer the question.
//! `ext2probe-host`'s whole run is ~0.21 s and most of that is reading the
//! image into RAM, so a 10 ms host clock cannot resolve a change worth tens of
//! nanoseconds per acquire — and `akuma-ext2` acquires on 25 sites, at least
//! once per filesystem operation. Measure the thing that changed.
//!
//! **Uncontended only, deliberately.** The contended path is a spin loop whose
//! cost is dominated by how long the *holder* holds, which is device I/O, not
//! the protocol; and the two locks' waiting behaviour differs by design
//! (writer priority, the backstop kick). What the swap can regress without
//! anyone noticing is the fast path every FS operation pays, so that is what
//! this times.
//!
//! ```text
//! cargo run --release -p akuma-locks-rw-cell --bin lock-ab \
//!   --features cli --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"
//! ```
//!
//! Driver (repeats and reports the minimum): `scripts/benchmarks/locks_rw_ab.sh`.

use std::hint::black_box;
use std::time::Instant;

use akuma_locks_rw_cell::RecoverableCell;

/// Acquire/release pairs per pass. Large enough that the loop dwarfs the clock
/// read, small enough that a pass is well under a second.
const ITERS: u64 = 20_000_000;
/// Passes per arm; the minimum is reported. A minimum, not a mean — the noise
/// on this host is all additive (scheduling, other VMs), so the fastest pass is
/// the closest estimate of the true cost.
const PASSES: usize = 7;

fn bench(label: &str, mut f: impl FnMut() -> u64) {
    let mut best = f64::MAX;
    for _ in 0..PASSES {
        let t0 = Instant::now();
        let acc = f();
        let ns = t0.elapsed().as_nanos() as f64 / ITERS as f64;
        black_box(acc);
        best = best.min(ns);
    }
    println!("{label:<34} {best:>7.2} ns/op");
}

fn main() {
    println!("uncontended acquire+release, {ITERS} iters, best of {PASSES}\n");

    let st = spinning_top::RwSpinlock::new(0u64);
    bench("spinning_top RwSpinlock write", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            let mut g = st.write();
            *g += 1;
            acc = *g;
        }
        acc
    });
    bench("spinning_top RwSpinlock read", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            acc += *st.read();
        }
        acc
    });

    let rc = RecoverableCell::new(0u64);
    bench("RecoverableCell write", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            let mut g = rc.write();
            *g += 1;
            acc = *g;
        }
        acc
    });
    bench("RecoverableCell read", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            acc += *rc.read();
        }
        acc
    });

    // What `akuma-ext2` actually calls: the per-attempt-guard loops. The guard
    // is a ZST here, as it is in every build without `no-bkl-vfs`, so this
    // isolates the closure/loop overhead the ext2 conversion added on top of a
    // plain acquire.
    bench("RecoverableCell write_holding", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            let (mut g, _h) = rc.write_holding(|| ());
            *g += 1;
            acc = *g;
        }
        acc
    });
    bench("RecoverableCell read_holding", || {
        let mut acc = 0;
        for _ in 0..ITERS {
            let (g, _h) = rc.read_holding(|| ());
            acc += *g;
        }
        acc
    });
}
