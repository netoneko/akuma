//! `klogd` — keep the kernel log on disk while the box is up.
//!
//! Every 5 s: `dmesg` (the `syslog(2)` ring) to `$DIR/klog-N.dmesg`, written to
//! a temp file and renamed so a hang mid-write leaves the previous complete
//! copy; the `[herd]` lifecycle lines to `$DIR/herd-N.log`, append-only
//! across ring wraps (see the library half); and every new line of the ring
//! appended to `$DIR/klog-N.all`, the whole boot's log (2026-10-09: the
//! 64 KiB ring had lost both metal browser deaths by the time anyone read it). `N` is `$DIR/count` + 1, the same
//! boot counter the other ryzen services use.
//!
//! Replaced `overlays/ryzen/rootfs/etc/ryzen/klog.sh` (deleted). Allocation: one 128 KiB
//! ring buffer, a second one for the previous snapshot, and the merged herd
//! text (capped at 256 KiB), allocated once;
//! per pass only the filtered snapshot (a few KiB) and two path strings.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code))]

extern crate alloc;

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use klogd::merge;

use libakuma::{fs, mkdir_p, open, open_flags, print, rename, sleep_ms, syscall, write_fd};

const DIR: &str = "/var/log/ryzen";
/// 2 s, not 5: a burst (a crash dump, `strace_err` under a browser) can
/// write more than the 64 KiB ring between two passes, and `klog-N.all`
/// then carries a gap marker where the lines were lost.
const PERIOD_MS: u64 = 2000;
/// Stop appending to `klog-N.all` past this; p3 has room, but a log storm
/// must not fill the root filesystem of a box that stays up for hours.
const ALL_CAP: u64 = 256 * 1024 * 1024;
/// Kernel ring is 64 KiB; twice that is slack for a larger one.
const RING_BUF: usize = 128 * 1024;

#[cfg(target_arch = "x86_64")]
const SYS_SYSLOG: u64 = 103;
#[cfg(target_arch = "aarch64")]
const SYS_SYSLOG: u64 = 116;
#[cfg(target_arch = "x86_64")]
const SYS_SYNC: u64 = 162;
#[cfg(target_arch = "aarch64")]
const SYS_SYNC: u64 = 81;
/// `SYSLOG_ACTION_READ_ALL`.
const READ_ALL: u64 = 3;

/// Replace `path` with `data` via `path.tmp`, so a reader never sees half of it.
fn put(path: &str, data: &[u8]) {
    let tmp = format!("{path}.tmp");
    if fs::write(&tmp, data).is_ok() {
        rename(&tmp, path);
    }
}

/// All of `data` to `fd`, counting what landed.
fn append(fd: i32, data: &[u8], len: &mut u64) {
    let mut pos = 0;
    while pos < data.len() {
        let n = write_fd(fd, &data[pos..]);
        if n <= 0 {
            break;
        }
        pos += n as usize;
    }
    *len += pos as u64;
}

#[no_mangle]
pub extern "C" fn main() {
    print("[klogd] rev ");
    print(libakuma::GIT_REV);
    print("\n");

    mkdir_p(DIR);
    let count_path = format!("{DIR}/count");
    let prev: u64 = fs::read_to_string(&count_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let n = prev + 1;
    put(&count_path, format!("{n}\n").as_bytes());

    let klog = format!("{DIR}/klog-{n}.dmesg");
    let herd = format!("{DIR}/herd-{n}.log");
    // The whole boot's log, append-only: the snapshot above is only the
    // ring's last 64 KiB, which a death followed by two minutes of anything
    // else no longer contains. Opened once and kept open.
    let all = open(
        &format!("{DIR}/klog-{n}.all"),
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND,
    );
    let mut all_len: u64 = 0;
    let mut ring = vec![0u8; RING_BUF];
    // The previous pass's snapshot, swapped with `ring` each pass; together
    // the only per-boot buffers besides `acc`.
    let mut prev = vec![0u8; RING_BUF];
    let mut prev_len = 0usize;
    let mut acc: Vec<u8> = Vec::new();

    loop {
        let got = syscall(SYS_SYSLOG, READ_ALL, ring.as_mut_ptr() as u64, ring.len() as u64, 0, 0, 0) as i64;
        if got > 0 {
            let snap = &ring[..got as usize];
            put(&klog, snap);
            if all >= 0 && all_len < ALL_CAP {
                let (tail, gap) = klogd::new_tail(&prev[..prev_len], snap);
                if gap {
                    append(all, klogd::GAP, &mut all_len);
                }
                append(all, tail, &mut all_len);
            }
            let mut cur: Vec<u8> = Vec::new();
            klogd::herd_lines(snap, &mut cur);
            if merge(&mut acc, &cur) {
                put(&herd, &acc);
            }
            syscall(SYS_SYNC, 0, 0, 0, 0, 0, 0);
            prev_len = got as usize;
            core::mem::swap(&mut ring, &mut prev);
        }
        sleep_ms(PERIOD_MS);
    }
}
