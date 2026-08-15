//! `shareprobe` — tries to trigger the share-without-inc premature-free route.
//!
//! Route 2 of `docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md`: the old
//! `[WILD-DA]` autopsies include a **writable private** victim page
//! (`AP_RW_ALL`, a 2-page anonymous region) freed by *another thread's*
//! munmap with `[COW-HIST] dec 0->0` — a frame two address spaces referenced
//! while `COW_REFCOUNTS` had **no entry at all**. The file-page cache never
//! serves writable pages, so if that route is real it lives in the fork/CoW
//! machinery: a share that skipped its `cow_ref_inc` (fork-share dedupe, a
//! demand-fault install race, the ELF `.data`/`.bss` no-region class). Then
//! either side's unmap decs 0→0 = "single owner, free it" and the frame is
//! poisoned under the survivor.
//!
//! No files are involved anywhere in this probe, so the file-page cache — and
//! the 2026-08-15 W1/W2 fixes — are entirely out of the picture. A hit here
//! is *independent* evidence for route 2.
//!
//! Shape (chosen to match the autopsy victim exactly — small 2-page anon RW
//! regions, munmap racing fork teardown):
//!
//!   - The parent holds R two-page anonymous RW regions filled with a
//!     generation-stamped pattern.
//!   - Each generation it forks children **without reaping the previous
//!     batch** (teardown overlaps everything), and each child: verifies its
//!     inherited view, CoW-breaks half the regions by scribbling, munmaps a
//!     third of them from its own side, churns a private scratch region, and
//!     exits (address-space teardown — the `as-teardown` free path).
//!   - The parent meanwhile munmaps + remaps + refills one region per
//!     generation (the `munmap-region` path racing live CoW children) and
//!     re-verifies **every** region **every** generation.
//!
//! Any parent-side mismatch is corruption of private memory the kernel had no
//! right to touch. A qword whose high half is `0xFEEDFACE` is quarantine
//! poison and names its own frame (`pa = word ^ 0xFEEDFACE_DEAD0000`).
//!
//! Verdict lines for a log grep:
//!   `shareprobe: ALL PASS`  /  `shareprobe: CORRUPTION events=<n>`
//!
//! Detection notes: `[PMM-RESURRECT]` can NOT see this route (no inc ever
//! happens — there is nothing to resurrect), which is exactly why the probe
//! carries its own witness. A worker dying of SIGSEGV counts as a hit too
//! (a poisoned qword loaded as a pointer — the cargo null-`Rc` shape).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use libakuma::mmap_flags::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use libakuma::{exit, fork, mmap, munmap, println, sleep_ms, wait_any, ForkResult};

const PAGE: usize = 4096;
/// 2 pages per region — the autopsy victim's exact shape.
const REGION_PAGES: usize = 2;
const REGIONS: usize = 8;
/// Fork/verify generations.
const GENERATIONS: usize = 300;
/// Children forked per generation.
const CHILDREN_PER_GEN: usize = 4;
/// How many unreaped children may pile up before draining — keeps teardown
/// overlapping the parent's faults instead of serialising behind it.
const REAP_BACKLOG: usize = 8;

/// Expected qword `q` of page `p` in region `r`, as stamped at generation `g`.
/// High half `0x5A17_xxxx` can never collide with `0xFEEDFACE` poison.
#[inline]
fn pat(r: usize, g: usize, p: usize, q: usize) -> u64 {
    0x5A17_0000_0000_0000
        ^ ((r as u64) << 40)
        ^ ((g as u64) << 16)
        ^ ((p as u64) << 12)
        ^ (q as u64)
}

/// Child-scribble pattern — distinct from every parent stamp.
#[inline]
fn scribble(c: usize, q: usize) -> u64 {
    0x5C1B_0000_0000_0000 ^ ((c as u64) << 32) ^ (q as u64)
}

fn classify(got: u64) -> &'static str {
    if got >> 32 == 0xFEED_FACE {
        "QUARANTINE POISON (pa = word ^ 0xFEEDFACE_DEAD0000)"
    } else if got == 0 {
        "zeros (recycled/re-zeroed frame)"
    } else if got >> 48 == 0x5C1B {
        "a CHILD'S scribble (CoW isolation broken)"
    } else if got >> 48 == 0x5A17 {
        "a STALE generation stamp (lost write / stale frame)"
    } else {
        "foreign bytes (recycled frame, new owner's data)"
    }
}

const REGION_BYTES: usize = REGION_PAGES * PAGE;
const QWORDS: usize = PAGE / 8;

fn map_region() -> Option<usize> {
    let a = mmap(
        0,
        REGION_BYTES,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
    );
    if a == 0 || a > usize::MAX - REGION_BYTES {
        None
    } else {
        Some(a)
    }
}

fn stamp(addr: usize, r: usize, g: usize) {
    for p in 0..REGION_PAGES {
        for q in 0..QWORDS {
            unsafe {
                ((addr + p * PAGE + q * 8) as *mut u64).write_volatile(pat(r, g, p, q));
            }
        }
    }
}

/// Verify a region against its stamp; report at most `budget` mismatches.
/// Returns the mismatch count. Repairs what it finds so one bad frame does
/// not spam every later generation.
fn verify(tag: &str, addr: usize, r: usize, g: usize, gen_now: usize, budget: &mut usize) -> usize {
    let mut bad = 0usize;
    for p in 0..REGION_PAGES {
        for q in 0..QWORDS {
            let a = addr + p * PAGE + q * 8;
            let got = unsafe { (a as *const u64).read_volatile() };
            let want = pat(r, g, p, q);
            if got != want {
                bad += 1;
                if *budget > 0 {
                    *budget -= 1;
                    println(&format!(
                        "shareprobe:   [{}] CORRUPTION gen={} region={} (stamped g={}) page={} q={} got={:#018x} want={:#018x} — {}",
                        tag, gen_now, r, g, p, q, got, want, classify(got)
                    ));
                }
                unsafe { (a as *mut u64).write_volatile(want) };
            }
        }
    }
    bad
}

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    println("shareprobe: starting (fork/CoW share-without-inc probe, no files)");

    let mut addrs: Vec<usize> = Vec::with_capacity(REGIONS);
    let mut gens: Vec<usize> = Vec::with_capacity(REGIONS);
    for r in 0..REGIONS {
        match map_region() {
            Some(a) => {
                stamp(a, r, 0);
                addrs.push(a);
                gens.push(0);
            }
            None => {
                println("shareprobe: SETUP FAIL (mmap)");
                exit(1);
            }
        }
    }

    let mut events = 0usize;
    let mut report_budget = 16usize;
    let mut pending = 0usize;
    let mut child_hits = 0usize;

    for gen in 1..=GENERATIONS {
        // Fork this generation's children against the current snapshot.
        for c in 0..CHILDREN_PER_GEN {
            match fork() {
                Ok(ForkResult::Child) => {
                    exit(run_child(c, gen, &addrs, &gens));
                }
                Ok(ForkResult::Parent(_)) => pending += 1,
                Err(_) => {
                    // Table pressure — drain and move on.
                    break;
                }
            }
        }

        // The munmap-under-CoW race: retire one region while children of this
        // and previous generations still share its frames, then remap and
        // restamp it. This is the `munmap-region` free path firing against
        // live sharers, every generation.
        let r = gen % REGIONS;
        munmap(addrs[r], REGION_BYTES);
        match map_region() {
            Some(a) => {
                stamp(a, r, gen);
                addrs[r] = a;
                gens[r] = gen;
            }
            None => {
                println("shareprobe: remap failed under pressure — stopping early");
                break;
            }
        }

        // Parent-side full verification: private RW memory must be exactly
        // what the parent last stamped, no matter what forks/unmaps/teardowns
        // are in flight.
        for i in 0..REGIONS {
            events += verify("parent", addrs[i], i, gens[i], gen, &mut report_budget);
        }

        // CoW-break churn on a second region (writes into shared frames while
        // children hold them RO).
        let r2 = (gen + REGIONS / 2) % REGIONS;
        stamp(addrs[r2], r2, gen);
        gens[r2] = gen;

        // Drain the backlog.
        while pending > REAP_BACKLOG {
            match wait_any() {
                Some(st) => {
                    pending -= 1;
                    if st.signaled() {
                        child_hits += 1;
                        println(&format!(
                            "shareprobe:   child {} DIED from signal {:?} gen={} — counts as corruption",
                            st.pid,
                            st.term_signal(),
                            gen
                        ));
                    } else if st.exit_code() != 0 {
                        child_hits += st.exit_code() as usize;
                    }
                }
                None => sleep_ms(2),
            }
        }

        if gen % 50 == 0 {
            println(&format!(
                "shareprobe: gen {}/{} events={} child_hits={}",
                gen, GENERATIONS, events, child_hits
            ));
        }
    }

    // Final drain.
    let mut waited = 0u64;
    while pending > 0 && waited < 20_000 {
        match wait_any() {
            Some(st) => {
                pending -= 1;
                if st.signaled() {
                    child_hits += 1;
                } else if st.exit_code() != 0 {
                    child_hits += st.exit_code() as usize;
                }
            }
            None => {
                sleep_ms(5);
                waited += 5;
            }
        }
    }

    let total = events + child_hits;
    if total == 0 {
        println("shareprobe: ALL PASS");
        exit(0);
    } else {
        println(&format!(
            "shareprobe: CORRUPTION events={} (parent={} children={})",
            total, events, child_hits
        ));
        exit(2);
    }
}

/// One forked child. Its exit code is its corruption count (capped at 100);
/// 0 means its whole CoW view behaved.
fn run_child(c: usize, gen: usize, addrs: &[usize], gens: &[usize]) -> i32 {
    let mut bad = 0usize;
    let mut budget = 4usize;

    // 1. The inherited CoW view must read exactly as stamped at fork time.
    for r in 0..REGIONS {
        bad += verify("child", addrs[r], r, gens[r], gen, &mut budget);
    }

    // 2. CoW-break half the regions by scribbling, then verify our private
    //    copies stuck (a lost CoW break shows here).
    for r in 0..REGIONS {
        if r % 2 == c % 2 {
            for q in 0..QWORDS {
                unsafe {
                    ((addrs[r] + q * 8) as *mut u64).write_volatile(scribble(c, q));
                }
            }
            for q in 0..QWORDS {
                let got = unsafe { ((addrs[r] + q * 8) as *const u64).read_volatile() };
                if got != scribble(c, q) {
                    bad += 1;
                }
            }
        }
    }

    // 3. Drop a third of the regions from this side — child-side munmap of
    //    frames the parent (and sibling children) still map. This is the
    //    dec-under-sharers edge; with correct accounting it releases only
    //    this AS's reference.
    for r in 0..REGIONS {
        if (r + c) % 3 == 0 {
            munmap(addrs[r], REGION_BYTES);
        }
    }

    // 4. Recycling churn: allocate, stamp, verify and free a private scratch
    //    region so freed frames get re-handed-out while siblings race.
    if let Some(s) = map_region() {
        stamp(s, REGIONS + c, gen);
        let mut b = 0usize;
        b += verify("child-scratch", s, REGIONS + c, gen, gen, &mut budget);
        munmap(s, REGION_BYTES);
        bad += b;
    }

    bad.min(100) as i32
}
