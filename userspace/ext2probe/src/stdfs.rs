//! `FsOps` over the host OS filesystem (`std::fs`), for a reference-point run of
//! the shared workload against a real kernel's ext2/ext4 — e.g. in a throwaway
//! Docker container. Compiled only with the `std-probe` feature.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

use crate::FsOps;

pub struct StdFsOps {
    start: Instant,
}

impl Default for StdFsOps {
    fn default() -> Self {
        Self { start: Instant::now() }
    }
}

impl FsOps for StdFsOps {
    fn mkdir(&self, path: &str) {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("mkdir({path}): {e}"),
        }
    }
    fn rmdir(&self, path: &str) {
        fs::remove_dir(path).unwrap_or_else(|e| panic!("rmdir({path}): {e}"));
    }
    fn create_write(&self, path: &str, data: &[u8]) {
        let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create({path}): {e}"));
        f.write_all(data).unwrap_or_else(|e| panic!("write({path}): {e}"));
    }
    fn seq_write(&self, path: &str, total: usize, chunk: usize) {
        let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create({path}): {e}"));
        let buf = vec![0xCDu8; chunk];
        let mut w = 0;
        while w < total {
            let n = core::cmp::min(chunk, total - w);
            f.write_all(&buf[..n]).unwrap_or_else(|e| panic!("write({path}): {e}"));
            w += n;
        }
    }
    fn read_all(&self, path: &str) -> usize {
        let mut f = fs::File::open(path).unwrap_or_else(|e| panic!("open({path}): {e}"));
        let mut buf = vec![0u8; 8192];
        let mut total = 0;
        loop {
            let n = f.read(&mut buf).unwrap_or_else(|e| panic!("read({path}): {e}"));
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    }
    fn unlink(&self, path: &str) {
        fs::remove_file(path).unwrap_or_else(|e| panic!("unlink({path}): {e}"));
    }
    fn list_dir(&self, path: &str) -> usize {
        fs::read_dir(path).map(|it| it.count()).unwrap_or(0)
    }
    fn stat(&self, path: &str) {
        fs::metadata(path).unwrap_or_else(|e| panic!("stat({path}): {e}"));
    }
    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// Run the shared baseline + stress workload against `root` and print timings in
/// the same shape as the guest `ext2probe`.
pub fn run(root: &str) {
    use crate::{workload, BASE_N, DEFAULT_TREE_DIRS, DEFAULT_TREE_FILES, FILE_SIZE, SEQ_BYTES, SEQ_CHUNK};

    let ops = StdFsOps::default();
    let _ = fs::remove_dir_all(root);
    ops.mkdir(root);
    println!("ext2probe-stdfs: root={root}");

    for (tag, sub) in [("BEFORE", "b1"), ("AFTER", "b2")] {
        if tag == "AFTER" {
            // churn between the passes, same as the guest probe
            let troot = format!("{root}/stress");
            let t0 = ops.now_us();
            workload::build_tree(&ops, &troot, DEFAULT_TREE_DIRS, DEFAULT_TREE_FILES);
            println!("ext2probe-stdfs: build tree:  {} us", ops.now_us() - t0);
            let t0 = ops.now_us();
            workload::mass_delete_tree(&ops, &troot, DEFAULT_TREE_DIRS, DEFAULT_TREE_FILES);
            let md = ops.now_us() - t0;
            println!("ext2probe-stdfs: mass delete: {md} us");
            println!(
                "ext2probe-stdfs: mass delete rate: {} files/sec",
                (DEFAULT_TREE_DIRS * DEFAULT_TREE_FILES) as u64 * 1_000_000 / md.max(1)
            );
        }
        let dir = format!("{root}/{sub}");
        ops.mkdir(&dir);
        println!("\next2probe-stdfs: --- {tag} pass ---");

        let t0 = ops.now_us();
        workload::create_files(&ops, &dir, BASE_N, FILE_SIZE);
        println!("ext2probe-stdfs: create:    {} us", ops.now_us() - t0);

        let big = format!("{dir}/big.dat");
        let t0 = ops.now_us();
        workload::seq_write(&ops, &big, SEQ_BYTES, SEQ_CHUNK);
        println!("ext2probe-stdfs: seq_write: {} us", ops.now_us() - t0);

        let t0 = ops.now_us();
        let n = ops.read_all(&big);
        println!("ext2probe-stdfs: seq_read:  {} us ({} bytes)", ops.now_us() - t0, n);

        let t0 = ops.now_us();
        let listed = ops.list_dir(&dir);
        println!("ext2probe-stdfs: list_dir:  {} us ({listed} entries)", ops.now_us() - t0);

        ops.unlink(&big);
        let t0 = ops.now_us();
        workload::delete_files(&ops, &dir, BASE_N);
        println!("ext2probe-stdfs: delete:    {} us", ops.now_us() - t0);
    }
    let _ = fs::remove_dir_all(root);
    if !Path::new(root).exists() {
        println!("\next2probe-stdfs: done");
    }
}
