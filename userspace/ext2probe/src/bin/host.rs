//! `ext2probe-host` — the host-side device-I/O probe. Links `akuma-ext2`,
//! mounts it over an in-RAM copy of a real ext2 image, drives the shared
//! [`ext2probe`] workloads, and prints device `read_bytes` / `write_bytes` call
//! counts bucketed by on-disk region.
//!
//!   cargo run -p ext2probe --bin ext2probe-host \
//!     --no-default-features --features host-probe \
//!     --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)" -- [ext2-image]
//!
//! Default image: `disk.img` at the repo root. It is copied into RAM; the file
//! on disk is not modified. Full analysis: `crates/akuma-ext2/README.md`.

use ext2probe::host::{HostFsOps, Snap, REGIONS};
use ext2probe::{workload, FsOps, BASE_N, FILE_SIZE, SEQ_BYTES, SEQ_CHUNK};

fn report(title: &str, d: Snap) {
    println!("{title}");
    println!(
        "     rd={:>7} calls ({:>7} KB)   wr={:>7} calls ({:>7} KB)",
        d.r_calls,
        d.r_bytes / 1024,
        d.w_calls,
        d.w_bytes / 1024
    );
    let cols = |v: &[u64; 5]| {
        REGIONS
            .iter()
            .zip(v)
            .map(|(n, c)| format!("{n}={c}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("     writes:  {}", cols(&d.w));
    println!("     reads:   {}", cols(&d.r));
}

fn main() {
    let img_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| repo_root().join("disk.img").to_string_lossy().into_owned());
    let bytes = std::fs::read(&img_path)
        .unwrap_or_else(|e| panic!("read {img_path}: {e}\n(pass an ext2 image, or run scripts/create_disk.sh)"));
    let img_mb = bytes.len() / 1024 / 1024;

    let ops = HostFsOps::mount(bytes);
    println!(
        "image: {}  ({} MB, block_size={}, {} groups, {} inodes/group)\n",
        img_path, img_mb, ops.geom.block_size, ops.geom.group_count, ops.geom.inodes_per_group,
    );
    let snap = || ops.counters.snapshot();
    let root = format!("/ioprobe_{}", std::process::id());
    ops.mkdir(&root);

    // [1] create BASE_N x 4 KB files
    ops.mkdir(&format!("{root}/s1"));
    let a = snap();
    workload::create_files(&ops, &format!("{root}/s1"), BASE_N, FILE_SIZE);
    let d = snap() - a;
    report(&format!("[1] create {BASE_N} x 4KB files:"), d);
    println!("     => {:.1} device writes / file\n", d.w_calls as f64 / BASE_N as f64);

    // Content check — the zero-fill skip (Fix A) must not corrupt written data.
    ops.verify(&format!("{root}/s1/00000.dat"), 0xAB, FILE_SIZE);
    ops.verify(&format!("{root}/s1/00299.dat"), 0xAB, FILE_SIZE);

    // [2] sequential 2 MB write
    let a = snap();
    workload::seq_write(&ops, &format!("{root}/s1/big.dat"), SEQ_BYTES, SEQ_CHUNK);
    let d = snap() - a;
    report("[2] sequential 2 MB write (8KB write_at):", d);
    println!(
        "     => {:.1}x write amplification ({} KB to device for {} KB)\n",
        d.w_bytes as f64 / SEQ_BYTES as f64,
        d.w_bytes / 1024,
        SEQ_BYTES / 1024,
    );

    // [3] read the 2 MB back (right after writing it)
    let a = snap();
    let n = ops.read_all(&format!("{root}/s1/big.dat"));
    let d = snap() - a;
    report(&format!("[3] read {} KB back (immediately after write):", n / 1024), d);
    println!();

    // [4] delete the BASE_N files
    let a = snap();
    workload::delete_files(&ops, &format!("{root}/s1"), BASE_N);
    let d = snap() - a;
    report(&format!("[4] delete {BASE_N} x 4KB files:"), d);
    println!("     => {:.1} device writes / file\n", d.w_calls as f64 / BASE_N as f64);

    // [5] flat directory O(n^2): cost of file #N in one big dir
    ops.mkdir(&format!("{root}/flat"));
    println!("[5] add 2000 files to ONE flat directory, windows of 200:");
    for base in (0..2000).step_by(200) {
        let a = snap();
        workload::flat_fill(&ops, &format!("{root}/flat"), base, 200);
        let d = snap() - a;
        println!(
            "     files {:>4}..{:<4}: wr={:>5} ({:>5} KB)  data-writes={:>5}  => {:.1} wr/file",
            base,
            base + 200,
            d.w_calls,
            d.w_bytes / 1024,
            d.w[4],
            d.w_calls as f64 / 200.0
        );
    }
    println!();

    // [6] tree build + mass delete
    let a = snap();
    workload::build_tree(&ops, &format!("{root}/tree"), 16, 200);
    let d = snap() - a;
    report("[6a] build 16 x 200 tree (3200 files):", d);
    println!("     => {:.1} device writes / file", d.w_calls as f64 / 3200.0);
    let a = snap();
    workload::mass_delete_tree(&ops, &format!("{root}/tree"), 16, 200);
    let d = snap() - a;
    report("[6b] mass-delete the 3200-file tree:", d);
    println!("     => {:.1} device writes / file\n", d.w_calls as f64 / 3200.0);

    // [7] deep-path stat cost (warm)
    let deep = format!("{root}/a/b/c/d/e");
    for p in ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e"] {
        ops.mkdir(&format!("{root}/{p}"));
    }
    ops.create_write(&format!("{deep}/leaf"), b"hi");
    let a = snap();
    for _ in 0..100 {
        ops.stat(&format!("{deep}/leaf"));
    }
    let d = snap() - a;
    report("[7] 100 x stat() on a 7-component path (warm):", d);
    println!("     => {:.2} device reads / lookup", d.r_calls as f64 / 100.0);

    // [8] flush cost
    let a = snap();
    ops.sync();
    let d = snap() - a;
    report("[8] sync():", d);
}

fn repo_root() -> std::path::PathBuf {
    // this file: <root>/userspace/ext2probe/src/bin/host.rs
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}
