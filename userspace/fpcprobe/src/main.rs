//! `fpcprobe` — tries to trigger the file-page-cache premature-free race.
//!
//! Route 1 of `docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md`: the shared
//! file-page cache's reference protocol raced its own free paths (W1/W2 in
//! `docs/reference/subsystems/memory.md` § "Frame lifecycle"), so a frame
//! serving a read-only file page could be freed — and quarantine-poisoned —
//! while a mapper still held it. The victim only ever *reads* the poison, so
//! no kernel instrument fires; the only witness is wrong bytes where file
//! content belongs.
//!
//! This probe recreates the triangle at maximum density:
//!
//!   - N reader workers mmap the same file `PROT_READ` (the exact shape
//!     `rust-lld` uses on `.rlib`s — cache-eligible), fault every page in,
//!     verify a known pattern, munmap, repeat.
//!   - One writer rewrites pages with **identical content** in a tight loop.
//!     Content-identical writes keep every reader's verification valid while
//!     still driving `vfs::invalidate_file_pages` → `invalidate_inode` →
//!     `free_page_at(FpcacheInvalidate)` on every write — the free half of
//!     the W1 race. An occasional rename drives the resolve-then-invalidate
//!     path too.
//!   - Every worker also keeps a private RW anonymous canary arena and
//!     verifies it each generation: second-order damage (a desynced frame
//!     recycled into someone else's private memory and then freed under
//!     them) lands here.
//!
//! Any verification mismatch is corruption, and the two signatures this probe
//! can produce name **different bugs**:
//!
//!   - a qword whose high half is `0xFEEDFACE` is quarantine poison — the
//!     premature-free race (`pa = word ^ 0xFEEDFACE_DEAD0000`);
//!   - a whole page of **zeros** paired with kernel-side
//!     `[FILL-SHORT] inode=0 … Err(NotFound)` is the *path-identity* defect
//!     this probe found on its first run (2026-08-15): the fd layer
//!     (`KernelFile`) is path-identified, so an mmap that races a rename
//!     records `inode=0` and a fault landing in a later rename window
//!     zero-fills instead of reading the file. Linux semantics pin the inode
//!     at map time; Akuma's path-only fallback cannot. Run with `norename`
//!     (argv[1]) to take renames — and that whole bug — out of the picture
//!     and exercise only the invalidate/evict machinery.
//!
//! Verdict lines for a log grep:
//!   `fpcprobe: ALL PASS`  /  `fpcprobe: CORRUPTION events=<n>`
//!
//! On the pre-fix kernel the poison signature is the route behind ~60 % red
//! self-host builds; on the fixed kernel `norename` runs must stay silent,
//! and `[PMM-RESURRECT]` must not print (that detector fires at the exact
//! moment the premature-free race is won).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use libakuma::mmap_flags::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use libakuma::open_flags::{O_CREAT, O_RDONLY, O_WRONLY};
use libakuma::{
    close, exit, fork, getpid, lseek, mmap, munmap, open, println, rename, sleep_ms, syscall,
    unlink, wait_any, write, ForkResult,
};

const PAGE: usize = 4096;
/// File size in pages. Small enough to re-fault quickly, large enough that a
/// mapping outlives many invalidations.
const FILE_PAGES: usize = 48;
/// Reader workers. With the writer and the parent this oversubscribes SMP=4,
/// which is the point — the race needs preemption inside the window.
const WORKERS: usize = 6;
/// Map/verify/unmap cycles per worker.
const GENERATIONS: usize = 300;
/// Private RW canary pages per worker.
const CANARY_PAGES: usize = 8;

const DATA_PATH: &str = "/tmp/fpcprobe.dat";
const DATA_TMP: &str = "/tmp/fpcprobe.tmp";
const STOP_PATH: &str = "/tmp/fpcprobe.stop";

/// Set from argv before any fork; the writer child inherits it. `norename`
/// takes the rename dance — and the fd path-identity bug it triggers — out of
/// the run, leaving only the invalidate/evict machinery under test.
static RENAME_DANCE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Expected qword `q` of file page `page`. High half `0x5EED_xxxx` can never
/// collide with the `0xFEEDFACE` poison prefix, so poison is unambiguous.
#[inline]
fn file_pat(page: usize, q: usize) -> u64 {
    0x5EED_0000_0000_0000 ^ ((page as u64) << 24) ^ (q as u64)
}

/// Expected qword `q` of canary page `page` for worker `pid`.
#[inline]
fn canary_pat(pid: u32, page: usize, q: usize) -> u64 {
    0x5EED_CAFE_0000_0000 ^ ((pid as u64) << 20) ^ ((page as u64) << 12) ^ (q as u64)
}

fn classify(got: u64) -> &'static str {
    if got >> 32 == 0xFEED_FACE {
        "QUARANTINE POISON (pa = word ^ 0xFEEDFACE_DEAD0000)"
    } else if got == 0 {
        "zeros (recycled/re-zeroed frame)"
    } else {
        "foreign bytes (recycled frame, new owner's data)"
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    let mut norename = false;
    for a in libakuma::args() {
        if a == "norename" {
            norename = true;
        }
    }
    if norename {
        RENAME_DANCE.store(false, core::sync::atomic::Ordering::Relaxed);
    }
    println(if norename {
        "fpcprobe: starting (file-page-cache race probe, renames OFF)"
    } else {
        "fpcprobe: starting (file-page-cache race probe)"
    });
    let _ = unlink(STOP_PATH);

    if !write_data_file() {
        println("fpcprobe: SETUP FAIL (could not create data file)");
        exit(1);
    }

    // The writer child: invalidation pressure until the stop file appears.
    let writer_pid = match fork() {
        Ok(ForkResult::Child) => {
            exit(run_writer());
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("fpcprobe: SETUP FAIL (writer fork errno {})", e));
            exit(1);
        }
    };

    let mut worker_pids: Vec<u32> = Vec::new();
    for _ in 0..WORKERS {
        match fork() {
            Ok(ForkResult::Child) => {
                exit(run_worker());
            }
            Ok(ForkResult::Parent(pid)) => worker_pids.push(pid),
            Err(e) => {
                println(&format!("fpcprobe: worker fork errno {} — continuing with fewer", e));
                break;
            }
        }
    }
    println(&format!(
        "fpcprobe: {} workers + 1 writer live, {} generations each",
        worker_pids.len(),
        GENERATIONS
    ));

    // Reap the workers; their exit code is their corruption count (capped).
    let mut events = 0usize;
    let mut reaped = 0usize;
    while reaped < worker_pids.len() {
        match wait_any() {
            Some(st) if st.pid == writer_pid => {
                // Writer died early — that is a failure of pressure, not of
                // correctness, but say so.
                println(&format!(
                    "fpcprobe: writer exited early (raw=0x{:x})",
                    st.raw
                ));
            }
            Some(st) => {
                reaped += 1;
                if st.signaled() {
                    // A SIGSEGV here is itself a hit: a poisoned pointer read.
                    println(&format!(
                        "fpcprobe: worker {} DIED from signal {:?} — counts as corruption",
                        st.pid,
                        st.term_signal()
                    ));
                    events += 1;
                } else {
                    events += st.exit_code() as usize;
                }
            }
            None => sleep_ms(10),
        }
    }

    // Tell the writer to stop, then reap it.
    let stop = open(STOP_PATH, O_CREAT | O_WRONLY);
    if stop >= 0 {
        close(stop);
    }
    let mut waited = 0u64;
    while waited < 10_000 {
        match wait_any() {
            Some(st) if st.pid == writer_pid => break,
            Some(_) => {}
            None => {
                sleep_ms(10);
                waited += 10;
            }
        }
    }

    let _ = unlink(STOP_PATH);
    if events == 0 {
        println("fpcprobe: ALL PASS");
        exit(0);
    } else {
        println(&format!("fpcprobe: CORRUPTION events={}", events));
        exit(2);
    }
}

/// Create the data file with its full pattern.
fn write_data_file() -> bool {
    let _ = unlink(DATA_PATH);
    let fd = open(DATA_PATH, O_CREAT | O_WRONLY);
    if fd < 0 {
        return false;
    }
    let mut page_buf = [0u8; PAGE];
    for page in 0..FILE_PAGES {
        fill_page(&mut page_buf, page);
        if write(fd, &page_buf) != PAGE as isize {
            close(fd);
            return false;
        }
    }
    close(fd) == 0
}

fn fill_page(buf: &mut [u8; PAGE], page: usize) {
    for q in 0..(PAGE / 8) {
        let v = file_pat(page, q);
        buf[q * 8..q * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// fd-backed mmap — `libakuma::mmap` hardcodes fd/offset to 0, so go raw.
fn mmap_file(len: usize, fd: i32) -> Option<usize> {
    let ret = syscall(
        libakuma::syscall::MMAP,
        0,
        len as u64,
        PROT_READ as u64,
        MAP_PRIVATE as u64,
        fd as u64,
        0,
    ) as usize;
    // Failure is usize::MAX / a negative errno in the top page.
    if ret == 0 || ret > usize::MAX - PAGE {
        None
    } else {
        Some(ret)
    }
}

// ============================================================================
// Reader worker
// ============================================================================

fn run_worker() -> i32 {
    let pid = getpid();
    let mut events = 0usize;

    // Private RW canary arena — where second-order damage would land.
    let canary = mmap(
        0,
        CANARY_PAGES * PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
    );
    let canary_ok = canary != 0 && canary <= usize::MAX - CANARY_PAGES * PAGE;
    if canary_ok {
        for page in 0..CANARY_PAGES {
            for q in 0..(PAGE / 8) {
                unsafe {
                    ((canary + page * PAGE + q * 8) as *mut u64)
                        .write_volatile(canary_pat(pid, page, q));
                }
            }
        }
    }

    for gen in 0..GENERATIONS {
        // The file may be mid-rename; retry the open briefly.
        let mut fd = -1;
        for _ in 0..200 {
            fd = open(DATA_PATH, O_RDONLY);
            if fd >= 0 {
                break;
            }
            sleep_ms(2);
        }
        if fd < 0 {
            println(&format!("fpcprobe:   [w{}] open never succeeded (gen {})", pid, gen));
            return events.min(100) as i32;
        }

        match mmap_file(FILE_PAGES * PAGE, fd) {
            Some(base) => {
                // Fault every page in and verify a sample of qwords. The first
                // read of each page is the demand fault that takes the cache's
                // lookup_and_ref path — the inc half of the W1 race.
                for page in 0..FILE_PAGES {
                    for q in [0usize, 1, 7, 255, 256, 511] {
                        let addr = base + page * PAGE + q * 8;
                        let got = unsafe { (addr as *const u64).read_volatile() };
                        let want = file_pat(page, q);
                        if got != want && events < 16 {
                            events += 1;
                            println(&format!(
                                "fpcprobe:   [w{}] FILE CORRUPTION gen={} page={} q={} got={:#018x} want={:#018x} — {}",
                                pid, gen, page, q, got, want, classify(got)
                            ));
                        } else if got != want {
                            events += 1;
                        }
                    }
                }
                munmap(base, FILE_PAGES * PAGE);
            }
            None => {
                // Transient pressure failure — not corruption.
            }
        }
        close(fd);

        // Canary sweep: private RW memory must never change underneath us.
        if canary_ok {
            for page in 0..CANARY_PAGES {
                for q in 0..(PAGE / 8) {
                    let addr = canary + page * PAGE + q * 8;
                    let got = unsafe { (addr as *const u64).read_volatile() };
                    let want = canary_pat(pid, page, q);
                    if got != want {
                        events += 1;
                        if events <= 16 {
                            println(&format!(
                                "fpcprobe:   [w{}] CANARY CORRUPTION gen={} page={} q={} got={:#018x} — {}",
                                pid, gen, page, q, got, classify(got)
                            ));
                        }
                        // Repair so one bad frame doesn't spam every later gen.
                        unsafe { (addr as *mut u64).write_volatile(want) };
                    }
                }
            }
        }
    }

    events.min(100) as i32
}

// ============================================================================
// Writer — invalidation pressure
// ============================================================================

fn run_writer() -> i32 {
    let mut page_buf = [0u8; PAGE];
    let mut iter = 0usize;
    loop {
        // Stop when the parent says so.
        let stop = open(STOP_PATH, O_RDONLY);
        if stop >= 0 {
            close(stop);
            return 0;
        }

        // Content-identical rewrite of one page: fires invalidate_inode (the
        // free half of the race) without ever making a reader's expected
        // bytes wrong.
        let page = iter % FILE_PAGES;
        let fd = open(DATA_PATH, O_WRONLY);
        if fd >= 0 {
            fill_page(&mut page_buf, page);
            if lseek(fd, (page * PAGE) as i64, 0) >= 0 {
                let _ = write(fd, &page_buf);
            }
            close(fd);
        }

        // Occasionally exercise the rename invalidation path (resolve inode
        // before the path stops naming it). Readers retry their open. Skipped
        // under `norename` — see the module doc: on a path-identified fd
        // layer this window has its own, separate wrong-bytes bug.
        if iter % 64 == 63 && RENAME_DANCE.load(core::sync::atomic::Ordering::Relaxed) {
            let _ = rename(DATA_PATH, DATA_TMP);
            let _ = rename(DATA_TMP, DATA_PATH);
        }

        iter += 1;
        if iter % 512 == 0 {
            sleep_ms(1); // let starved readers make progress
        }
    }
}
