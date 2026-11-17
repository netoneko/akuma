//! `ext2put` — put host files into an ext2 image **copy**, without mounting it (Akuma has no loop
//! device), through the same host-tested writer as `devroot-ensure` (`akuma-ext2` via `HostFsOps`).
//!
//!   cargo build -p ext2probe --bin ext2put --no-default-features \
//!     --features host-probe --target x86_64-unknown-linux-musl --release
//!   ext2put IMG SRC:DST [SRC:DST ...]
//!
//! Each `SRC` (a host file) is written to `DST` (an absolute path in the image); missing parent
//! directories are created, an existing file is truncated in place (its mode survives), a new one gets
//! 0644 (the guest kernel execs any readable ELF). Every write is read back and its size checked. The
//! image is replaced atomically, so a failure leaves it as it was. A `*.orig` is refused, like
//! `devroot-ensure`. The whole image is held in RAM twice: mind the size.

use ext2probe::{host::HostFsOps, FsOps};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ext2put IMG SRC:DST [SRC:DST ...]");
        std::process::exit(2);
    }
    let path = &args[1];
    if path.ends_with(".orig") {
        eprintln!("ext2put: refusing to touch the .orig ({path})");
        std::process::exit(2);
    }
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let ops = HostFsOps::mount(bytes);
    for pair in &args[2..] {
        let (src, dst) = pair.split_once(':').unwrap_or_else(|| panic!("{pair}: want SRC:DST"));
        assert!(dst.starts_with('/'), "{dst}: DST must be absolute");
        let mut dir = String::new();
        let parts: Vec<&str> = dst.split('/').filter(|p| !p.is_empty()).collect();
        for p in &parts[..parts.len().saturating_sub(1)] {
            dir.push('/');
            dir.push_str(p);
            ops.mkdir(&dir);
        }
        let data = std::fs::read(src).unwrap_or_else(|e| panic!("read {src}: {e}"));
        ops.create_write(dst, &data);
        let got = ops.read_all(dst);
        assert_eq!(got, data.len(), "{dst} read-back size");
        println!("ext2put: {dst} = {got} bytes from {src} (read back OK)");
    }
    ops.sync();
    let image = ops.image_bytes();
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, &image).unwrap_or_else(|e| panic!("write {tmp}: {e}"));
    std::fs::rename(&tmp, path).unwrap_or_else(|e| panic!("rename {tmp}: {e}"));
    println!("ext2put: wrote {path} ({} bytes)", image.len());
}
