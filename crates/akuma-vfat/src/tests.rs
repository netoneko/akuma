extern crate std;

use crate::layout::*;
use crate::{BlockDevice, FatKind, FormatOptions, Vfat, format};
use akuma_vfs::{Filesystem, FsError};
use std::sync::{Arc, Mutex};
use std::vec;
use std::vec::Vec;

#[derive(Clone)]
pub struct Ram(pub Arc<Mutex<Vec<u8>>>);

impl Ram {
    pub fn new(bytes: usize) -> Self {
        Self(Arc::new(Mutex::new(vec![0xA5; bytes])))
    }
}

impl BlockDevice for Ram {
    fn read_bytes(&self, off: u64, buf: &mut [u8]) -> Result<(), ()> {
        let d = self.0.lock().unwrap();
        let o = off as usize;
        buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(())?);
        Ok(())
    }
    fn write_bytes(&self, off: u64, data: &[u8]) -> Result<(), ()> {
        let mut d = self.0.lock().unwrap();
        let o = off as usize;
        d.get_mut(o..o + data.len()).ok_or(())?.copy_from_slice(data);
        Ok(())
    }
}

fn clock() -> u64 {
    1_700_000_000
}

fn make(kind: FatKind, mib: usize) -> (Ram, Vfat<Ram>) {
    let ram = Ram::new(mib << 20);
    let opts = FormatOptions { kind: Some(kind), ..FormatOptions::default() };
    let g = format(&ram, (mib as u64) << 20, &opts).unwrap();
    assert_eq!(g.kind, kind);
    (ram.clone(), Vfat::mount(ram, clock).unwrap())
}

const KINDS: [(FatKind, usize); 3] = [(FatKind::Fat12, 4), (FatKind::Fat16, 64), (FatKind::Fat32, 64)];

// ------------------------------------------------------------------ layout

#[test]
fn checksum_known_value() {
    // Independent hand computation over "FOO     BAR".
    let mut sum = 0u8;
    for b in *b"FOO     BAR" {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b);
    }
    assert_eq!(lfn_checksum(b"FOO     BAR"), sum);
}

#[test]
fn dos_time_round_trip() {
    for secs in [315_532_800u64, 1_700_000_000, 1_709_164_799, 4_000_000_000, 951_782_400] {
        let (d, t, odd) = unix_to_dos(secs);
        let back = dos_to_unix(d, t).unwrap();
        assert_eq!(back + u64::from(odd == 100), secs, "{secs}");
    }
    assert_eq!(dos_to_unix(0, 0), None);
    // 2000-02-29 exists.
    let (d, t, _) = unix_to_dos(951_782_400);
    assert_eq!((d >> 9) + 1980, 2000);
    assert_eq!((d >> 5) & 0xF, 2);
    assert_eq!(d & 0x1F, 29);
    assert_eq!(t, 0);
}

#[test]
fn short_name_encoding() {
    assert_eq!(encode_short_exact("README.TXT"), Some((*b"README  TXT", 0)));
    assert_eq!(encode_short_exact("readme.txt"), Some((*b"README  TXT", 0x18)));
    assert_eq!(encode_short_exact("grub.cfg"), Some((*b"GRUB    CFG", 0x18)));
    assert_eq!(encode_short_exact("Makefile"), None); // mixed case
    assert_eq!(encode_short_exact("Readme.txt"), None);
    assert_eq!(encode_short_exact("toolongname.txt"), None);
    assert_eq!(encode_short_exact("a.b.c"), None);
    assert_eq!(encode_short_exact("has space"), None);
    assert_eq!(encode_short_exact("ünï.txt"), None);
    assert_eq!(encode_short_exact("NOEXT"), Some((*b"NOEXT      ", 0)));
    assert_eq!(encode_short_exact("lower.TXT"), Some((*b"LOWER   TXT", 0x08)));
}

#[test]
fn alias_generation() {
    assert_eq!(&make_alias("Hello World.text", 1), b"HELLOW~1TEX");
    assert_eq!(&make_alias("Makefile", 1), b"MAKEFI~1   ");
    assert_eq!(&make_alias("Makefile", 12), b"MAKEF~12   ");
    assert_eq!(&make_alias(".hidden", 1), b"HIDDEN~1   ");
    assert_eq!(&make_alias("a+b.c", 1), b"A_B~1   C  ");
}

#[test]
fn short_display_applies_case_flags() {
    assert_eq!(short_display(b"GRUB    CFG", 0x18), "grub.cfg");
    assert_eq!(short_display(b"GRUB    CFG", 0x08), "grub.CFG");
    assert_eq!(short_display(b"NOEXT      ", 0x08), "noext");
}

#[test]
fn names() {
    assert!(valid_name("grub.cfg"));
    assert!(valid_name("a b.txt"));
    assert!(!valid_name(""));
    assert!(!valid_name("trailing."));
    assert!(!valid_name("a:b"));
    assert!(!valid_name("a?b"));
    assert!(!valid_name(&"x".repeat(256)));
    assert!(valid_name(&"x".repeat(255)));
    assert!(names_equal("GRUB.CFG", "grub.cfg"));
    assert!(!names_equal("a", "ab"));
}

#[test]
fn lfn_build_and_read_back() {
    let name = "A fairly long file name with ünïcode.txt";
    let short = make_alias(name, 1);
    let slots = build_lfn(name, &short);
    assert_eq!(slots.len(), lfn_slots_needed(name));
    assert_eq!(slots[0][0] & 0x40, 0x40, "highest ordinal first, flagged");
    assert_eq!(slots.last().unwrap()[0], 1);
    let mut units = Vec::new();
    for s in slots.iter().rev() {
        assert_eq!(s[13], lfn_checksum(&short));
        units.extend_from_slice(&lfn_units(s));
    }
    let end = units.iter().position(|&u| u == 0).unwrap();
    assert_eq!(String::from_utf16(&units[..end]).unwrap(), name);
    // Exactly-13-unit name has no terminator and no padding.
    let s = build_lfn("abcdefghijklm", &short);
    assert_eq!(s.len(), 1);
    assert!(lfn_units(&s[0]).iter().all(|&u| u != 0 && u != 0xFFFF));
}

#[test]
fn geometry_rejects_garbage() {
    assert_eq!(Geometry::parse(&[0u8; 512]), Err(FsError::NoFilesystem));
    let ram = Ram::new(4 << 20);
    format(&ram, 4 << 20, &FormatOptions::default()).unwrap();
    let mut sector = ram.0.lock().unwrap()[..512].to_vec();
    assert!(Geometry::parse(&sector).is_ok());
    sector[13] = 3; // sectors per cluster not a power of two
    assert_eq!(Geometry::parse(&sector), Err(FsError::Corrupt));
}

#[test]
fn format_picks_kinds_by_size() {
    for (mib, kind) in [(2usize, FatKind::Fat12), (100, FatKind::Fat16), (600, FatKind::Fat32)] {
        let ram = Ram::new(0); // too small to write: only the plan is checked via error type
        let _ = ram;
        let dev = SparseCheck;
        let g = format(&dev, (mib as u64) << 20, &FormatOptions::default()).unwrap();
        assert_eq!(g.kind, kind, "{mib} MiB");
    }
    assert!(format(&SparseCheck, 10 << 20, &FormatOptions { kind: Some(FatKind::Fat32), ..Default::default() }).is_err());
}

/// A device that accepts every write and discards it (for planning large sizes).
struct SparseCheck;
impl BlockDevice for SparseCheck {
    fn read_bytes(&self, _: u64, buf: &mut [u8]) -> Result<(), ()> {
        buf.fill(0);
        Ok(())
    }
    fn write_bytes(&self, _: u64, _: &[u8]) -> Result<(), ()> {
        Ok(())
    }
}

// -------------------------------------------------------------- filesystem

#[test]
fn empty_volume() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        assert!(fs.read_dir("/").unwrap().is_empty());
        let st = fs.stats().unwrap();
        let expect = if kind == FatKind::Fat32 { st.total_blocks - 1 } else { st.total_blocks };
        assert_eq!(st.free_blocks, expect, "{kind:?}");
        assert!(fs.metadata("/").unwrap().is_dir);
    }
}

#[test]
fn write_read_round_trip_and_remount() {
    for (kind, mib) in KINDS {
        let (ram, fs) = make(kind, mib);
        fs.write_file("/grub.cfg", b"set timeout=5\n").unwrap();
        assert_eq!(fs.read_file("/grub.cfg").unwrap(), b"set timeout=5\n");
        assert_eq!(fs.read_file("/GRUB.CFG").unwrap(), b"set timeout=5\n", "case-insensitive");
        drop(fs);
        let fs = Vfat::mount(ram, clock).unwrap();
        assert_eq!(fs.read_file("/grub.cfg").unwrap(), b"set timeout=5\n", "{kind:?}");
        let m = fs.metadata("/grub.cfg").unwrap();
        assert_eq!((m.size, m.is_dir, m.modified), (14, false, Some(clock())));
    }
}

#[test]
fn edit_in_place_like_grub_cfg() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        fs.create_dir("/EFI").unwrap();
        fs.create_dir("/EFI/akuma").unwrap();
        fs.write_file("/EFI/akuma/grub.cfg", b"menuentry one {}\n").unwrap();
        // Replace with something shorter, then longer.
        fs.write_file("/EFI/akuma/grub.cfg", b"x\n").unwrap();
        assert_eq!(fs.read_file("/EFI/akuma/grub.cfg").unwrap(), b"x\n");
        let long = "menuentry two { linux /boot/akuma }\n".repeat(400);
        fs.write_file("/EFI/akuma/grub.cfg", long.as_bytes()).unwrap();
        assert_eq!(fs.read_file("/EFI/akuma/grub.cfg").unwrap(), long.as_bytes());
        let (n, wrote) = fs.append("/EFI/akuma/grub.cfg", b"# end\n").unwrap();
        assert_eq!((n, wrote), (long.len(), 6));
        let all = fs.read_file("/EFI/akuma/grub.cfg").unwrap();
        assert!(all.ends_with(b"# end\n"));
        assert_eq!(all.len(), long.len() + 6);
    }
}

#[test]
fn long_names_and_listing() {
    for (kind, mib) in KINDS {
        let (ram, fs) = make(kind, mib);
        let names = ["Hello World.txt", "Makefile", "lower.txt", "UPPER.TXT", "ünï.txt", "a.b.c.d"];
        for n in names {
            fs.write_file(&std::format!("/{n}"), n.as_bytes()).unwrap();
        }
        let fs = {
            drop(fs);
            Vfat::mount(ram, clock).unwrap()
        };
        let mut listed: Vec<_> = fs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
        listed.sort();
        let mut want: Vec<_> = names.iter().map(|s| (*s).into()).collect::<Vec<std::string::String>>();
        want.sort();
        assert_eq!(listed, want, "{kind:?}");
        for n in names {
            assert_eq!(fs.read_file(&std::format!("/{n}")).unwrap(), n.as_bytes());
        }
        // Found by alias too.
        assert_eq!(fs.read_file("/HELLOW~1.TXT").unwrap(), b"Hello World.txt");
        assert!(fs.exists("/hello world.TXT"));
    }
}

#[test]
fn alias_collisions_are_numbered() {
    let (_, fs) = make(FatKind::Fat16, 64);
    for i in 0..12 {
        fs.write_file(&std::format!("/Long File Name {i}.txt"), b"x").unwrap();
    }
    assert_eq!(fs.read_dir("/").unwrap().len(), 12);
    assert!(fs.exists("/LONGFI~1.TXT"));
    assert!(fs.exists("/LONGFI~9.TXT"));
    assert!(fs.exists("/LONGF~10.TXT"));
}

#[test]
fn large_file_across_clusters_and_free_space_returns() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        let before = fs.stats().unwrap().free_blocks;
        let data: Vec<u8> = (0..1_000_003u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        fs.write_file("/big.bin", &data).unwrap();
        assert_eq!(fs.read_file("/big.bin").unwrap(), data, "{kind:?}");
        let mut buf = [0u8; 5000];
        let n = fs.read_at("/big.bin", 123_457, &mut buf).unwrap();
        assert_eq!(&buf[..n], &data[123_457..123_457 + n]);
        assert_eq!(fs.read_at("/big.bin", data.len(), &mut buf).unwrap(), 0);
        assert!(fs.stats().unwrap().free_blocks < before);
        fs.remove_file("/big.bin").unwrap();
        assert_eq!(fs.stats().unwrap().free_blocks, before, "{kind:?} leaked clusters");
    }
}

#[test]
fn write_at_extends_and_zero_fills() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        fs.write_file("/f", b"abc").unwrap();
        fs.write_at("/f", 10_000, b"Z").unwrap();
        let v = fs.read_file("/f").unwrap();
        assert_eq!(v.len(), 10_001);
        assert_eq!(&v[..3], b"abc");
        assert!(v[3..10_000].iter().all(|&b| b == 0), "{kind:?}");
        assert_eq!(v[10_000], b'Z');
        // Overwrite in the middle leaves the rest.
        fs.write_at("/f", 1, b"XY").unwrap();
        assert_eq!(&fs.read_file("/f").unwrap()[..4], b"aXY\0");
    }
}

#[test]
fn truncate_shrink_grow_and_stale_data() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        let data = vec![0x55u8; 20_000];
        fs.write_file("/t", &data).unwrap();
        fs.truncate("/t", 100).unwrap();
        assert_eq!(fs.read_file("/t").unwrap().len(), 100);
        fs.truncate("/t", 9_000).unwrap();
        let v = fs.read_file("/t").unwrap();
        assert_eq!(v.len(), 9_000);
        assert!(v[..100].iter().all(|&b| b == 0x55));
        assert!(v[100..].iter().all(|&b| b == 0), "stale bytes resurfaced on {kind:?}");
        fs.truncate("/t", 0).unwrap();
        assert!(fs.read_file("/t").unwrap().is_empty());
    }
}

#[test]
fn directories_nest_and_remove() {
    for (kind, mib) in KINDS {
        let (_, fs) = make(kind, mib);
        fs.create_dir("/a").unwrap();
        fs.create_dir("/a/b").unwrap();
        fs.write_file("/a/b/f.txt", b"hi").unwrap();
        assert_eq!(fs.create_dir("/a").unwrap_err(), FsError::AlreadyExists);
        assert_eq!(fs.create_dir("/nope/x").unwrap_err(), FsError::NotFound);
        assert_eq!(fs.write_file("/a/b/f.txt/x", b"").unwrap_err(), FsError::NotADirectory);
        assert_eq!(fs.remove_dir("/a").unwrap_err(), FsError::DirectoryNotEmpty);
        assert_eq!(fs.remove_dir("/a/b/f.txt").unwrap_err(), FsError::NotADirectory);
        assert_eq!(fs.remove_file("/a").unwrap_err(), FsError::NotAFile);
        let names: Vec<_> = fs.read_dir("/a").unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
        assert_eq!(names, [("b".into(), true)]);
        fs.remove_file("/a/b/f.txt").unwrap();
        fs.remove_dir("/a/b").unwrap();
        fs.remove_dir("/a").unwrap();
        assert!(!fs.exists("/a"));
        let st = fs.stats().unwrap();
        assert_eq!(st.free_blocks, if kind == FatKind::Fat32 { st.total_blocks - 1 } else { st.total_blocks });
    }
}

#[test]
fn directory_grows_past_one_cluster() {
    for (kind, mib) in [(FatKind::Fat16, 64), (FatKind::Fat32, 64)] {
        let (ram, fs) = make(kind, mib);
        fs.create_dir("/d").unwrap();
        // 600 entries * (1 LFN + 1 short) * 32 B is far more than one cluster.
        for i in 0..600 {
            fs.write_file(&std::format!("/d/some long file name {i:04}.dat"), &[i as u8]).unwrap();
        }
        drop(fs);
        let fs = Vfat::mount(ram, clock).unwrap();
        assert_eq!(fs.read_dir("/d").unwrap().len(), 600);
        assert_eq!(fs.read_file("/d/some long file name 0599.dat").unwrap(), [(599 % 256) as u8]);
        for i in (0..600).step_by(2) {
            fs.remove_file(&std::format!("/d/some long file name {i:04}.dat")).unwrap();
        }
        assert_eq!(fs.read_dir("/d").unwrap().len(), 300);
        // Freed slots are reused rather than growing again.
        let g = fs.geometry();
        let _ = g;
        fs.write_file("/d/again.txt", b"1").unwrap();
        assert_eq!(fs.read_dir("/d").unwrap().len(), 301);
    }
}

#[test]
fn fixed_root_fills_up() {
    let (_, fs) = make(FatKind::Fat16, 64);
    let mut created = 0;
    let err = loop {
        match fs.write_file(&std::format!("/f{created:05}"), b"") {
            Ok(()) => created += 1,
            Err(e) => break e,
        }
    };
    assert_eq!(err, FsError::NoSpace);
    assert_eq!(created, 512, "512 root slots, one per short-only name");
}

#[test]
fn rename_cases() {
    for (kind, mib) in KINDS {
        let (ram, fs) = make(kind, mib);
        fs.create_dir("/src").unwrap();
        fs.create_dir("/dst").unwrap();
        fs.write_file("/src/a.txt", b"AAA").unwrap();
        fs.write_file("/dst/b.txt", b"BBB").unwrap();
        // Plain move + rename to a long name.
        fs.rename("/src/a.txt", "/dst/A Much Longer Name.txt").unwrap();
        assert!(!fs.exists("/src/a.txt"));
        assert_eq!(fs.read_file("/dst/a much longer name.TXT").unwrap(), b"AAA");
        // Replace an existing file.
        fs.rename("/dst/A Much Longer Name.txt", "/dst/b.txt").unwrap();
        assert_eq!(fs.read_file("/dst/b.txt").unwrap(), b"AAA");
        assert_eq!(fs.read_dir("/dst").unwrap().len(), 1);
        // Case-only rename.
        fs.rename("/dst/b.txt", "/dst/B.TXT").unwrap();
        assert_eq!(fs.read_dir("/dst").unwrap()[0].name, "B.TXT");
        // Directory move updates `..` and survives remount.
        fs.create_dir("/src/sub").unwrap();
        fs.write_file("/src/sub/x", b"x").unwrap();
        fs.rename("/src/sub", "/dst/sub").unwrap();
        assert_eq!(fs.read_file("/dst/sub/x").unwrap(), b"x");
        // Into itself is refused; onto a non-empty directory is refused.
        assert_eq!(fs.rename("/dst", "/dst/sub/inner").unwrap_err(), FsError::InvalidPath);
        assert_eq!(fs.rename("/src", "/dst").unwrap_err(), FsError::DirectoryNotEmpty);
        assert_eq!(fs.rename("/dst/B.TXT", "/dst/sub").unwrap_err(), FsError::NotAFile);
        assert_eq!(fs.rename("/nonexistent", "/z").unwrap_err(), FsError::NotFound);
        drop(fs);
        let fs = Vfat::mount(ram, clock).unwrap();
        assert_eq!(fs.read_file("/dst/sub/x").unwrap(), b"x", "{kind:?}");
    }
}

#[test]
fn read_only_attribute() {
    let (_, fs) = make(FatKind::Fat16, 64);
    fs.write_file("/ro.txt", b"keep").unwrap();
    fs.chmod("/ro.txt", 0o444).unwrap();
    assert_eq!(fs.metadata("/ro.txt").unwrap().mode, 0o100_444);
    assert_eq!(fs.write_file("/ro.txt", b"x").unwrap_err(), FsError::PermissionDenied);
    assert_eq!(fs.remove_file("/ro.txt").unwrap_err(), FsError::PermissionDenied);
    assert_eq!(fs.truncate("/ro.txt", 0).unwrap_err(), FsError::PermissionDenied);
    fs.chmod("/ro.txt", 0o644).unwrap();
    fs.write_file("/ro.txt", b"ok").unwrap();
}

#[test]
fn set_times_round_trips() {
    let (_, fs) = make(FatKind::Fat32, 64);
    fs.write_file("/t", b"x").unwrap();
    fs.set_times("/t", Some(1_000_000_000), Some(1_100_000_000)).unwrap();
    let m = fs.metadata("/t").unwrap();
    assert_eq!(m.modified, Some(1_100_000_000));
    assert_eq!(m.accessed, Some(1_000_000_000 / 86_400 * 86_400));
}

#[test]
fn no_space_is_clean() {
    let (_, fs) = make(FatKind::Fat12, 1);
    let before = fs.stats().unwrap().free_blocks;
    let huge = vec![1u8; 2 << 20];
    assert_eq!(fs.write_file("/huge", &huge).unwrap_err(), FsError::NoSpace);
    fs.remove_file("/huge").ok();
    assert_eq!(fs.stats().unwrap().free_blocks, before, "failed write leaked clusters");
}

#[test]
fn invalid_paths() {
    let (_, fs) = make(FatKind::Fat16, 64);
    assert_eq!(fs.write_file("/a:b", b"").unwrap_err(), FsError::InvalidPath);
    assert_eq!(fs.write_file("/", b"").unwrap_err(), FsError::InvalidPath);
    assert_eq!(fs.read_file("/missing").unwrap_err(), FsError::NotFound);
    assert_eq!(fs.read_dir("/missing").unwrap_err(), FsError::NotFound);
    assert_eq!(fs.read_file("/").unwrap_err(), FsError::NotAFile);
}

#[test]
fn fat12_odd_and_even_entries_do_not_disturb_neighbours() {
    // Allocate many single-cluster files and free alternate ones: every 12-bit
    // entry shares a byte with its neighbour.
    let (_, fs) = make(FatKind::Fat12, 4);
    let cb = fs.geometry().cluster_bytes as usize;
    for i in 0..40 {
        fs.write_file(&std::format!("/f{i}"), &vec![i as u8; cb * 3 + 1]).unwrap();
    }
    for i in (0..40).step_by(2) {
        fs.remove_file(&std::format!("/f{i}")).unwrap();
    }
    for i in (1..40).step_by(2) {
        assert_eq!(fs.read_file(&std::format!("/f{i}")).unwrap(), vec![i as u8; cb * 3 + 1]);
    }
}
