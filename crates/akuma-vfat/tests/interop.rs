//! Black-box interop with the host's `mkfs.vfat` and `fsck.vfat`.
//!
//! Each test skips (passes, printing why) when the tool is not installed, so
//! the suite stays runnable anywhere. When they are present these are the
//! tests that matter: our own formatter and our own reader agreeing with each
//! other proves little; `fsck.vfat -n` accepting what we wrote, and us reading
//! what `mkfs.vfat` made, does.

use akuma_vfat::{BlockDevice, FatKind, FormatOptions, Vfat, format};
use akuma_vfs::Filesystem;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

struct FileDev(Mutex<File>);

impl BlockDevice for FileDev {
    fn read_bytes(&self, off: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.0.lock().unwrap().read_exact_at(buf, off).map_err(|_| ())
    }
    fn write_bytes(&self, off: u64, data: &[u8]) -> Result<(), ()> {
        self.0.lock().unwrap().write_all_at(data, off).map_err(|_| ())
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--help").output().is_ok()
}

fn image(name: &str, mib: u64) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let f = File::create(&p).unwrap();
    f.set_len(mib << 20).unwrap();
    p
}

fn open(p: &PathBuf) -> FileDev {
    FileDev(Mutex::new(OpenOptions::new().read(true).write(true).open(p).unwrap()))
}

fn fsck(p: &PathBuf) {
    let out = Command::new("fsck.vfat").arg("-n").arg(p).output().unwrap();
    assert!(
        out.status.success(),
        "fsck.vfat rejected the image:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn clock() -> u64 {
    1_700_000_000
}

/// A workout that touches long names, directory growth, rename, truncate,
/// delete and large files.
fn workout(fs: &Vfat<FileDev>) {
    fs.create_dir("/EFI").unwrap();
    fs.create_dir("/EFI/akuma").unwrap();
    fs.write_file("/EFI/akuma/grub.cfg", b"set timeout=5\nmenuentry 'Akuma' {}\n").unwrap();
    fs.write_file("/Long File Name With Spaces.txt", b"long").unwrap();
    fs.write_file("/MiXeD.CaSe", b"mixed").unwrap();
    for i in 0..300 {
        fs.write_file(&format!("/EFI/akuma/module number {i:03}.mod"), &[i as u8; 700]).unwrap();
    }
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i >> 3) as u8).collect();
    fs.write_file("/EFI/kernel.bin", &big).unwrap();
    fs.write_file("/EFI/akuma/grub.cfg", b"short\n").unwrap();
    fs.truncate("/EFI/kernel.bin", 1_000_000).unwrap();
    fs.rename("/Long File Name With Spaces.txt", "/EFI/Renamed Long Name.txt").unwrap();
    fs.create_dir("/moved").unwrap();
    fs.rename("/EFI/akuma", "/moved/akuma").unwrap();
    for i in (0..300).step_by(3) {
        fs.remove_file(&format!("/moved/akuma/module number {i:03}.mod")).unwrap();
    }
    fs.create_dir("/gone").unwrap();
    fs.remove_dir("/gone").unwrap();
    fs.sync().unwrap();
}

#[test]
fn fsck_accepts_what_we_write() {
    if !have("fsck.vfat") {
        eprintln!("skip: fsck.vfat not installed");
        return;
    }
    for (kind, mib, name) in [
        (FatKind::Fat12, 8, "fsck12.img"),
        (FatKind::Fat16, 64, "fsck16.img"),
        (FatKind::Fat32, 128, "fsck32.img"),
    ] {
        let p = image(name, mib);
        let dev = open(&p);
        format(&dev, mib << 20, &FormatOptions { kind: Some(kind), ..Default::default() }).unwrap();
        fsck(&p); // our mkfs, untouched
        let fs = Vfat::mount(dev, clock).unwrap();
        if kind == FatKind::Fat12 {
            // 8 MiB of FAT12: skip the 3 MB file, keep the rest.
            fs.create_dir("/EFI").unwrap();
            fs.create_dir("/EFI/akuma").unwrap();
            fs.write_file("/EFI/akuma/grub.cfg", b"x\n").unwrap();
            fs.write_file("/Long Name.txt", b"y").unwrap();
            for i in 0..50 {
                fs.write_file(&format!("/EFI/akuma/module number {i:03}.mod"), &[1u8; 700]).unwrap();
            }
            fs.rename("/Long Name.txt", "/EFI/Other Long Name.txt").unwrap();
            fs.sync().unwrap();
        } else {
            workout(&fs);
        }
        drop(fs);
        fsck(&p);
    }
}

#[test]
fn we_read_and_edit_a_volume_made_by_mkfs() {
    if !have("mkfs.vfat") || !have("fsck.vfat") {
        eprintln!("skip: dosfstools not installed");
        return;
    }
    for (flag, mib, name) in [("12", 8, "mk12.img"), ("16", 64, "mk16.img"), ("32", 128, "mk32.img")] {
        let p = image(name, mib);
        let st = Command::new("mkfs.vfat").args(["-F", flag, "-n", "AKUMA"]).arg(&p).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        let fs = Vfat::mount(open(&p), clock).unwrap();
        assert!(fs.read_dir("/").unwrap().is_empty());
        let free = fs.stats().unwrap().free_blocks;
        assert!(free > 0);
        workout_small(&fs);
        drop(fs);
        fsck(&p);
    }
}

fn workout_small(fs: &Vfat<FileDev>) {
    fs.create_dir("/boot").unwrap();
    fs.create_dir("/boot/grub").unwrap();
    fs.write_file("/boot/grub/grub.cfg", b"set default=0\n").unwrap();
    fs.write_file("/boot/grub/a really quite long file name.cfg", &[7u8; 5000]).unwrap();
    let mut v = fs.read_file("/boot/grub/grub.cfg").unwrap();
    v.extend_from_slice(b"set timeout=3\n");
    fs.write_file("/boot/grub/grub.cfg", &v).unwrap();
    assert_eq!(fs.read_file("/boot/grub/grub.cfg").unwrap(), b"set default=0\nset timeout=3\n");
    fs.rename("/boot/grub/grub.cfg", "/boot/grub/grub.cfg.bak").unwrap();
    fs.sync().unwrap();
}

#[test]
fn mkfs_vfat_files_written_by_mtools_style_layout_are_readable() {
    // `mkfs.vfat` has no way to put files in, but `mcopy` does; use it if present.
    if !have("mkfs.vfat") || Command::new("mcopy").arg("--version").output().is_err() {
        eprintln!("skip: mtools not installed");
        return;
    }
    let p = image("mtools.img", 64);
    assert!(Command::new("mkfs.vfat").args(["-F", "16"]).arg(&p).status().unwrap().success());
    let src = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mtools-src.txt");
    std::fs::write(&src, b"from mtools").unwrap();
    let ok = Command::new("mmd").arg("-i").arg(&p).arg("::/Some Directory").status().unwrap().success()
        && Command::new("mcopy").arg("-i").arg(&p).arg(&src).arg("::/Some Directory/A Long File Name.txt").status().unwrap().success()
        && Command::new("mcopy").arg("-i").arg(&p).arg(&src).arg("::/short.txt").status().unwrap().success();
    assert!(ok);
    let fs = Vfat::mount(open(&p), clock).unwrap();
    assert_eq!(fs.read_file("/Some Directory/A Long File Name.txt").unwrap(), b"from mtools");
    assert_eq!(fs.read_file("/short.txt").unwrap(), b"from mtools");
    let names: Vec<_> = fs.read_dir("/Some Directory").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["A Long File Name.txt"]);
}
