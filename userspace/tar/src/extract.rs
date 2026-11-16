//! Extraction: the parts that touch the filesystem.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use alloc::collections::BTreeSet;

use libakuma::{
    chmod, close, eprintln, fstatat, lchown, link, mkdir_p, open, open_flags, print, print_dec, println, read_fd,
    readlink, symlink, unlink, write_fd,
};
use miniz_oxide::inflate;

use crate::format::{
    BLOCK_SIZE, Entry, EntryKind, PathState, is_path_safe, join_path, parse_header, relative_symlink_target,
    resolve_in_root, strip_gzip_header,
};

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;

/// `lstat`'s file type bits, `None` if nothing is there.
fn file_type(path: &str) -> Option<u32> {
    fstatat(AT_FDCWD, path, AT_SYMLINK_NOFOLLOW).ok().map(|st| st.st_mode & S_IFMT)
}

/// The extraction directory, and the directories under it already seen to be
/// real directories. Every entry is placed through it, so no write follows a
/// symlink out of the tree (`crate::format::resolve_in_root`).
///
/// The cache is sound because extraction never turns a directory into anything
/// else: it unlinks only symlinks, and `symlink()` over a directory fails. It
/// keeps the check to one `lstat` per new directory rather than one per
/// component per entry.
struct Root {
    dir: String,
    known_dirs: BTreeSet<String>,
}

impl Root {
    fn new(dir: &str) -> Self {
        Self { dir: String::from(dir), known_dirs: BTreeSet::new() }
    }

    /// Where root-relative `rel` really is, with its directories resolved
    /// inside the root. `None`: a symlink loop or an unreadable link.
    fn resolve(&mut self, rel: &str) -> Option<String> {
        let dir = &self.dir;
        let known = &mut self.known_dirs;
        let resolved = resolve_in_root(rel, |p| {
            if known.contains(p) {
                return PathState::NotLink;
            }
            let full = join_path(dir, p);
            match file_type(&full) {
                Some(S_IFLNK) => readlink(&full).map_or(PathState::Unreadable, PathState::Link),
                Some(S_IFDIR) => {
                    known.insert(String::from(p));
                    PathState::NotLink
                }
                _ => PathState::NotLink,
            }
        })?;
        Some(join_path(&self.dir, &resolved))
    }

    /// The full path to create entry `rel` at. A symlink already at that name
    /// is removed so the entry replaces it (GNU tar's behaviour) rather than
    /// being written through it.
    fn place(&mut self, rel: &str) -> Option<String> {
        let full = self.resolve(rel)?;
        if file_type(&full) == Some(S_IFLNK) {
            unlink(&full);
        }
        Some(full)
    }
}

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
    /// Entries that could not be written: a create or write that failed, or
    /// an archive that ended inside an entry. Each used to be printed and
    /// forgotten - `tar` exited 0 and `box pull` cached the half-written
    /// layer as complete.
    pub failed: usize,
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
    let mut root = Root::new(target_dir);
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

        let Some(full_path) = root.place(&entry.path) else {
            eprintln(&format!("tar: refusing entry behind a symlink loop: {}", entry.path));
            stats.rejected += 1;
            read_skip(fd, entry.payload_span());
            continue;
        };
        count(&mut stats, &entry);

        if entry.kind == EntryKind::File {
            let (written, ok) = write_entry_streaming(fd, &full_path, entry.size, opts.verbose);
            if ok {
                apply_attrs(&full_path, &entry);
            } else {
                stats.failed += 1;
            }
            let padding = crate::format::padded_size(entry.size) - written;
            if padding > 0 {
                read_skip(fd, padding);
            }
        } else {
            create_nonfile(&full_path, &entry, opts.verbose, &mut root);
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
    let mut root = Root::new(target_dir);
    if opts.verbose {
        print("tar: decompressed to ");
        print_dec(data.len());
        println(" bytes");
    }

    let mut stats = Stats::default();
    let mut pending = crate::format::Overrides::default();
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

        if entry.kind == EntryKind::Metadata {
            // PAX / GNU long-name records describe the entry after them.
            let end = (pos + entry.size).min(data.len());
            match entry.typeflag {
                b'x' => pending = crate::format::parse_pax(&data[pos..end]),
                b'L' => pending.path = Some(crate::format::parse_gnu_longname(&data[pos..end])),
                b'K' => pending.linkpath = Some(crate::format::parse_gnu_longname(&data[pos..end])),
                _ => {}
            }
            pos += entry.payload_span();
            continue;
        }
        let mut entry = entry;
        core::mem::take(&mut pending).apply(&mut entry);
        if entry.path.is_empty() {
            pos += entry.payload_span();
            continue;
        }
        if !is_path_safe(&entry.path) {
            eprintln(&format!("tar: refusing entry outside target: {}", entry.path));
            stats.rejected += 1;
            pos += entry.payload_span();
            continue;
        }

        let Some(full_path) = root.place(&entry.path) else {
            eprintln(&format!("tar: refusing entry behind a symlink loop: {}", entry.path));
            stats.rejected += 1;
            pos += entry.payload_span();
            continue;
        };
        count(&mut stats, &entry);

        if entry.kind == EntryKind::File {
            let end = pos + entry.size;
            if end > data.len() {
                eprintln(&format!("tar: error: truncated entry {}", entry.path));
                stats.failed += 1;
                break;
            }
            if !write_entry(&full_path, &data[pos..end], &entry, opts.verbose) {
                stats.failed += 1;
            }
        } else {
            create_nonfile(&full_path, &entry, opts.verbose, &mut root);
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
    if verbose || stats.rejected > 0 || stats.failed > 0 {
        print("tar: extracted ");
        print_dec(stats.entries);
        print(" entries");
        if stats.rejected > 0 {
            print(", refused ");
            print_dec(stats.rejected);
        }
        if stats.failed > 0 {
            print(", failed ");
            print_dec(stats.failed);
        }
        println("");
    }
}

/// Directory, symlink or hardlink. A hardlink is a real `link(2)` since
/// 2026-10-14; it was a relative symlink while the kernel's `linkat` copied the
/// whole file, and an image layer routinely has hundreds of names for one binary.
fn create_nonfile(full_path: &str, entry: &Entry, verbose: bool, root: &mut Root) {
    match entry.kind {
        EntryKind::Directory => {
            if verbose {
                print("d ");
                println(&entry.path);
            }
            if !mkdir_p(full_path) && verbose {
                eprintln(&format!("tar: warning: failed to create directory {full_path}"));
            }
            // A directory's mode matters as much as a file's: an image's `/tmp`
            // is `1777`, and before this every extracted directory kept
            // `mkdir`'s default instead. Applied now rather than deferred to
            // the end of the archive, which is only safe because extraction
            // runs as root — a non-root extractor would lock itself out of a
            // `0555` directory before writing its contents.
            apply_attrs(full_path, entry);
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
            if ret >= 0 {
                apply_attrs(full_path, entry);
            }
        }
        EntryKind::HardLink => {
            let target = crate::format::normalize_entry_path(&entry.linkname);
            if verbose {
                print("h ");
                print(&entry.path);
                print(" => ");
                println(&target);
            }
            ensure_parent(full_path);
            // A real hard link to the earlier entry, its path resolved inside
            // the root like any other (a link through a planted symlink would
            // otherwise be a second name for a host file). Never to a symlink
            // itself, which `link` may follow. The relative symlink this always
            // used to be — while `linkat` copied the file — is the fallback for
            // a filesystem that cannot link (`EPERM`) or a target outside it
            // (`EXDEV`).
            if is_path_safe(&target) {
                if let Some(target_full) = root.resolve(&target) {
                    if file_type(&target_full) != Some(S_IFLNK) && link(&target_full, full_path) == 0 {
                        return;
                    }
                }
            }
            let rel = relative_symlink_target(&entry.path, &target);
            let ret = symlink(&rel, full_path);
            if ret < 0 && verbose {
                eprintln(&format!("tar: warning: hardlink {full_path}: errno {}", -ret));
            }
        }
        EntryKind::File | EntryKind::Metadata => {}
    }
}

/// Write `data` to `fd` across short writes; false on an error.
fn write_all_fd(fd: i32, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = write_fd(fd, data);
        if n <= 0 {
            return false;
        }
        data = &data[(n as usize).min(data.len())..];
    }
    true
}

/// False if the file could not be created or fully written.
fn write_entry(full_path: &str, data: &[u8], entry: &Entry, verbose: bool) -> bool {
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
        return false;
    }
    let ok = write_all_fd(fd, data);
    close(fd);
    if !ok {
        eprintln(&format!("tar: error: write failed for {full_path}"));
        return false;
    }
    apply_attrs(full_path, entry);
    true
}

/// Copy `size` bytes from `fd` into a new file. Returns the bytes consumed from
/// the archive, so the caller can skip the right amount of padding even if the
/// read stopped early, and whether all `size` bytes reached the file.
fn write_entry_streaming(fd: i32, full_path: &str, size: usize, verbose: bool) -> (usize, bool) {
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
        return (size, false);
    }

    let mut remaining = size;
    let mut buf = [0u8; 4096];
    let mut write_ok = true;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let n = read_fd(fd, &mut buf[..want]);
        if n <= 0 {
            eprintln(&format!("tar: error: archive ended inside {full_path}"));
            break;
        }
        let n = n as usize;
        remaining -= n; // consumed from the archive, written or not
        if !write_all_fd(out, &buf[..n]) {
            eprintln(&format!("tar: error: write failed for {full_path}"));
            write_ok = false;
            break;
        }
    }
    close(out);
    (size - remaining, write_ok && remaining == 0)
}

/// Apply the archived owner and permission bits — owner first, as GNU tar
/// does, so a kernel that clears set-id bits on `chown` cannot undo the mode.
///
/// Skipping the mode is invisible until something checks: a shell's `PATH`
/// search calls `access(X_OK)` and refuses a 0644 binary with "Permission
/// denied", which is exactly how an extracted image fails to run its own
/// commands. Skipping the owner is invisible until an image checks *who* owns
/// a path — postgres refuses a data directory it does not own.
///
/// Ownership is restored only when extracting as root (GNU tar's
/// `--same-owner` default): anyone else may not give files away, and the
/// attempt would just fail per entry. Symlinks get the owner but not a mode —
/// `chmod` follows the link and would change its target.
fn apply_attrs(full_path: &str, entry: &Entry) {
    if libakuma::geteuid() == 0 && (entry.uid != 0 || entry.gid != 0) {
        lchown(full_path, entry.uid, entry.gid);
    }
    if entry.mode != 0 && entry.kind != EntryKind::Symlink && entry.kind != EntryKind::HardLink {
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
