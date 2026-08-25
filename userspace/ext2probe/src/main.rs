//! `ext2probe` — file create/write/read/list throughput benchmark, run once
//! on a clean directory and again after a large create-then-mass-delete
//! churn, to test the `docs/archive/EXT2_PERFORMANCE_AUDIT.md` theory that
//! ordinary ext2 file operations measurably regress after a bulk `rm -rf`
//! (free-block/inode bitmap fragmentation, the deferred-inode-free list,
//! file-page-cache pinning — see that doc for the full writeup and which of
//! these this probe can and can't distinguish).
//!
//! Usage: `ext2probe [stress_files_per_dir] [stress_dirs]`
//!   Defaults: 200 files/dir, 16 dirs (3200 files, ~12.5 MB) for the stress
//!   phase — sized to approximate a mid-size build-cache tree like the
//!   256-subdirectory `/.cache/go-build` tree in
//!   `docs/archive/GETDENTS64_DIR_CACHE_FIX.md`.
//!
//! Verdict line for a log grep: `ext2probe: REGRESSION` / `ext2probe: NO REGRESSION`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec;

use libakuma::open_flags::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use libakuma::{
    clock_gettime, close, exit, mkdir, open, print, print_dec, println, read, read_dir, rmdir,
    unlink, write, Timespec, CLOCK_MONOTONIC,
};

const FILE_SIZE: usize = 4096;
const BASE_N: usize = 300;
const SEQ_BYTES: usize = 2 * 1024 * 1024;
const SEQ_CHUNK: usize = 8192;
const DEFAULT_STRESS_FILES: usize = 200;
const DEFAULT_STRESS_DIRS: usize = 16;

/// A degradation this large on any single op after the mass delete is called
/// out as a regression in the final verdict line.
const REGRESSION_PCT: i64 = 20;

/// Linux/musl `EEXIST` — not exported by libakuma, hardcoded per the ABI docs
/// (`docs/reference/abi/`) since the kernel's syscall ABI mirrors Linux's.
const EEXIST: i32 = 17;

fn now_us() -> u64 {
    let mut ts = Timespec::default();
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1000
}

fn print_us(label: &str, us: u64) {
    print(label);
    print_dec(us as usize);
    println(" us");
}

fn mkdir_ignore_exists(path: &str) {
    let r = mkdir(path);
    if r != 0 && r != -EEXIST {
        print("ext2probe: WARNING mkdir(");
        print(path);
        print(") rc=");
        print_dec((-r) as usize);
        println("");
    }
}

/// Create `n` files of `size` bytes each directly under `dir`, named
/// `00000.dat`.. Returns elapsed microseconds.
fn create_files(dir: &str, n: usize, size: usize) -> u64 {
    let buf = vec![0xABu8; size];
    let t0 = now_us();
    for i in 0..n {
        let path = format!("{}/{:05}.dat", dir, i);
        let fd = open(&path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd < 0 {
            print("ext2probe: WARNING open failed for ");
            println(&path);
            continue;
        }
        let mut off = 0;
        while off < buf.len() {
            let w = write(fd, &buf[off..]);
            if w <= 0 {
                break;
            }
            off += w as usize;
        }
        close(fd);
    }
    now_us() - t0
}

/// Unlink the `n` files `create_files` made under `dir`, then rmdir `dir`
/// itself. Returns elapsed microseconds — the `rm -rf` analogue this whole
/// probe exists to time.
fn delete_files(dir: &str, n: usize) -> u64 {
    let t0 = now_us();
    for i in 0..n {
        let path = format!("{}/{:05}.dat", dir, i);
        unlink(&path);
    }
    rmdir(dir);
    now_us() - t0
}

fn seq_write(path: &str, total: usize, chunk: usize) -> u64 {
    let buf = vec![0xCDu8; chunk];
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC);
    if fd < 0 {
        println("ext2probe: WARNING seq_write open failed");
        return 0;
    }
    let t0 = now_us();
    let mut written = 0;
    while written < total {
        let n = write(fd, &buf);
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
    let elapsed = now_us() - t0;
    close(fd);
    elapsed
}

fn seq_read(path: &str, chunk: usize) -> (u64, usize) {
    let mut buf = vec![0u8; chunk];
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        println("ext2probe: WARNING seq_read open failed");
        return (0, 0);
    }
    let t0 = now_us();
    let mut total = 0;
    loop {
        let n = read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    let elapsed = now_us() - t0;
    close(fd);
    (elapsed, total)
}

fn list_dir(dir: &str) -> (u64, usize) {
    let t0 = now_us();
    let count = read_dir(dir).map(Iterator::count).unwrap_or(0);
    (now_us() - t0, count)
}

/// One (create N files, seq write+read a big file, list, delete) pass.
/// Returns (create_us, write_us, read_us, list_us).
fn baseline_pass(tag: &str, dir: &str) -> (u64, u64, u64, u64) {
    println("");
    print("ext2probe: --- ");
    print(tag);
    println(" pass ---");
    mkdir_ignore_exists(dir);

    let create_us = create_files(dir, BASE_N, FILE_SIZE);
    print_us("ext2probe: create:    ", create_us);

    let big = format!("{}/big.dat", dir);
    let write_us = seq_write(&big, SEQ_BYTES, SEQ_CHUNK);
    print_us("ext2probe: seq_write: ", write_us);

    let (read_us, read_bytes) = seq_read(&big, SEQ_CHUNK);
    print_us("ext2probe: seq_read:  ", read_us);
    if read_bytes != SEQ_BYTES {
        print("ext2probe: WARNING seq_read got ");
        print_dec(read_bytes);
        print(" bytes, expected ");
        print_dec(SEQ_BYTES);
        println("");
    }

    let (list_us, listed) = list_dir(dir);
    print_us("ext2probe: list_dir:  ", list_us);
    print("ext2probe:   (");
    print_dec(listed);
    println(" entries)");

    unlink(&big);
    let delete_us = delete_files(dir, BASE_N);
    print_us("ext2probe: delete:    ", delete_us);

    (create_us, write_us, read_us, list_us)
}

/// Build a `dirs`-subdirectory, `files_per_dir`-file-each tree (matching the
/// 256-subdirectory `go-build`-cache shape from
/// `docs/archive/GETDENTS64_DIR_CACHE_FIX.md`) and mass-delete it in one
/// timed pass — the `rm -rf /tmp/akuma`-shaped operation this probe exists to
/// reproduce. Returns (delete_us, total_files).
fn stress_pass(root: &str, dirs: usize, files_per_dir: usize) -> (u64, usize) {
    println("");
    println("ext2probe: --- stress pass (simulated `rm -rf` of a big tree) ---");
    print("ext2probe: building ");
    print_dec(dirs);
    print(" dirs x ");
    print_dec(files_per_dir);
    println(" files");

    mkdir_ignore_exists(root);
    let t0 = now_us();
    for d in 0..dirs {
        let sub = format!("{}/d{:04}", root, d);
        mkdir_ignore_exists(&sub);
        create_files(&sub, files_per_dir, FILE_SIZE);
    }
    print_us("ext2probe: build tree:  ", now_us() - t0);

    let t0 = now_us();
    for d in 0..dirs {
        let sub = format!("{}/d{:04}", root, d);
        for i in 0..files_per_dir {
            let path = format!("{}/{:05}.dat", sub, i);
            unlink(&path);
        }
        rmdir(&sub);
    }
    rmdir(root);
    let delete_us = now_us() - t0;
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

#[no_mangle]
pub extern "C" fn main() {
    let stress_files: usize = libakuma::arg(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_STRESS_FILES);
    let stress_dirs: usize = libakuma::arg(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_STRESS_DIRS);

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

    mkdir_ignore_exists("/probe");

    let (c1, w1, r1, l1) = baseline_pass("BEFORE", "/probe/base1");

    let (mass_delete_us, total_files) = stress_pass("/probe/stress", stress_dirs, stress_files);
    print("ext2probe: mass delete rate: ");
    if mass_delete_us > 0 {
        print_dec((total_files as u64 * 1_000_000 / mass_delete_us) as usize);
    } else {
        print_dec(0);
    }
    println(" files/sec");

    let (c2, w2, r2, l2) = baseline_pass("AFTER", "/probe/base2");

    println("");
    println("ext2probe: --- comparison (before vs after mass delete) ---");
    let mut regressed = false;
    regressed |= print_delta("create   ", c1, c2);
    regressed |= print_delta("seq_write", w1, w2);
    regressed |= print_delta("seq_read ", r1, r2);
    regressed |= print_delta("list_dir ", l1, l2);

    rmdir("/probe");

    if regressed {
        println("ext2probe: REGRESSION (>=20% slower on at least one op after the mass delete)");
    } else {
        println("ext2probe: NO REGRESSION");
    }
    exit(0);
}
