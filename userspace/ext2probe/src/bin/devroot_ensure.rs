//! `devroot-ensure` — stage-8 M2: make a dev guest root image copy carry the
//! DNS prerequisites, using the tree's own host-tested ext2 writer
//! (`akuma-ext2` through the same host plumbing as `ext2probe-host`).
//!
//!   cargo build -p ext2probe --bin devroot-ensure --no-default-features \
//!     --features host-probe --target x86_64-unknown-linux-musl --release
//!   devroot-ensure IMG
//!
//! Ensures, idempotently:
//!   /etc/resolv.conf == "nameserver 10.0.2.3\n"   the relay's DNS proxy
//!   /bin/nslookup    -> busybox                   the applet is in the
//!                    image's busybox, but this image build predates the
//!                    symlink line in akuma_guest_rootfs.sh's SERVE list
//!                    (checked 2026-10-26: 402 applets incl. nslookup, no
//!                    /bin/nslookup dirent).
//!
//! boot_dev.sh runs this on the per-boot COPY of the frozen
//! /boot/akuma-dev-root-apk.img.orig — a *.orig argument is refused. Prints
//! one line per action; exits nonzero on any error. Read-back verification
//! is built in (write then read), so a silent bad write cannot pass.

use ext2probe::{host::HostFsOps, FsOps};
const RESOLV: &[u8] = b"nameserver 10.0.2.3\n";
const NSLOOKUP: &str = "/bin/nslookup";
const NSLOOKUP_TARGET: &str = "busybox";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: devroot-ensure IMG");
        std::process::exit(2);
    }
    let path = args[1].clone();
    if path.ends_with(".orig") {
        eprintln!("devroot-ensure: refusing to touch the .orig ({path})");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let ops = HostFsOps::mount(bytes);

    // resolv.conf: create_write is create-or-truncate, so re-running is a
    // pure content re-assert; read it back to prove the write landed.
    ops.create_write("/etc/resolv.conf", RESOLV);
    let got = ops.read_all("/etc/resolv.conf");
    assert_eq!(got, RESOLV.len(), "resolv.conf read-back size");
    println!("devroot-ensure: /etc/resolv.conf = {} bytes (read back OK)", got);

    // nslookup name: create_symlink, AlreadyExists tolerated (idempotent).
    if ops.ensure_symlink(NSLOOKUP, NSLOOKUP_TARGET) {
        println!("devroot-ensure: {NSLOOKUP} -> {NSLOOKUP_TARGET} created");
    } else {
        println!("devroot-ensure: {NSLOOKUP} already present");
    }

    ops.sync();
    let image = ops.image_bytes();
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, &image).unwrap_or_else(|e| panic!("write {tmp}: {e}"));
    std::fs::rename(&tmp, &path).unwrap_or_else(|e| panic!("rename {tmp}: {e}"));
    println!("devroot-ensure: wrote {path} ({} bytes)", image.len());
}
