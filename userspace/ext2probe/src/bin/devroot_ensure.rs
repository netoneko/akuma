//! `devroot-ensure` — stage-8 M2/M3: make a dev guest root image copy carry
//! the DNS + apk-repository prerequisites, using the tree's own host-tested ext2 writer
//! (`akuma-ext2` through the same host plumbing as `ext2probe-host`).
//!
//!   cargo build -p ext2probe --bin devroot-ensure --no-default-features \
//!     --features host-probe --target x86_64-unknown-linux-musl --release
//!   devroot-ensure IMG
//!
//! Ensures, idempotently:
//!   /etc/resolv.conf == "nameserver 10.0.2.3\n"   the relay's DNS proxy
//!   /etc/apk/repositories == "http://192.168.1.159:8123/main\n"  the frozen
//!                    LAN mirror (stage-8 M3); apk-tools 3 appends the
//!                    architecture itself, so the wire request is
//!                    /main/x86_64/APKINDEX.tar.gz. create_write truncates
//!                    away the .orig's offline-staging comment line.
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
const REPOSITORIES: &[u8] = b"http://192.168.1.159:8123/main\n";
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

    // repositories (stage-8 M3): same create-or-truncate + read-back pattern.
    ops.create_write("/etc/apk/repositories", REPOSITORIES);
    let got = ops.read_all("/etc/apk/repositories");
    assert_eq!(got, REPOSITORIES.len(), "repositories read-back size");
    println!("devroot-ensure: /etc/apk/repositories = {} bytes (read back OK)", got);

    // nslookup name: create_symlink, AlreadyExists tolerated (idempotent).
    if ops.ensure_symlink(NSLOOKUP, NSLOOKUP_TARGET) {
        println!("devroot-ensure: {NSLOOKUP} -> {NSLOOKUP_TARGET} created");
    } else {
        println!("devroot-ensure: {NSLOOKUP} already present");
    }

    // [tmp] wedge-Q7 (remove with the [tmp] set): opt-in bake of an
    // instrumented /bin/sshd (RESULT-Q6 §4 item 1: bake, don't swap). When
    // AKUMA_DEVROOT_BAKE_SSHD points at a built sshd (the one carrying the
    // bridge iters/reaped prints), overwrite the image's /bin/sshd with it so
    // the instrument serves from boot one — no wget/killall/respawn question.
    // The file already exists in the image (755), so create_write truncates in
    // place and the mode survives; read-back assert proves the landing.
    if let Ok(src) = std::env::var("AKUMA_DEVROOT_BAKE_SSHD") {
        let sshd = std::fs::read(&src).unwrap_or_else(|e| panic!("read {src}: {e}"));
        ops.create_write("/bin/sshd", &sshd);
        let got = ops.read_all("/bin/sshd");
        assert_eq!(got, sshd.len(), "/bin/sshd read-back size");
        println!("devroot-ensure: baked /bin/sshd = {} bytes from {src} (read back OK)", got);
    }

    ops.sync();
    let image = ops.image_bytes();
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, &image).unwrap_or_else(|e| panic!("write {tmp}: {e}"));
    std::fs::rename(&tmp, &path).unwrap_or_else(|e| panic!("rename {tmp}: {e}"));
    println!("devroot-ensure: wrote {path} ({} bytes)", image.len());
}
