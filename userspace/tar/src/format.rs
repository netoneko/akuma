//! Tar header parsing and path arithmetic — pure functions, no I/O.
//!
//! Everything here is decided from bytes alone, which is what makes it testable
//! on the host. The two bugs this module exists to keep fixed both lived in
//! header interpretation: mode bits were never read (so extracted binaries lost
//! `+x`, and a shell's `PATH` search refused them), and hardlink entries need
//! their own handling or they cost a full file copy each.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const BLOCK_SIZE: usize = 512;

/// What a header says to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    /// A second name for an earlier entry. Extracted as a relative symlink:
    /// akuma's `linkat` copies the whole file, which turns an image's 410
    /// hardlinks to one binary into 410 copies of it.
    HardLink,
    /// pax/GNU extended headers — data to skip, not an entry to create.
    Metadata,
}

/// A parsed 512-byte tar header.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub linkname: String,
    pub size: usize,
    pub mode: u32,
    /// Numeric owner and group from the header. Names (`uname`/`gname`) are
    /// deliberately ignored: they would resolve against the *extracting*
    /// system's user table, and an OCI layer's ids mean what the image's own
    /// `/etc/passwd` says — runc and containerd use the numbers too.
    pub uid: u32,
    pub gid: u32,
    pub kind: EntryKind,
    /// The raw ustar typeflag — distinguishes PAX `x` / GNU `L` among
    /// [`EntryKind::Metadata`].
    pub typeflag: u8,
}

impl Entry {
    /// Bytes to advance past this entry's payload, including block padding.
    #[must_use]
    pub const fn payload_span(&self) -> usize {
        padded_size(self.size)
    }
}

/// Parse a header block. `None` means "not an entry": an all-zero block (the
/// archive terminator) or a block whose checksum does not verify.
#[must_use]
pub fn parse_header(header: &[u8; BLOCK_SIZE]) -> Option<Entry> {
    if header.iter().all(|&b| b == 0) || !verify_checksum(header) {
        return None;
    }

    let typeflag = header[156];
    let raw_path = parse_tar_path(header);
    let kind = match typeflag {
        b'x' | b'g' | b'L' | b'K' => EntryKind::Metadata,
        b'5' => EntryKind::Directory,
        b'2' => EntryKind::Symlink,
        b'1' => EntryKind::HardLink,
        _ if raw_path.ends_with('/') => EntryKind::Directory,
        _ => EntryKind::File,
    };

    Some(Entry {
        path: normalize_entry_path(&raw_path),
        linkname: extract_str(&header[157..257]),
        size: parse_octal(&header[124..136]),
        mode: (parse_octal(&header[100..108]) & 0o7777) as u32,
        uid: parse_id(&header[108..116]),
        gid: parse_id(&header[116..124]),
        kind,
        typeflag,
    })
}

/// Overrides a PAX extended header (`x`) or a GNU long-name record (`L`/`K`)
/// applies to the **next** entry.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub path: Option<String>,
    pub linkpath: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<usize>,
}

impl Overrides {
    /// Apply to the entry they precede.
    pub fn apply(self, e: &mut Entry) {
        if let Some(p) = self.path {
            e.path = normalize_entry_path(&p);
        }
        if let Some(l) = self.linkpath {
            e.linkname = l;
        }
        if let Some(u) = self.uid {
            e.uid = u;
        }
        if let Some(g) = self.gid {
            e.gid = g;
        }
        if let Some(n) = self.size {
            e.size = n;
        }
    }
}

/// Parse a PAX payload: records of `"<len> <key>=<value>\n"`. Unknown keys and
/// malformed records are skipped, as every reader must.
#[must_use]
pub fn parse_pax(data: &[u8]) -> Overrides {
    let mut o = Overrides::default();
    let mut pos = 0;
    while pos < data.len() {
        let Some(sp) = data[pos..].iter().position(|&b| b == b' ') else { break };
        let Ok(len) = core::str::from_utf8(&data[pos..pos + sp]).unwrap_or("").parse::<usize>() else { break };
        if len == 0 || pos + len > data.len() || sp + 1 >= len {
            break;
        }
        let rec = &data[pos + sp + 1..pos + len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&b| b == b'=') {
            let key = core::str::from_utf8(&rec[..eq]).unwrap_or("");
            let val = String::from(core::str::from_utf8(&rec[eq + 1..]).unwrap_or(""));
            match key {
                "path" => o.path = Some(val),
                "linkpath" => o.linkpath = Some(val),
                "uid" => o.uid = val.parse().ok(),
                "gid" => o.gid = val.parse().ok(),
                "size" => o.size = val.parse().ok(),
                _ => {}
            }
        }
        pos += len;
    }
    o
}

/// A GNU `L`/`K` payload is the bare NUL-terminated name.
#[must_use]
pub fn parse_gnu_longname(data: &[u8]) -> String {
    extract_str(data)
}

/// Strip the `./` prefix tar archives conventionally carry.
#[must_use]
pub fn normalize_entry_path(path: &str) -> String {
    let mut p = path;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    String::from(p)
}

/// Whether an entry may be written under the extraction directory.
///
/// An archive is untrusted input — `box pull` hands this whatever a registry
/// served — and both an absolute path and a `..` component would place files
/// outside the target directory. Neither is legal in an OCI layer, so rejecting
/// is free.
#[must_use]
pub fn is_path_safe(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    !path.split('/').any(|c| c == "..")
}

/// Parse the filename, using the USTAR prefix field only when the magic is
/// present — a non-USTAR archive has unrelated bytes at that offset.
#[must_use]
pub fn parse_tar_path(header: &[u8; BLOCK_SIZE]) -> String {
    let name = extract_str(&header[0..100]);

    if header[257..263].starts_with(b"ustar\0") {
        let prefix = extract_str(&header[345..500]);
        if !prefix.is_empty() {
            return format!("{prefix}/{name}");
        }
    }
    name
}

/// Verify the header checksum, which is what catches a misaligned read before
/// it is interpreted as garbage entries.
#[must_use]
pub fn verify_checksum(header: &[u8; BLOCK_SIZE]) -> bool {
    let stored = parse_octal(&header[148..156]);

    // The field is checksummed as if it held spaces. Some writers used signed
    // byte arithmetic, so accept either total.
    let mut unsigned: u32 = 0;
    let mut signed: u32 = 0;
    for (i, &b) in header.iter().enumerate() {
        if (148..156).contains(&i) {
            unsigned += 0x20;
            signed += 0x20;
        } else {
            unsigned += u32::from(b);
            signed = signed.wrapping_add(i32::from(b as i8) as u32);
        }
    }

    unsigned as usize == stored || signed as usize == stored
}

/// Extract a NUL-terminated string from a fixed-width header field.
#[must_use]
pub fn extract_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from(core::str::from_utf8(&bytes[..end]).unwrap_or(""))
}

/// Parse an octal header field.
/// A uid/gid field: octal, or GNU base-256 (high bit of the first byte set)
/// for ids too large for seven octal digits. Anything unparseable is 0, the
/// same answer the old extractor gave by not reading the field at all.
#[must_use]
pub fn parse_id(bytes: &[u8]) -> u32 {
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        let mut v: u64 = u64::from(bytes[0] & 0x7f);
        for &b in &bytes[1..] {
            v = (v << 8) | u64::from(b);
        }
        return u32::try_from(v).unwrap_or(0);
    }
    u32::try_from(parse_octal(bytes)).unwrap_or(0)
}

#[must_use]
pub fn parse_octal(bytes: &[u8]) -> usize {
    let s = core::str::from_utf8(bytes).unwrap_or("");
    let s = s.trim_matches(|c: char| c == '\0' || c == ' ');
    usize::from_str_radix(s, 8).unwrap_or(0)
}

/// Round up to the next 512-byte block boundary.
#[must_use]
pub const fn padded_size(size: usize) -> usize {
    size.div_ceil(BLOCK_SIZE) * BLOCK_SIZE
}

/// Join an extraction directory and an archive-relative path.
#[must_use]
pub fn join_path(target_dir: &str, path: &str) -> String {
    let mut full = String::from(target_dir);
    if !full.ends_with('/') && !path.starts_with('/') {
        full.push('/');
    } else if full.ends_with('/') && path.starts_with('/') {
        full.pop();
    }
    full.push_str(path);
    full
}

/// Compute a relative symlink target from `link_path` to `target_path`, both
/// relative to the archive root (e.g. `bin/sh` and `bin/busybox` → `busybox`).
///
/// Relative rather than absolute because the extraction directory is not where
/// the tree will be rooted at runtime: an image layer is extracted to
/// `/var/lib/box/layers/<digest>/` but a container sees it as `/`.
#[must_use]
pub fn relative_symlink_target(link_path: &str, target_path: &str) -> String {
    let link_dir = link_path.rfind('/').map_or("", |i| &link_path[..i]);
    let target_dir = target_path.rfind('/').map_or("", |i| &target_path[..i]);
    let target_name = target_path.rfind('/').map_or(target_path, |i| &target_path[i + 1..]);

    if link_dir == target_dir {
        return String::from(target_name);
    }

    let split = |s: &str| -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            s.split('/').map(String::from).collect()
        }
    };
    let link_parts = split(link_dir);
    let target_parts = split(target_dir);

    let mut common = 0;
    while common < link_parts.len()
        && common < target_parts.len()
        && link_parts[common] == target_parts[common]
    {
        common += 1;
    }

    let mut result = String::new();
    for _ in common..link_parts.len() {
        result.push_str("../");
    }
    for part in target_parts.iter().skip(common) {
        result.push_str(part);
        result.push('/');
    }
    result.push_str(target_name);
    result
}

/// Strip the gzip framing, returning the raw DEFLATE stream.
///
/// Gzip is a 10-byte header, optional extra/name/comment/CRC16 fields selected
/// by the flag byte, the DEFLATE stream, and an 8-byte trailer.
#[must_use]
pub fn strip_gzip_header(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return None;
    }
    let flg = data[3];
    let mut pos: usize = 10;

    if flg & 0x04 != 0 {
        if pos + 2 > data.len() {
            return None;
        }
        let xlen = data[pos] as usize | (data[pos + 1] as usize) << 8;
        pos += 2 + xlen;
    }
    if flg & 0x08 != 0 {
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flg & 0x10 != 0 {
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flg & 0x02 != 0 {
        pos += 2;
    }

    let end = data.len() - 8;
    if pos >= end {
        return None;
    }
    Some(&data[pos..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_records_override_the_next_entry() {
        let o = parse_pax(b"20 path=long/name.x\n12 uid=4000\n13 foo=bar\n");
        assert_eq!(o.path.as_deref(), Some("long/name.x"));
        assert_eq!(o.uid, Some(4000));
        assert_eq!(o.gid, None);
        // Truncated / malformed input must not panic or loop.
        let _ = parse_pax(b"999 path=x\n");
        let _ = parse_pax(b"abc");
        let _ = parse_pax(b"5 \n");
        assert_eq!(parse_gnu_longname(b"a/b\0\0\0"), "a/b");
    }


    /// Build a header the way GNU tar does, so the checksum is real.
    fn header(name: &str, size: usize, mode: u32, typeflag: u8, linkname: &str) -> [u8; BLOCK_SIZE] {
        let mut h = [0u8; BLOCK_SIZE];
        h[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut h[100..108], mode as usize);
        write_octal(&mut h[124..136], size);
        h[156] = typeflag;
        h[157..157 + linkname.len()].copy_from_slice(linkname.as_bytes());
        h[257..262].copy_from_slice(b"ustar");

        // Checksum last: it is computed over the field-as-spaces.
        for b in &mut h[148..156] {
            *b = b' ';
        }
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        write_octal(&mut h[148..155], sum as usize);
        h[155] = 0;
        h
    }

    fn fix_checksum(h: &mut [u8; BLOCK_SIZE]) {
        for b in &mut h[148..156] {
            *b = b' ';
        }
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        write_octal(&mut h[148..155], sum as usize);
        h[155] = 0;
    }

    fn write_octal(field: &mut [u8], value: usize) {
        let mut v = value;
        for slot in field.iter_mut().rev().skip(1) {
            *slot = b'0' + (v % 8) as u8;
            v /= 8;
        }
    }

    #[test]
    fn a_regular_file_carries_its_mode() {
        let e = parse_header(&header("bin/busybox", 1_185_328, 0o755, b'0', "")).unwrap();
        assert_eq!(e.kind, EntryKind::File);
        assert_eq!(e.path, "bin/busybox");
        assert_eq!(e.size, 1_185_328);
        assert_eq!(e.mode, 0o755, "mode is what makes an extracted binary runnable");
        assert_eq!(e.payload_span(), 2316 * BLOCK_SIZE);
    }

    #[test]
    fn each_typeflag_maps_to_its_kind() {
        for (flag, kind) in [
            (b'0', EntryKind::File),
            (b'\0', EntryKind::File),
            (b'5', EntryKind::Directory),
            (b'2', EntryKind::Symlink),
            (b'1', EntryKind::HardLink),
            (b'x', EntryKind::Metadata),
            (b'g', EntryKind::Metadata),
            (b'L', EntryKind::Metadata),
        ] {
            let e = parse_header(&header("x", 0, 0o644, flag, "")).unwrap();
            assert_eq!(e.kind, kind, "typeflag {}", flag as char);
        }
    }

    #[test]
    fn a_trailing_slash_means_directory_whatever_the_flag() {
        let e = parse_header(&header("etc/", 0, 0o755, b'0', "")).unwrap();
        assert_eq!(e.kind, EntryKind::Directory);
    }

    #[test]
    fn a_hardlink_reports_its_target() {
        let e = parse_header(&header("bin/cat", 0, 0o755, b'1', "bin/busybox")).unwrap();
        assert_eq!(e.kind, EntryKind::HardLink);
        assert_eq!(e.linkname, "bin/busybox");
        assert_eq!(e.size, 0, "a hardlink entry has no payload");
    }

    #[test]
    fn the_dot_slash_prefix_is_stripped() {
        let e = parse_header(&header("./etc/passwd", 0, 0o644, b'0', "")).unwrap();
        assert_eq!(e.path, "etc/passwd");
    }

    #[test]
    fn a_zero_block_and_a_corrupt_block_are_both_rejected() {
        assert!(parse_header(&[0u8; BLOCK_SIZE]).is_none());
        let mut bad = header("x", 0, 0o644, b'0', "");
        bad[0] = b'!'; // changes the sum without fixing the checksum field
        assert!(parse_header(&bad).is_none());
    }

    #[test]
    fn escaping_paths_are_refused() {
        assert!(is_path_safe("bin/busybox"));
        assert!(is_path_safe("a/b/c"));
        assert!(!is_path_safe("/etc/passwd"));
        assert!(!is_path_safe("../outside"));
        assert!(!is_path_safe("bin/../../outside"));
        assert!(!is_path_safe(""));
    }

    #[test]
    fn the_ustar_prefix_is_used_only_with_the_magic() {
        let mut h = header("name", 0, 0o644, b'0', "");
        h[345..355].copy_from_slice(b"some/where");
        // Re-checksum after the edit.
        for b in &mut h[148..156] {
            *b = b' ';
        }
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        write_octal(&mut h[148..155], sum as usize);
        h[155] = 0;
        assert_eq!(parse_tar_path(&h), "some/where/name");

        h[257..262].copy_from_slice(b"xxxxx");
        assert_eq!(parse_tar_path(&h), "name", "no magic, no prefix");
    }

    #[test]
    fn hardlink_targets_become_relative_symlinks() {
        assert_eq!(relative_symlink_target("bin/cat", "bin/busybox"), "busybox");
        assert_eq!(relative_symlink_target("usr/bin/env", "bin/env"), "../../bin/env");
        assert_eq!(relative_symlink_target("a/b/c/f", "a/g"), "../../g");
        assert_eq!(relative_symlink_target("top", "other"), "other");
    }

    #[test]
    fn paths_join_without_doubling_slashes() {
        assert_eq!(join_path("/layers/x", "bin/sh"), "/layers/x/bin/sh");
        assert_eq!(join_path("/layers/x/", "bin/sh"), "/layers/x/bin/sh");
        assert_eq!(join_path("/layers/x/", "/bin/sh"), "/layers/x/bin/sh");
    }

    #[test]
    fn octal_fields_tolerate_padding() {
        assert_eq!(parse_octal(b"0000755\0"), 0o755);
        assert_eq!(parse_octal(b"0000755 "), 0o755);
        assert_eq!(parse_octal(b"        "), 0);
        assert_eq!(parse_octal(b"garbage!"), 0);
    }

    #[test]
    fn sizes_round_up_to_a_block() {
        assert_eq!(padded_size(0), 0);
        assert_eq!(padded_size(1), 512);
        assert_eq!(padded_size(512), 512);
        assert_eq!(padded_size(513), 1024);
    }

    #[test]
    fn gzip_framing_is_stripped_and_junk_refused() {
        let mut gz = alloc::vec![0x1f, 0x8b, 0x08, 0x00];
        gz.extend_from_slice(&[0; 6]); // rest of the fixed header
        gz.extend_from_slice(b"DEFLATE-PAYLOAD");
        gz.extend_from_slice(&[0; 8]); // CRC32 + ISIZE
        assert_eq!(strip_gzip_header(&gz), Some(&b"DEFLATE-PAYLOAD"[..]));

        assert!(strip_gzip_header(b"not gzip at all___________").is_none());
        assert!(strip_gzip_header(&[0x1f, 0x8b]).is_none());
    }

    #[test]
    fn owner_fields_are_parsed_in_octal_and_base_256() {
        let mut h = header("f", 0, 0o644, b'0', "");
        write_octal(&mut h[108..116], 999);
        write_octal(&mut h[116..124], 1000);
        fix_checksum(&mut h);
        let e = parse_header(&h).unwrap();
        assert_eq!((e.uid, e.gid), (999, 1000));

        // GNU base-256: 0x80 marker, big-endian value in the remaining bytes.
        let mut h = header("f", 0, 0o644, b'0', "");
        h[108..116].copy_from_slice(&[0x80, 0, 0, 0, 0, 0x01, 0x11, 0x70]);
        fix_checksum(&mut h);
        assert_eq!(parse_header(&h).unwrap().uid, 70_000);
    }
}
