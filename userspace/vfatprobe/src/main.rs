//! vfatprobe: exercise `mount -t vfat` end to end from a guest.
//!
//! `vfatprobe <source> <target>` mounts the FAT volume, edits a GRUB-style config
//! the way an operator would (create, rewrite longer, append, rename, a long
//! name, a subdirectory), unmounts, mounts again and checks it all persisted,
//! then prints `VFATPROBE PASS` or `VFATPROBE FAIL: <why>`. Run it as `init=`
//! (`amd64/run-vfat.sh`) or from a shell.
//!
//! It needs no busybox, which the offline amd64 image does not have; on an image
//! that does, `mount -t vfat` is the same syscall.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::open_flags::{O_APPEND, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use libakuma::{arg, close, exit, mkdir, mount, open, print, read, reboot, read_dir, rename, rmdir, umount, unlink, write};

fn fail(why: &str) -> ! {
    print(&format!("VFATPROBE FAIL: {why}\n"));
    let _ = reboot();
    exit(1);
}

fn put(path: &str, flags: u32, data: &[u8]) {
    let fd = open(path, flags);
    if fd < 0 {
        fail(&format!("open {path} for writing: {fd}"));
    }
    let n = write(fd, data);
    if n != data.len() as isize {
        fail(&format!("write {path}: {n} of {}", data.len()));
    }
    close(fd);
}

fn get(path: &str) -> Vec<u8> {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        fail(&format!("open {path}: {fd}"));
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            fail(&format!("read {path}: {n}"));
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    out
}

fn expect(path: &str, want: &[u8]) {
    let got = get(path);
    if got != want {
        fail(&format!("{path}: read {} bytes, wanted {} (first diff differs)", got.len(), want.len()));
    }
    print(&format!("  ok  {path} ({} bytes)\n", want.len()));
}

fn names(dir: &str) -> Vec<String> {
    let Some(rd) = read_dir(dir) else { fail(&format!("read_dir {dir}")) };
    let mut v: Vec<String> = rd.map(|e| e.name).filter(|n| n != "." && n != "..").collect();
    v.sort();
    v
}

#[no_mangle]
pub extern "C" fn main() {
    let (Some(source), Some(target)) = (arg(1), arg(2)) else {
        print("usage: vfatprobe <source> <target>\n");
        exit(2);
    };
    print(&format!("vfatprobe: mount -t vfat {source} {target}\n"));
    let r = mount(source, target, "vfat");
    if r != 0 {
        fail(&format!("mount returned {r}"));
    }

    let p = |s: &str| format!("{target}/{s}");
    print(&format!("  root: {:?}\n", names(target)));

    // A GRUB-style tree, written the way `echo >` and an editor do.
    if mkdir(&p("EFI")) != 0 || mkdir(&p("EFI/akuma")) != 0 {
        fail("mkdir EFI/akuma");
    }
    let cfg = p("EFI/akuma/grub.cfg");
    put(&cfg, O_WRONLY | O_CREAT | O_TRUNC, b"set timeout=5\n");
    expect(&cfg, b"set timeout=5\n");

    // Rewrite with something longer than a cluster, as `sed -i` does.
    let long: String = (0..300).map(|i| format!("menuentry 'Akuma {i}' {{ linux /boot/akuma }}\n")).collect();
    put(&cfg, O_WRONLY | O_TRUNC, long.as_bytes());
    expect(&cfg, long.as_bytes());

    // Shrink again, then append.
    put(&cfg, O_WRONLY | O_TRUNC, b"set default=0\n");
    put(&cfg, O_WRONLY | O_APPEND, b"set timeout=3\n");
    expect(&cfg, b"set default=0\nset timeout=3\n");

    // Long and mixed-case names, rename, unlink.
    put(&p("Backup Of Grub Config.cfg"), O_WRONLY | O_CREAT | O_TRUNC, b"backup\n");
    if rename(&p("Backup Of Grub Config.cfg"), &p("EFI/akuma/grub.cfg.bak")) != 0 {
        fail("rename");
    }
    put(&p("Scratch.TXT"), O_WRONLY | O_CREAT | O_TRUNC, b"x");
    if unlink(&p("Scratch.TXT")) != 0 {
        fail("unlink");
    }
    if mkdir(&p("gone")) != 0 || rmdir(&p("gone")) != 0 {
        fail("mkdir/rmdir");
    }
    print(&format!("  EFI/akuma: {:?}\n", names(&p("EFI/akuma"))));

    // Everything must survive an unmount and a fresh mount.
    if umount(target) != 0 {
        fail("umount");
    }
    let r = mount(source, target, "vfat");
    if r != 0 {
        fail(&format!("remount returned {r}"));
    }
    expect(&cfg, b"set default=0\nset timeout=3\n");
    expect(&p("EFI/akuma/grub.cfg.bak"), b"backup\n");
    let listed = names(&p("EFI/akuma"));
    if listed != ["grub.cfg", "grub.cfg.bak"] {
        fail(&format!("EFI/akuma lists {listed:?}"));
    }
    if names(target) != ["EFI"] {
        fail("root should hold only EFI");
    }

    print("VFATPROBE PASS\n");
    let _ = reboot();
    exit(0);
}
