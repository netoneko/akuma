//! Extraction: the parts that touch the filesystem.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{
    chmod, close, eprintln, mkdir_p, open, open_flags, print, print_dec, println, read_fd, symlink,
    write_fd,
};
use miniz_oxide::inflate;

use crate::format::{
    BLOCK_SIZE, Entry, EntryKind, is_path_safe, join_path, parse_header, relative_symlink_target,
    strip_gzip_header,
};

#[derive(Debug)]
pub enum TarError {
    Io(i32, String),
    Gzip(&'static str),
    /// The decompressed archive exceeds [`ExtractOptions::max_bytes`].
    TooLarge(usize),
}

impl TarError {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Io(errno, path) => format!("errno {errno} for '{path}'"),
            Self::Gzip(msg) => format!("gzip: {msg}"),
            Self::TooLarge(limit) => format!("archive exceeds {limit} bytes uncompressed"),
        }
    }
}

pub struct ExtractOptions {
    pub gzip: bool,
    pub verbose: bool,
    /// Ceiling on the decompressed archive for the gzip path, which must hold
    /// the whole thing in memory. In-process that memory is the caller's, so a
    /// hostile or simply enormous layer has to be refused rather than discovered
    /// by running out. 0 disables the check.
    pub max_bytes: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self { gzip: false, verbose: false, max_bytes: 512 * 1024 * 1024 }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub entries: usize,
    /// OCI whiteout markers seen (`.wh.*`). Left on disk for the overlay to
    /// interpret; counted so a caller can tell a layer carries deletions.
    pub whiteouts: usize,
    /// Entries refused for pointing outside the extraction directory.
    pub rejected: usize,
}

/// Extract `archive_path` into `target_dir`.
///
/// # Errors
/// I/O failures on the archive itself, and gzip framing/decompression errors.
/// A failure on an individual entry is reported and skipped, not fatal — a
/// half-extracted layer is caught by the caller's staging directory, not here.
pub fn extract_file(
    archive_path: &str,
    target_dir: &str,
    opts: &ExtractOptions,
) -> Result<Stats, TarError> {
    if opts.gzip {
        extract_gzip(archive_path, target_dir, opts)
    } else {
        extract_stream(archive_path, target_dir, opts)
    }
}

/// Streaming extraction: 512-byte headers and file data straight off the fd,
/// never holding the archive in memory.
fn extract_stream(
    archive_path: &str,
    target_dir: &str,
    opts: &ExtractOptions,
) -> Result<Stats, TarError> {
    let fd = open(archive_path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::Io(fd, String::from(archive_path)));
    }

    let mut stats = Stats::default();
    let mut header = [0u8; BLOCK_SIZE];
    let mut zero_blocks = 0;

    loop {
        if !read_exact(fd, &mut header) {
            break;
        }

        let Some(entry) = parse_header(&header) else {
            if header.iter().all(|&b| b == 0) {
                zero_blocks += 1;
                if zero_blocks >= 2 {
                    break;
                }
                continue;
            }
            eprintln("tar: warning: bad header checksum, stopping");
            break;
        };
        zero_blocks = 0;

        if entry.kind == EntryKind::Metadata || entry.path.is_empty() {
            read_skip(fd, entry.payload_span());
            continue;
        }
        if !is_path_safe(&entry.path) {
            eprintln(&format!("tar: refusing entry outside target: {}", entry.path));
            stats.rejected += 1;
            read_skip(fd, entry.payload_span());
            continue;
        }

        count(&mut stats, &entry);
        let full_path = join_path(target_dir, &entry.path);

        if entry.kind == EntryKind::File {
            let written = write_entry_streaming(fd, &full_path, entry.size, opts.verbose);
            chmod_entry(&full_path, &entry);
            let padding = crate::format::padded_size(entry.size) - written;
            if padding > 0 {
                read_skip(fd, padding);
            }
        } else {
            create_nonfile(&full_path, &entry, opts.verbose);
            read_skip(fd, entry.payload_span());
        }
    }

    close(fd);
    report(&stats, opts.verbose);
    Ok(stats)
}

/// Gzip extraction. DEFLATE has no random access, so the archive is
/// decompressed whole before entries are walked.
fn extract_gzip(
    archive_path: &str,
    target_dir: &str,
    opts: &ExtractOptions,
) -> Result<Stats, TarError> {
    let raw = read_file_to_vec(archive_path)?;
    if opts.verbose {
        print("tar: read ");
        print_dec(raw.len());
        println(" bytes (compressed)");
    }

    let deflate = strip_gzip_header(&raw).ok_or(TarError::Gzip("invalid gzip header"))?;
    let data = inflate::decompress_to_vec(deflate).map_err(|_| TarError::Gzip("decompression failed"))?;
    drop(raw);

    if opts.max_bytes != 0 && data.len() > opts.max_bytes {
        return Err(TarError::TooLarge(opts.max_bytes));
    }
    if opts.verbose {
        print("tar: decompressed to ");
        print_dec(data.len());
        println(" bytes");
    }

    let mut stats = Stats::default();
    let mut pos: usize = 0;
    let mut zero_blocks = 0;

    while pos + BLOCK_SIZE <= data.len() {
        let mut header = [0u8; BLOCK_SIZE];
        header.copy_from_slice(&data[pos..pos + BLOCK_SIZE]);
        pos += BLOCK_SIZE;

        let Some(entry) = parse_header(&header) else {
            if header.iter().all(|&b| b == 0) {
                zero_blocks += 1;
                if zero_blocks >= 2 {
                    break;
                }
                continue;
            }
            if opts.verbose {
                eprintln("tar: warning: bad header checksum, stopping");
            }
            break;
        };
        zero_blocks = 0;

        if entry.kind == EntryKind::Metadata || entry.path.is_empty() {
            pos += entry.payload_span();
            continue;
        }
        if !is_path_safe(&entry.path) {
            eprintln(&format!("tar: refusing entry outside target: {}", entry.path));
            stats.rejected += 1;
            pos += entry.payload_span();
            continue;
        }

        count(&mut stats, &entry);
        let full_path = join_path(target_dir, &entry.path);

        if entry.kind == EntryKind::File {
            let end = pos + entry.size;
            if end > data.len() {
                eprintln(&format!("tar: error: truncated entry {}", entry.path));
                break;
            }
            write_entry(&full_path, &data[pos..end], &entry, opts.verbose);
        } else {
            create_nonfile(&full_path, &entry, opts.verbose);
        }

        pos += entry.payload_span();
    }

    report(&stats, opts.verbose);
    Ok(stats)
}

fn count(stats: &mut Stats, entry: &Entry) {
    stats.entries += 1;
    let name = entry.path.rsplit('/').next().unwrap_or("");
    if name.starts_with(".wh.") {
        stats.whiteouts += 1;
    }
}

fn report(stats: &Stats, verbose: bool) {
    if verbose || stats.rejected > 0 {
        print("tar: extracted ");
        print_dec(stats.entries);
        print(" entries");
        if stats.rejected > 0 {
            print(", refused ");
            print_dec(stats.rejected);
        }
        println("");
    }
}

/// Directory, symlink or hardlink. A hardlink becomes a relative symlink: the
/// kernel's `linkat` copies the whole file, and an image layer routinely has
/// hundreds of names for one binary.
fn create_nonfile(full_path: &str, entry: &Entry, verbose: bool) {
    match entry.kind {
        EntryKind::Directory => {
            if verbose {
                print("d ");
                println(&entry.path);
            }
            if !mkdir_p(full_path) && verbose {
                eprintln(&format!("tar: warning: failed to create directory {full_path}"));
            }
        }
        EntryKind::Symlink => {
            if verbose {
                print("l ");
                print(&entry.path);
                print(" -> ");
                println(&entry.linkname);
            }
            ensure_parent(full_path);
            let ret = symlink(&entry.linkname, full_path);
            if ret < 0 && verbose {
                eprintln(&format!("tar: warning: symlink {full_path}: errno {}", -ret));
            }
        }
        EntryKind::HardLink => {
            let target = crate::format::normalize_entry_path(&entry.linkname);
            let rel = relative_symlink_target(&entry.path, &target);
            if verbose {
                print("h ");
                print(&entry.path);
                print(" -> ");
                println(&rel);
            }
            ensure_parent(full_path);
            let ret = symlink(&rel, full_path);
            if ret < 0 && verbose {
                eprintln(&format!("tar: warning: hardlink {full_path}: errno {}", -ret));
            }
        }
        EntryKind::File | EntryKind::Metadata => {}
    }
}

fn write_entry(full_path: &str, data: &[u8], entry: &Entry, verbose: bool) {
    if verbose {
        print("x ");
        print(&entry.path);
        print(" (");
        print_dec(entry.size);
        println(" bytes)");
    }
    ensure_parent(full_path);

    let fd = open(full_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        eprintln(&format!("tar: error: failed to create {full_path}: errno {}", -fd));
        return;
    }
    if !data.is_empty() {
        write_fd(fd, data);
    }
    close(fd);
    chmod_entry(full_path, entry);
}

/// Copy `size` bytes from `fd` into a new file. Returns the bytes consumed from
/// the archive, so the caller can skip the right amount of padding even if the
/// read stopped early.
fn write_entry_streaming(fd: i32, full_path: &str, size: usize, verbose: bool) -> usize {
    if verbose {
        print("x ");
        print(full_path);
        print(" (");
        print_dec(size);
        println(" bytes)");
    }
    ensure_parent(full_path);

    let out = open(full_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if out < 0 {
        eprintln(&format!("tar: error: failed to create {full_path}: errno {}", -out));
        read_skip(fd, size);
        return size;
    }

    let mut remaining = size;
    let mut buf = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let n = read_fd(fd, &mut buf[..want]);
        if n <= 0 {
            break;
        }
        let n = n as usize;
        if write_fd(out, &buf[..n]) < 0 {
            eprintln(&format!("tar: error: write failed for {full_path}"));
            break;
        }
        remaining -= n;
    }
    close(out);
    size - remaining
}

/// Apply the archived permission bits.
///
/// Skipping this is invisible until something checks: a shell's `PATH` search
/// calls `access(X_OK)` and refuses a 0644 binary with "Permission denied",
/// which is exactly how an extracted image fails to run its own commands.
fn chmod_entry(full_path: &str, entry: &Entry) {
    if entry.mode != 0 {
        chmod(full_path, entry.mode);
    }
}

fn ensure_parent(full_path: &str) {
    if let Some(slash) = full_path.rfind('/') {
        let parent = &full_path[..slash];
        if !parent.is_empty() {
            mkdir_p(parent);
        }
    }
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, TarError> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::Io(fd, String::from(path)));
    }

    let mut buffer = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut tmp);
        if n < 0 {
            close(fd);
            return Err(TarError::Io(n as i32, String::from(path)));
        }
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&tmp[..n as usize]);
    }
    close(fd);
    Ok(buffer)
}

/// Read exactly `buf.len()` bytes. False on EOF or error.
fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut offset = 0;
    while offset < buf.len() {
        let n = read_fd(fd, &mut buf[offset..]);
        if n <= 0 {
            return false;
        }
        offset += n as usize;
    }
    true
}

/// Skip `n` bytes by reading and discarding — no lseek dependency.
fn read_skip(fd: i32, mut n: usize) {
    let mut buf = [0u8; 4096];
    while n > 0 {
        let want = n.min(buf.len());
        let got = read_fd(fd, &mut buf[..want]);
        if got <= 0 {
            break;
        }
        n -= got as usize;
    }
}
