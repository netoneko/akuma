//! Tar header *encoding* — the inverse of [`crate::format::parse_header`], pure
//! and host-testable. `box commit` streams a directory through this to make an
//! OCI layer; the file payloads are the caller's (it already has them open), so
//! this module only produces headers and padding.
//!
//! The format is POSIX ustar, with a PAX extended header (`x`) in front of an
//! entry only when a field does not fit: a path over what name+prefix hold, a
//! link target over 100 bytes, an id over seven octal digits, a size over 8 GiB.
//! That is what GNU tar and Docker emit, and what `docker load` reads.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::format::{padded_size, BLOCK_SIZE};

/// What kind of entry to write. Values are the ustar typeflags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    /// A second name for a path already in the archive; `linkname` is that path.
    HardLink,
}

impl Kind {
    const fn typeflag(self) -> u8 {
        match self {
            Self::File => b'0',
            Self::Directory => b'5',
            Self::Symlink => b'2',
            Self::HardLink => b'1',
        }
    }
}

pub struct WriteEntry<'a> {
    /// Archive-relative, no leading `/` or `./`. A directory's trailing `/` is
    /// added here.
    pub path: &'a str,
    pub linkname: &'a str,
    pub kind: Kind,
    /// Payload bytes that follow the header — 0 for everything but `File`.
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
}

const MAX_OCTAL_7: u64 = 0o7_777_777;
const MAX_OCTAL_11: u64 = 0o77_777_777_777;

fn put_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let mut v = value;
    for slot in field[..digits].iter_mut().rev() {
        *slot = b'0' + (v & 7) as u8;
        v >>= 3;
    }
    field[digits] = 0;
}

fn put_str(field: &mut [u8], s: &str) {
    field[..s.len()].copy_from_slice(s.as_bytes());
}

/// Split `path` into ustar `(prefix, name)` at a `/`, if it can be done.
fn split_ustar(path: &str) -> Option<(&str, &str)> {
    if path.len() <= 100 {
        return Some(("", path));
    }
    for (i, _) in path.match_indices('/') {
        let (prefix, name) = (&path[..i], &path[i + 1..]);
        if prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            return Some((prefix, name));
        }
    }
    None
}

/// One PAX record: `"<len> <key>=<value>\n"` where `len` counts itself.
fn pax_record(key: &str, value: &str, out: &mut Vec<u8>) {
    let body = key.len() + value.len() + 3; // space, '=', newline
    let mut len = body + 1;
    // The decimal length is part of the length; iterate to a fixed point.
    loop {
        let total = body + format!("{len}").len();
        if total == len {
            break;
        }
        len = total;
    }
    out.extend_from_slice(format!("{len} {key}={value}\n").as_bytes());
}

fn raw_header(name: &str, prefix: &str, e: &WriteEntry, typeflag: u8, size: u64, linkname: &str) -> [u8; BLOCK_SIZE] {
    let mut h = [0u8; BLOCK_SIZE];
    // Over-long fields are truncated here and carried in full by the PAX header.
    put_str(&mut h[0..100], &name[..name.len().min(100)]);
    put_octal(&mut h[100..108], u64::from(e.mode & 0o7777));
    put_octal(&mut h[108..116], u64::from(e.uid).min(MAX_OCTAL_7));
    put_octal(&mut h[116..124], u64::from(e.gid).min(MAX_OCTAL_7));
    put_octal(&mut h[124..136], size.min(MAX_OCTAL_11));
    put_octal(&mut h[136..148], e.mtime.min(MAX_OCTAL_11));
    h[156] = typeflag;
    put_str(&mut h[157..257], &linkname[..linkname.len().min(100)]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    put_str(&mut h[345..500], &prefix[..prefix.len().min(155)]);
    for b in &mut h[148..156] {
        *b = b' ';
    }
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    put_octal(&mut h[148..155], u64::from(sum));
    h[155] = b' ';
    h
}

/// Append the header block(s) for `e` to `out`: a PAX extension first when one
/// is needed, then the entry's own header. The payload and its padding
/// ([`padding_for`]) follow from the caller.
pub fn encode_header(e: &WriteEntry, out: &mut Vec<u8>) {
    let mut path = String::from(e.path);
    if e.kind == Kind::Directory && !path.ends_with('/') {
        path.push('/');
    }

    let mut pax = Vec::new();
    let (prefix, name) = match split_ustar(&path) {
        Some(split) => split,
        None => {
            pax_record("path", &path, &mut pax);
            ("", path.as_str())
        }
    };
    if e.linkname.len() > 100 {
        pax_record("linkpath", e.linkname, &mut pax);
    }
    if u64::from(e.uid) > MAX_OCTAL_7 {
        pax_record("uid", &format!("{}", e.uid), &mut pax);
    }
    if u64::from(e.gid) > MAX_OCTAL_7 {
        pax_record("gid", &format!("{}", e.gid), &mut pax);
    }
    if e.size > MAX_OCTAL_11 {
        pax_record("size", &format!("{}", e.size), &mut pax);
    }

    if !pax.is_empty() {
        // The extension header names itself innocuously; readers ignore it.
        let ext = WriteEntry {
            path: "",
            linkname: "",
            kind: Kind::File,
            size: pax.len() as u64,
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: e.mtime,
        };
        out.extend_from_slice(&raw_header("PaxHeader", "", &ext, b'x', pax.len() as u64, ""));
        out.extend_from_slice(&pax);
        out.resize(out.len() + padding_for(pax.len() as u64), 0);
    }

    let size = if e.kind == Kind::File { e.size } else { 0 };
    out.extend_from_slice(&raw_header(name, prefix, e, e.kind.typeflag(), size, e.linkname));
}

/// Zero bytes that pad a `size`-byte payload to the block boundary.
#[must_use]
pub const fn padding_for(size: u64) -> usize {
    (padded_size(size as usize)) - size as usize
}

/// The end-of-archive marker: two zero blocks.
pub const END_OF_ARCHIVE: [u8; 2 * BLOCK_SIZE] = [0; 2 * BLOCK_SIZE];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{parse_header, EntryKind};

    fn entry<'a>(path: &'a str, kind: Kind) -> WriteEntry<'a> {
        WriteEntry { path, linkname: "", kind, size: 0, mode: 0o755, uid: 0, gid: 0, mtime: 1_700_000_000 }
    }

    fn first_block(out: &[u8]) -> [u8; BLOCK_SIZE] {
        out[..BLOCK_SIZE].try_into().unwrap()
    }

    #[test]
    fn a_file_header_round_trips_through_the_parser() {
        let mut e = entry("usr/bin/tool", Kind::File);
        e.size = 1234;
        e.mode = 0o4755;
        e.uid = 1000;
        e.gid = 100;
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        assert_eq!(out.len(), BLOCK_SIZE);
        let p = parse_header(&first_block(&out)).expect("checksum must verify");
        assert_eq!(p.path, "usr/bin/tool");
        assert_eq!(p.kind, EntryKind::File);
        assert_eq!((p.size, p.mode, p.uid, p.gid), (1234, 0o4755, 1000, 100));
    }

    #[test]
    fn directories_get_a_trailing_slash_and_no_payload() {
        let mut e = entry("etc/apk", Kind::Directory);
        e.size = 99; // must be ignored
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        let p = parse_header(&first_block(&out)).unwrap();
        assert_eq!(p.kind, EntryKind::Directory);
        assert_eq!(p.size, 0);
    }

    #[test]
    fn links_carry_their_target() {
        let mut e = entry("bin/sh", Kind::Symlink);
        e.linkname = "busybox";
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        let p = parse_header(&first_block(&out)).unwrap();
        assert_eq!((p.kind, p.linkname.as_str()), (EntryKind::Symlink, "busybox"));

        let mut e = entry("bin/ls", Kind::HardLink);
        e.linkname = "bin/busybox";
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        let p = parse_header(&first_block(&out)).unwrap();
        assert_eq!((p.kind, p.linkname.as_str()), (EntryKind::HardLink, "bin/busybox"));
    }

    #[test]
    fn a_long_path_splits_into_prefix_and_name_without_pax() {
        let dir = "d".repeat(60);
        let path = format!("{dir}/{dir}/{}", "f".repeat(50));
        assert!(path.len() > 100);
        let mut out = Vec::new();
        encode_header(&entry(&path, Kind::File), &mut out);
        assert_eq!(out.len(), BLOCK_SIZE, "fits ustar: no extension header");
        assert_eq!(parse_header(&first_block(&out)).unwrap().path, path);
    }

    #[test]
    fn an_unsplittable_path_uses_a_pax_header() {
        let path = "x".repeat(300);
        let mut out = Vec::new();
        encode_header(&entry(&path, Kind::File), &mut out);
        assert_eq!(out[156], b'x');
        assert!(out.len() > 2 * BLOCK_SIZE);
        let pax = &out[BLOCK_SIZE..2 * BLOCK_SIZE];
        let text = core::str::from_utf8(&pax[..pax.iter().position(|&b| b == 0).unwrap()]).unwrap();
        assert!(text.ends_with(&format!(" path={path}\n")));
        // The record's own length prefix must be exact.
        let n: usize = text.split(' ').next().unwrap().parse().unwrap();
        assert_eq!(n, text.len());
        // Then the real header follows.
        assert_eq!(out[2 * BLOCK_SIZE + 156], b'0');
    }

    /// What the writer emits, the extractor's PAX reader must read back — a
    /// layer `box commit` writes is unpacked by `box commit` itself.
    #[test]
    fn pax_output_is_read_back_by_the_extractor_side() {
        let path = "p/".repeat(150);
        let mut e = entry(&path, Kind::Symlink);
        let link = "t".repeat(200);
        e.linkname = &link;
        e.uid = 4_000_000_000;
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        let ext = parse_header(&first_block(&out)).expect("extension header parses");
        assert_eq!(ext.kind, EntryKind::Metadata);
        assert_eq!(ext.typeflag, b'x');
        let o = crate::format::parse_pax(&out[BLOCK_SIZE..BLOCK_SIZE + ext.size]);
        assert_eq!(o.path.as_deref(), Some(path.as_str()));
        assert_eq!(o.linkpath.as_deref(), Some(link.as_str()));
        assert_eq!(o.uid, Some(4_000_000_000));
        let at = BLOCK_SIZE + padded_size(ext.size);
        let mut real = parse_header(out[at..at + BLOCK_SIZE].try_into().unwrap()).unwrap();
        o.apply(&mut real);
        assert_eq!((real.path.trim_end_matches('/'), real.linkname.as_str()), (path.trim_end_matches('/'), link.as_str()));
        assert_eq!(real.uid, 4_000_000_000);
    }

    #[test]
    fn big_ids_go_to_pax_and_clamp_in_the_header() {
        let mut e = entry("f", Kind::File);
        e.uid = 4_000_000_000;
        let mut out = Vec::new();
        encode_header(&e, &mut out);
        assert_eq!(out[156], b'x');
        let text = String::from_utf8_lossy(&out[BLOCK_SIZE..2 * BLOCK_SIZE]);
        assert!(text.contains(" uid=4000000000\n"));
    }

    #[test]
    fn pax_record_length_is_self_consistent_across_digit_boundaries() {
        for vlen in [1usize, 5, 6, 7, 95, 96, 97, 98, 99, 100, 990, 996, 997] {
            let mut out = Vec::new();
            pax_record("path", &"v".repeat(vlen), &mut out);
            let text = core::str::from_utf8(&out).unwrap();
            let n: usize = text.split(' ').next().unwrap().parse().unwrap();
            assert_eq!(n, text.len(), "vlen {vlen}");
        }
    }

    #[test]
    fn padding_reaches_the_block_boundary() {
        assert_eq!(padding_for(0), 0);
        assert_eq!(padding_for(1), 511);
        assert_eq!(padding_for(512), 0);
        assert_eq!(padding_for(513), 511);
    }
}
