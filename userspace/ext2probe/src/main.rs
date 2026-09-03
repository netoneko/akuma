//! `ext2probe` (guest binary) — file create/write/read/list throughput against
//! the live kernel filesystem, run once on a clean directory and again after a
//! large create-then-mass-delete churn, to test whether ordinary ext2 ops
//! measurably regress after a bulk `rm -rf` (see
//! `docs/archive/EXT2_PERFORMANCE_AUDIT.md`).
//!
//! Workload shapes are shared with the host device-I/O probe (`ext2probe-host`)
//! via the crate library — see `src/lib.rs`.
//!
//! Usage: `ext2probe [stress_files_per_dir] [stress_dirs]`
//!   Defaults: 200 files/dir, 16 dirs (3200 files, ~12.5 MB) for the stress
//!   phase. Verdict line for a log grep: `ext2probe: REGRESSION` /
//!   `ext2probe: NO REGRESSION`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use ext2probe::{
    workload, FsOps, BASE_N, DEFAULT_TREE_DIRS, DEFAULT_TREE_FILES, FILE_SIZE, SEQ_BYTES, SEQ_CHUNK,
};
use libakuma::open_flags::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use libakuma::{
    clock_gettime, close, exit, mkdir, open, print, print_dec, println, read, read_dir, rmdir,
    unlink, write, Timespec, CLOCK_MONOTONIC,
};

/// A degradation this large on any single op after the mass delete is called out
/// as a regression in the final verdict line.
const REGRESSION_PCT: i64 = 20;
/// Linux/musl `EEXIST` — not exported by libakuma, hardcoded per the ABI docs.
const EEXIST: i32 = 17;

/// [`FsOps`] over libakuma syscalls, against the real kernel filesystem.
struct GuestFsOps;

impl FsOps for GuestFsOps {
    fn mkdir(&self, path: &str) {
        let r = mkdir(path);
        if r != 0 && r != -EEXIST {
            print("ext2probe: WARNING mkdir(");
            print(path);
            print(") rc=");
            print_dec((-r) as usize);
            println("");
        }
    }
    fn rmdir(&self, path: &str) {
        rmdir(path);
    }
    fn create_write(&self, path: &str, data: &[u8]) {
        let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd < 0 {
            print("ext2probe: WARNING open failed for ");
            println(path);
            return;
        }
        let mut off = 0;
        while off < data.len() {
            let w = write(fd, &data[off..]);
            if w <= 0 {
                break;
            }
            off += w as usize;
        }
        close(fd);
    }
    fn seq_write(&self, path: &str, total: usize, chunk: usize) {
        let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd < 0 {
            println("ext2probe: WARNING seq_write open failed");
            return;
        }
        let buf = [0xCDu8; SEQ_CHUNK];
        let mut written = 0;
        while written < total {
            let want = core::cmp::min(chunk.min(SEQ_CHUNK), total - written);
            let n = write(fd, &buf[..want]);
            if n <= 0 {
                break;
            }
            written += n as usize;
        }
        close(fd);
    }
    fn read_all(&self, path: &str) -> usize {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            println("ext2probe: WARNING read open failed");
            return 0;
        }
        let mut buf = [0u8; SEQ_CHUNK];
        let mut total = 0;
        loop {
            let n = read(fd, &mut buf);
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        close(fd);
        total
    }
    fn unlink(&self, path: &str) {
        unlink(path);
    }
    fn list_dir(&self, path: &str) -> usize {
        read_dir(path).map(Iterator::count).unwrap_or(0)
    }
    fn stat(&self, path: &str) {
        // No direct stat wrapper here; an O_RDONLY open + close is the closest
        // metadata touch and is all the deep-path scenario needs.
        let fd = open(path, O_RDONLY);
        if fd >= 0 {
            close(fd);
        }
    }
    fn now_us(&self) -> u64 {
        let mut ts = Timespec::default();
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000
    }
}

fn print_us(label: &str, us: u64) {
    print(label);
    print_dec(us as usize);
    println(" us");
}

/// One (create N files, seq write+read a big file, list, delete) pass.
/// Returns (create_us, write_us, read_us, list_us).
fn baseline_pass(ops: &GuestFsOps, tag: &str, dir: &str) -> (u64, u64, u64, u64) {
    println("");
    print("ext2probe: --- ");
    print(tag);
    println(" pass ---");
    ops.mkdir(dir);

    let t0 = ops.now_us();
    workload::create_files(ops, dir, BASE_N, FILE_SIZE);
    let create_us = ops.now_us() - t0;
    print_us("ext2probe: create:    ", create_us);

    let big = format!("{dir}/big.dat");
    let t0 = ops.now_us();
    workload::seq_write(ops, &big, SEQ_BYTES, SEQ_CHUNK);
    let write_us = ops.now_us() - t0;
    print_us("ext2probe: seq_write: ", write_us);

    let t0 = ops.now_us();
    let read_bytes = ops.read_all(&big);
    let read_us = ops.now_us() - t0;
    print_us("ext2probe: seq_read:  ", read_us);
    if read_bytes != SEQ_BYTES {
        print("ext2probe: WARNING seq_read got ");
        print_dec(read_bytes);
        print(" bytes, expected ");
        print_dec(SEQ_BYTES);
        println("");
    }

    let t0 = ops.now_us();
    let listed = ops.list_dir(dir);
    let list_us = ops.now_us() - t0;
    print_us("ext2probe: list_dir:  ", list_us);
    print("ext2probe:   (");
    print_dec(listed);
    println(" entries)");

    ops.unlink(&big);
    let t0 = ops.now_us();
    workload::delete_files(ops, dir, BASE_N);
    let delete_us = ops.now_us() - t0;
    print_us("ext2probe: delete:    ", delete_us);

    (create_us, write_us, read_us, list_us)
}

/// Build a `dirs × files_per_dir` tree and mass-delete it in one timed pass —
/// the `rm -rf`-shaped operation. Returns (delete_us, total_files).
fn stress_pass(ops: &GuestFsOps, root: &str, dirs: usize, files_per_dir: usize) -> (u64, usize) {
    println("");
    println("ext2probe: --- stress pass (simulated `rm -rf` of a big tree) ---");
    print("ext2probe: building ");
    print_dec(dirs);
    print(" dirs x ");
    print_dec(files_per_dir);
    println(" files");

    let t0 = ops.now_us();
    workload::build_tree(ops, root, dirs, files_per_dir);
    print_us("ext2probe: build tree:  ", ops.now_us() - t0);

    let t0 = ops.now_us();
    workload::mass_delete_tree(ops, root, dirs, files_per_dir);
    let delete_us = ops.now_us() - t0;
    print_us("ext2probe: mass delete: ", delete_us);

    (delete_us, dirs * files_per_dir)
}

/// Prints the before/after comparison for one metric and returns whether it
/// crossed [`REGRESSION_PCT`].
fn print_delta(label: &str, before: u64, after: u64) -> bool {
    let delta: i64 = if before == 0 {
        0
    } else {
        ((after as i64 - before as i64) * 100) / before as i64
    };
    print("ext2probe: ");
    print(label);
    print(": before=");
    print_dec(before as usize);
    print("us after=");
    print_dec(after as usize);
    print("us delta=");
    if delta < 0 {
        print("-");
        print_dec((-delta) as usize);
    } else {
        print_dec(delta as usize);
    }
    println("%");
    delta >= REGRESSION_PCT
}

/// Fraction of deleted bytes that must come back before the filesystem is
/// considered to be reclaiming. Well below 100 % on purpose: directory blocks,
/// group metadata and the probe's own dirs move a little in both directions.
const RECLAIM_OK_PCT: i64 = 80;

/// Unpinned control phase: bytes to create and delete with nothing mapping them.
const RECLAIM_BYTES: usize = 32 * 1024 * 1024;
const RECLAIM_FILE_SIZE: usize = 256 * 1024;

/// Pinned phase. **Deliberately more than `inode_pin`'s 1024 slots**: the leak
/// needs the pin table to overflow, after which `is_pinned` answers `true` for
/// every inode and every unlink defers.
const PINNED_FILES: usize = 1200;
const PINNED_FILE_SIZE: usize = 64 * 1024;

/// `(consumed_bytes, returned_bytes)` for a create-then-delete cycle, or `None`
/// if `statfs` is unavailable.
fn space_delta(before: u64, mid: u64, after: u64, bsize: u64) -> (u64, u64) {
    (
        before.saturating_sub(mid) * bsize,
        after.saturating_sub(mid) * bsize,
    )
}

fn report(label: &str, consumed: u64, returned: u64) -> Option<i64> {
    print("ext2probe: reclaim[");
    print(label);
    print("]: consumed=");
    print_dec((consumed / 1024) as usize);
    print("KB returned=");
    print_dec((returned / 1024) as usize);
    println("KB");
    if consumed == 0 {
        print("ext2probe: reclaim[");
        print(label);
        println("]: INCONCLUSIVE (create consumed no measurable space)");
        return None;
    }
    let pct = (returned as i64).saturating_mul(100) / consumed as i64;
    print("ext2probe: reclaim[");
    print(label);
    print("]: ");
    print_dec(pct.max(0) as usize);
    println("% of deleted bytes returned");
    Some(pct)
}

/// Control: create and delete files that **nothing maps**. `release_last_link`
/// takes its immediate-free path here, so this must reclaim ~everything. If even
/// this leaks, the defect is not the pin/deferral interaction.
fn reclaim_unpinned(ops: &dyn FsOps, dir: &str) -> Option<i64> {
    let n = RECLAIM_BYTES / RECLAIM_FILE_SIZE;
    let (bsize, _, before) = libakuma::statfs_free("/")?;
    ops.mkdir(dir);
    workload::create_files(ops, dir, n, RECLAIM_FILE_SIZE);
    let (_, _, mid) = libakuma::statfs_free("/")?;
    workload::delete_files(ops, dir, n);
    rmdir(dir);
    let (_, _, after) = libakuma::statfs_free("/")?;
    let (c, r) = space_delta(before, mid, after, bsize);
    report("unpinned", c, r)
}

/// **The model of the leak.** Unlink every file *while a mapping still holds its
/// inode*, then drop the mappings and ask whether the blocks came back.
///
/// Sequence, and every step matters:
///   1. create `PINNED_FILES` files (> the 1024-slot pin table on purpose),
///   2. `mmap` each one file-backed and close the fd — the mapping keeps the
///      `InodePin`, so the inode is pinned with no fd open,
///   3. `unlink` all of them: `is_pinned` is true, so each free is *deferred*
///      onto the 256-slot list, which overflows at 256 and — before the fix —
///      silently leaked the rest because `release_last_link` discarded
///      `DeferredFrees::push`'s `false`,
///   4. `munmap` everything, dropping the pins,
///   5. the next filesystem operation should drain the deferral list and return
///      the blocks.
///
/// A healthy filesystem returns ~all of it. Leaking returns ~0. Measured on the
/// pre-fix kernel: 49.4 MB deleted, **0 bytes back**.
fn reclaim_pinned(ops: &dyn FsOps, dir: &str) -> Option<i64> {
    use libakuma::{close, mmap_file, munmap, open, open_flags::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY}};
    const PROT_READ: u32 = 0x1;
    const MAP_PRIVATE: u32 = 0x02;

    let (bsize, _, before) = libakuma::statfs_free("/")?;
    ops.mkdir(dir);

    // 1. create
    let buf = alloc::vec![b'p'; PINNED_FILE_SIZE];
    for i in 0..PINNED_FILES {
        let path = alloc::format!("{}/p{}", dir, i);
        let fd = open(&path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd < 0 {
            break;
        }
        let _ = write(fd, &buf);
        close(fd);
    }
    let (_, _, mid) = libakuma::statfs_free("/")?;

    // 2. map each, then close the fd — the mapping holds the pin
    let mut maps = alloc::vec::Vec::with_capacity(PINNED_FILES);
    let mut pinned = 0usize;
    for i in 0..PINNED_FILES {
        let path = alloc::format!("{}/p{}", dir, i);
        let fd = open(&path, O_RDONLY);
        if fd < 0 {
            continue;
        }
        let addr = mmap_file(0, PINNED_FILE_SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
        close(fd);
        if addr != usize::MAX && addr != 0 {
            maps.push(addr);
            pinned += 1;
        }
    }
    print("ext2probe: reclaim[pinned]: mapped ");
    print_dec(pinned);
    print(" of ");
    print_dec(PINNED_FILES);
    println(" files (pin table has 1024 slots)");

    // 3. unlink while mapped -> every free is deferred
    for i in 0..PINNED_FILES {
        let path = alloc::format!("{}/p{}", dir, i);
        libakuma::unlink(&path);
    }

    // 4. drop the pins
    for addr in &maps {
        munmap(*addr, PINNED_FILE_SIZE);
    }

    // 5. give the drain something to run on, then measure
    let drain = alloc::format!("{}/drain", dir);
    let fd = open(&drain, O_CREAT | O_WRONLY | O_TRUNC);
    if fd >= 0 {
        let _ = write(fd, b"x");
        close(fd);
    }
    libakuma::unlink(&drain);
    rmdir(dir);

    let (_, _, after) = libakuma::statfs_free("/")?;
    let (c, r) = space_delta(before, mid, after, bsize);
    report("pinned", c, r)
}

#[no_mangle]
pub extern "C" fn main() {
    let stress_files: usize = libakuma::arg(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TREE_FILES);
    let stress_dirs: usize = libakuma::arg(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TREE_DIRS);

    let ops = GuestFsOps;

    println("ext2probe: starting");
    print("ext2probe: params base_n=");
    print_dec(BASE_N);
    print(" seq_bytes=");
    print_dec(SEQ_BYTES);
    print(" stress_files_per_dir=");
    print_dec(stress_files);
    print(" stress_dirs=");
    print_dec(stress_dirs);
    println("");

    ops.mkdir("/probe");

    let (c1, w1, r1, l1) = baseline_pass(&ops, "BEFORE", "/probe/base1");

    let (mass_delete_us, total_files) = stress_pass(&ops, "/probe/stress", stress_dirs, stress_files);
    print("ext2probe: mass delete rate: ");
    if mass_delete_us > 0 {
        print_dec((total_files as u64 * 1_000_000 / mass_delete_us) as usize);
    } else {
        print_dec(0);
    }
    println(" files/sec");

    let (c2, w2, r2, l2) = baseline_pass(&ops, "AFTER", "/probe/base2");

    println("");
    println("ext2probe: --- comparison (before vs after mass delete) ---");
    let mut regressed = false;
    regressed |= print_delta("create   ", c1, c2);
    regressed |= print_delta("seq_write", w1, w2);
    regressed |= print_delta("seq_read ", r1, r2);
    regressed |= print_delta("list_dir ", l1, l2);

    // Space reclamation — a different failure from the timing one above, and the
    // one that actually kills long campaigns. Run AFTER the stress pass, because
    // the pin-table overflow that triggers the leak needs a workload to provoke
    // it; on a freshly booted guest the tables are empty and unlink works.
    println("");
    println("ext2probe: --- space reclamation ---");
    let unpinned = reclaim_unpinned(&ops, "/probe/reclaim");
    let pinned = reclaim_pinned(&ops, "/probe/pinned");

    rmdir("/probe");

    if regressed {
        println("ext2probe: REGRESSION (>=20% slower on at least one op after the mass delete)");
    } else {
        println("ext2probe: NO REGRESSION");
    }
    // Two verdicts, because they fail for different reasons. `unpinned` failing
    // means the ordinary free path is broken; `pinned` failing is the
    // pin/deferral leak specifically.
    match (unpinned, pinned) {
        (Some(u), Some(p)) if u >= RECLAIM_OK_PCT && p >= RECLAIM_OK_PCT => {
            println("ext2probe: SPACE OK")
        }
        (Some(u), Some(_)) if u >= RECLAIM_OK_PCT => println(
            "ext2probe: SPACE LEAK (unlink of a MAPPED file did not return its blocks — pin/deferral leak)",
        ),
        (Some(_), _) => println(
            "ext2probe: SPACE LEAK (even unmapped unlink did not return its blocks)",
        ),
        _ => println("ext2probe: SPACE INCONCLUSIVE"),
    }
    exit(0);
}
