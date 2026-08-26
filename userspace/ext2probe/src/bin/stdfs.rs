//! `ext2probe-stdfs` — run the shared workload against the host OS filesystem
//! (`std::fs`), for a real-kernel reference point (a throwaway Docker container,
//! or a `mount -o sync` loop-mounted ext2 image for the fair "durable per op"
//! comparison against Akuma's write-through model).
//!
//!   cargo run -p ext2probe --bin ext2probe-stdfs \
//!     --no-default-features --features std-probe -- /tmp/ext2probe_root

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "/tmp/ext2probe_root".to_string());
    ext2probe::stdfs::run(&root);
}
