//! `ext2probe` — shared workload core for the two ext2 probes.
//!
//! There are two ways to measure the same filesystem, and this crate holds both
//! behind one workload definition so they never drift:
//!
//! * **guest probe** (`src/main.rs`, the `ext2probe` binary): a `#![no_std]`
//!   libakuma ELF that runs *inside* the VM and drives the real kernel
//!   filesystem through syscalls, timing each op with `CLOCK_MONOTONIC`. It
//!   prints a before/after `%`-delta and a `REGRESSION` / `NO REGRESSION`
//!   verdict line for a boot-log grep. Answers "did guest wall-clock change?".
//!
//! * **host probe** (`src/bin/host.rs`, the `ext2probe-host` binary, feature
//!   `host-probe`): a `std` binary that links `akuma-ext2` directly and mounts
//!   it over an in-RAM copy of a real ext2 image, with a `BlockDevice` shim that
//!   counts every `read_bytes` / `write_bytes` and buckets it by on-disk region.
//!   Answers "how many device ops does each operation cost, and where?" — each
//!   `write_bytes` is one synchronous busy-polled virtqueue sector-RMW on the
//!   guest, so the counts convert straight back to the wall-clock story without
//!   a boot and without timing variance.
//!
//! * **std::fs probe** (`src/bin/stdfs.rs`, `ext2probe-stdfs`, feature
//!   `std-probe`): the same workload against the host OS filesystem, for a
//!   real-kernel reference point (a Docker container against ext2 mounted
//!   `-o sync` and default — see `crates/akuma-ext2/README.md`).
//!
//! All three call the [`workload`] helpers below through the [`FsOps`] trait, so
//! the shapes under test (300 × 4 KB files, a 2 MB sequential write, a
//! `dirs × files` tree, a flat directory) are defined exactly once.
//!
//! Build / run:
//! ```text
//! userspace/build.sh --ext2probe-only                      # guest ELF
//! cargo run -p ext2probe --bin ext2probe-host \
//!   --no-default-features --features host-probe \
//!   --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)" -- disk.img
//! ```

#![cfg_attr(not(any(feature = "host-probe", feature = "std-probe", test)), no_std)]

extern crate alloc;

use alloc::format;
use alloc::vec;

#[cfg(feature = "host-probe")]
pub mod host;

#[cfg(feature = "std-probe")]
pub mod stdfs;

/// The filesystem surface both probes drive. The guest implements it over
/// libakuma syscalls; the host implements it over `akuma_ext2::Ext2Filesystem`.
///
/// Every method is infallible from the caller's view — an implementation logs or
/// asserts on error as fits its context (the guest prints a warning and presses
/// on; the host `expect()`s, since a failure there is a probe bug). Return values
/// carry only the information the workload needs to keep going.
pub trait FsOps {
    /// `mkdir(path)`, treating "already exists" as success.
    fn mkdir(&self, path: &str);
    /// `rmdir(path)`.
    fn rmdir(&self, path: &str);
    /// Create/truncate `path` and write all of `data` (the `open(O_CREAT|
    /// O_WRONLY|O_TRUNC); write; close` cycle a `cp` or a compiler output does).
    fn create_write(&self, path: &str, data: &[u8]);
    /// Create/truncate `path` and write `total` bytes to it in `chunk`-byte
    /// writes, in **one** open/stream/close (not a reopen per chunk) — the shape
    /// a large file write actually has.
    fn seq_write(&self, path: &str, total: usize, chunk: usize);
    /// Read `path` start to end; return the byte count.
    fn read_all(&self, path: &str) -> usize;
    /// `unlink(path)`.
    fn unlink(&self, path: &str);
    /// List `path`; return the entry count.
    fn list_dir(&self, path: &str) -> usize;
    /// `stat(path)` — the metadata-only lookup a shell `test -e` / `ls -l` does.
    fn stat(&self, path: &str);
    /// Monotonic microseconds. The host returns 0 (it reports I/O counts, not
    /// time); the guest returns `CLOCK_MONOTONIC`.
    fn now_us(&self) -> u64;
}

/// Bytes written per small file in the create/delete workloads.
pub const FILE_SIZE: usize = 4096;
/// File count for the baseline create/delete pass.
pub const BASE_N: usize = 300;
/// Total bytes for the sequential write/read pass.
pub const SEQ_BYTES: usize = 2 * 1024 * 1024;
/// Chunk size for the sequential write/read pass.
pub const SEQ_CHUNK: usize = 8192;
/// Default `files-per-dir` for the tree stress pass.
pub const DEFAULT_TREE_FILES: usize = 200;
/// Default subdirectory count for the tree stress pass.
pub const DEFAULT_TREE_DIRS: usize = 16;

/// The workload helpers. Each takes `&dyn FsOps` and does filesystem work only —
/// no timing, no printing, no device-counter reads. The caller wraps each call
/// in whatever instrumentation it reports (the guest brackets it with
/// `now_us()`, the host with a device-counter snapshot).
pub mod workload {
    use super::{FsOps, format, vec};

    /// Create `n` files of `size` bytes named `00000.dat`.. directly under `dir`.
    pub fn create_files(ops: &dyn FsOps, dir: &str, n: usize, size: usize) {
        let buf = vec![0xABu8; size];
        for i in 0..n {
            ops.create_write(&format!("{dir}/{i:05}.dat"), &buf);
        }
    }

    /// Unlink the `n` files [`create_files`] made under `dir`.
    pub fn delete_files(ops: &dyn FsOps, dir: &str, n: usize) {
        for i in 0..n {
            ops.unlink(&format!("{dir}/{i:05}.dat"));
        }
    }

    /// Sequential `total`-byte write to `path` in `chunk`-byte writes.
    pub fn seq_write(ops: &dyn FsOps, path: &str, total: usize, chunk: usize) {
        ops.seq_write(path, total, chunk);
    }

    /// Build a `dirs`-subdirectory tree under `root`, `files_per_dir` files each.
    pub fn build_tree(ops: &dyn FsOps, root: &str, dirs: usize, files_per_dir: usize) {
        ops.mkdir(root);
        for d in 0..dirs {
            let sub = format!("{root}/d{d:04}");
            ops.mkdir(&sub);
            create_files(ops, &sub, files_per_dir, super::FILE_SIZE);
        }
    }

    /// Mass-delete the tree [`build_tree`] made — the `rm -rf` analogue.
    pub fn mass_delete_tree(ops: &dyn FsOps, root: &str, dirs: usize, files_per_dir: usize) {
        for d in 0..dirs {
            let sub = format!("{root}/d{d:04}");
            delete_files(ops, &sub, files_per_dir);
            ops.rmdir(&sub);
        }
        ops.rmdir(root);
    }

    /// Add `n` tiny files to a single flat directory. Exposes the O(directory
    /// size) per-entry cost of `add_dir_entry` — call it in windows and watch
    /// the per-file cost climb.
    pub fn flat_fill(ops: &dyn FsOps, dir: &str, start: usize, n: usize) {
        let buf = [0u8; 64];
        for i in start..start + n {
            ops.create_write(&format!("{dir}/f{i:06}"), &buf);
        }
    }
}
