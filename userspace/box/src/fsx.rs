//! Small filesystem helpers shared by `commit` and `build` — thin, bounded-memory
//! wrappers over libakuma. Everything streams in 64 KiB chunks; nothing here
//! holds a whole file except [`read_file`], which is for config-sized inputs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{close, fstatat, open, open_flags, read_dir, read_fd, write_fd, Stat};
use sha2::{Digest, Sha256};

pub const CHUNK: usize = 64 * 1024;

pub const S_IFMT: u32 = 0o170_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;

/// `stat` that does not follow a final symlink.
pub fn lstat(path: &str) -> Result<Stat, i32> {
    fstatat(AT_FDCWD, path, AT_SYMLINK_NOFOLLOW)
}

pub fn stat(path: &str) -> Result<Stat, i32> {
    fstatat(AT_FDCWD, path, 0)
}

pub fn nlink(st: &Stat) -> u64 {
    st.st_nlink as u64
}

pub fn exists(path: &str) -> bool {
    lstat(path).is_ok()
}

pub fn read_file(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Some(out)
}

pub fn read_text(path: &str) -> Option<String> {
    String::from_utf8(read_file(path)?).ok()
}

/// Write all of `data`. `false` on a failed or short write.
pub fn write_all(fd: i32, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = write_fd(fd, data);
        if n <= 0 {
            return false;
        }
        data = &data[n as usize..];
    }
    true
}

pub fn write_file(path: &str, data: &[u8]) -> Result<(), String> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(format!("cannot create {path} (errno {})", -fd));
    }
    let ok = write_all(fd, data);
    close(fd);
    if ok {
        Ok(())
    } else {
        Err(format!("short write to {path}"))
    }
}

/// Stream `src` to `dst` (created/truncated). Returns bytes copied.
pub fn copy_file(src: &str, dst: &str) -> Result<u64, String> {
    let sfd = open(src, open_flags::O_RDONLY);
    if sfd < 0 {
        return Err(format!("cannot open {src} (errno {})", -sfd));
    }
    let dfd = open(dst, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if dfd < 0 {
        close(sfd);
        return Err(format!("cannot create {dst} (errno {})", -dfd));
    }
    let mut buf = alloc::vec![0u8; CHUNK];
    let mut total = 0u64;
    let mut result = Ok(());
    loop {
        let n = read_fd(sfd, &mut buf);
        if n < 0 {
            result = Err(format!("read error on {src}"));
            break;
        }
        if n == 0 {
            break;
        }
        if !write_all(dfd, &buf[..n as usize]) {
            result = Err(format!("short write to {dst}"));
            break;
        }
        total += n as u64;
    }
    close(sfd);
    close(dfd);
    result.map(|()| total)
}

fn hex(h: Sha256) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    for b in h.finalize() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// sha256 of a file's contents, hex.
pub fn sha256_file(path: &str) -> Option<String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return None;
    }
    let mut h = Sha256::new();
    let mut buf = alloc::vec![0u8; CHUNK];
    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        h.update(&buf[..n as usize]);
    }
    close(fd);
    Some(hex(h))
}

/// Names in a directory, sorted, without `.`/`..`.
pub fn list_sorted(path: &str) -> Vec<(String, bool)> {
    let mut names: Vec<(String, bool)> = read_dir(path)
        .map(|it| it.filter(|e| e.name != "." && e.name != "..").map(|e| (e.name, e.is_dir)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// Depth-first delete that never follows a symlink.
pub fn remove_tree(path: &str) {
    for (name, is_dir) in list_sorted(path) {
        let child = format!("{path}/{name}");
        if is_dir {
            remove_tree(&child);
        } else {
            libakuma::unlink(&child);
        }
    }
    libakuma::rmdir(path);
}

/// Wall-clock seconds since the epoch (0 if there is no RTC).
pub fn now_secs() -> u64 {
    libakuma::time() / 1_000_000
}

pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}
