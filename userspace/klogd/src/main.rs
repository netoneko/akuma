//! `klogd` — keep the kernel log on disk while the box is up.
//!
//! Every 5 s: `dmesg` (the `syslog(2)` ring) to `$DIR/klog-N.dmesg`, written to
//! a temp file and renamed so a hang mid-write leaves the previous complete
//! copy; and the `[herd]` lifecycle lines to `$DIR/herd-N.log`, append-only
//! across ring wraps (see the library half). `N` is `$DIR/count` + 1, the same
//! boot counter the other ryzen services use.
//!
//! Replaced `overlays/ryzen/rootfs/etc/ryzen/klog.sh` (deleted). Allocation: one 128 KiB
//! ring buffer and the merged herd text (capped at 256 KiB), allocated once;
//! per pass only the filtered snapshot (a few KiB) and two path strings.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code))]

extern crate alloc;

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use klogd::merge;

use libakuma::{fs, mkdir_p, print, rename, sleep_ms, syscall};

const DIR: &str = "/var/log/ryzen";
const PERIOD_MS: u64 = 5000;
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
    let mut ring = vec![0u8; RING_BUF];
    let mut acc: Vec<u8> = Vec::new();

    loop {
        let got = syscall(SYS_SYSLOG, READ_ALL, ring.as_mut_ptr() as u64, ring.len() as u64, 0, 0, 0) as i64;
        if got > 0 {
            let snap = &ring[..got as usize];
            put(&klog, snap);
            let mut cur: Vec<u8> = Vec::new();
            klogd::herd_lines(snap, &mut cur);
            if merge(&mut acc, &cur) {
                put(&herd, &acc);
            }
            syscall(SYS_SYNC, 0, 0, 0, 0, 0, 0);
        }
        sleep_ms(PERIOD_MS);
    }
}
