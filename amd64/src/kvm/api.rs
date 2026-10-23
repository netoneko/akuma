//! `/dev/kvm`, census stub (phase 1 of `docs/archive/AKUMA_FIRECRACKER_KVM.md`).
//!
//! With the `kvmapi` boot flag, `open("/dev/kvm")` hands out a descriptor whose every `ioctl` is
//! logged — request code decoded into direction / size / type / number, and the argument — and
//! answered from [`answer`]. It exists to learn, by watching the unmodified `firecracker`, which
//! calls it makes and in what order; it models nothing. No allocation: every line is printed straight
//! to the serial port.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::fd::errno;
use crate::serial;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU32 = AtomicU32::new(0);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
    if on {
        serial::puts("[kvmapi] census stub armed: /dev/kvm\n");
    }
}

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// What the stub says to `req` (`arg` is the raw third argument). `ENOTTY` is "no such request".
fn new_fd() -> u64 {
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return errno::ESRCH;
    };
    u64::from(proc.alloc_fd(akuma_exec_core::process::FileDescriptor::DevKvm))
}

fn answer(req: u32, arg: u64) -> u64 {
    match req {
        0xae00 => 12,
        0xae03 => 1,
        // SET_MSRS: the count set (all of them).
        0x4008_ae89 => crate::uaccess::read_val::<u32>(arg).map_or(errno::EFAULT, u64::from),
        0xae01 | 0xae41 => new_fd(),
        0xae04 => 0x2000,
        0xc008_ae05 => supported_cpuid(arg),
        _ => 0,
    }
}

/// Guessed stride of one CPUID entry in the array after the 8-byte header: seven words and three of
/// padding. Confirmed (or refuted) by what the VMM sends back with `set cpuid`.
const CPUID_ENTRY: u64 = 40;
/// Upper bound on entries offered.
const MAX_LEAVES: usize = 128;

/// The (leaf, subleaf) pairs offered: every basic and extended leaf the host has, with the subleaves
/// of the leaves that have them. Values are the host's, through the guest filter.
fn leaves(out: &mut [(u32, u32); MAX_LEAVES]) -> usize {
    use core::arch::x86_64::__cpuid_count as cpuid;
    let mut n = 0;
    let mut push = |l: u32, s: u32| {
        if n < MAX_LEAVES {
            out[n] = (l, s);
            n += 1;
        }
    };
    let basic = cpuid(0, 0).eax.min(0x20);
    for l in 0..=basic {
        let subs = match l {
            4 => 5,
            7 => cpuid(7, 0).eax.min(2) + 1,
            0xb => 3,
            0xd => 3,
            _ => 1,
        };
        for s in 0..subs {
            push(l, s);
        }
    }
    let ext = cpuid(0x8000_0000, 0).eax.min(0x8000_0021);
    for l in 0x8000_0000..=ext {
        let subs = if l == 0x8000_001d { 5 } else { 1 };
        for s in 0..subs {
            push(l, s);
        }
    }
    n
}

fn supported_cpuid(arg: u64) -> u64 {
    let Some(nent) = crate::uaccess::read_val::<u32>(arg) else {
        return errno::EFAULT;
    };
    let mut table = [(0u32, 0u32); MAX_LEAVES];
    let n = leaves(&mut table);
    if (nent as usize) < n {
        let _ = crate::uaccess::write_val::<u32>(arg, n as u32);
        return (-7i64) as u64; // E2BIG
    }
    for (i, &(leaf, sub)) in table[..n].iter().enumerate() {
        let r = core::arch::x86_64::__cpuid_count(leaf, sub);
        let f = akuma_svm_amd64::cpuid::filter(leaf, [r.eax, r.ebx, r.ecx, r.edx]);
        let mut e = [0u32; 10];
        e[0] = leaf;
        e[1] = sub;
        e[3..7].copy_from_slice(&f);
        let mut bytes = [0u8; 40];
        for (j, w) in e.iter().enumerate() {
            bytes[j * 4..j * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        if !crate::uaccess::write_bytes(arg + 8 + i as u64 * CPUID_ENTRY, &bytes) {
            return errno::EFAULT;
        }
    }
    let _ = crate::uaccess::write_val::<u32>(arg, n as u32);
    0
}

/// `ioctl(fd, req, arg)` on a `/dev/kvm` descriptor.
pub fn ioctl(fd: u64, req: u32, arg: u64) -> u64 {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let size = ((req >> 16) & 0x3fff) as usize;
    let pre = size > 0 && req >> 30 & 1 != 0;
    if pre {
        serial::puts("[kvmapi]   in: ");
        dump(arg, var_len(arg, req).unwrap_or(size.min(288)));
    }
    let r = answer(req, arg);
    serial::puts("[kvmapi] #");
    serial::put_dec(u64::from(n));
    serial::puts(" fd=");
    serial::put_dec(fd);
    serial::puts(" req=0x");
    serial::put_hexn(u64::from(req), 8);
    serial::puts(" dir=");
    serial::put_dec(u64::from(req >> 30));
    serial::puts(" size=");
    serial::put_dec(u64::from((req >> 16) & 0x3fff));
    serial::puts(" type=0x");
    serial::put_hexn(u64::from((req >> 8) & 0xff), 2);
    serial::puts(" nr=0x");
    serial::put_hexn(u64::from(req & 0xff), 2);
    serial::puts(" arg=0x");
    serial::put_hexn(arg, 16);
    serial::puts(" -> ");
    if (r as i64) < 0 {
        serial::puts("-");
        serial::put_dec((r as i64).unsigned_abs());
    } else {
        serial::put_dec(r);
    }
    serial::puts("\n");
    if size > 0 && req >> 30 & 2 != 0 {
        serial::puts("[kvmapi]   out: ");
        dump(arg, var_len(arg, req).unwrap_or(size.min(288)));
    }
    r
}

/// The bytes of the argument structure (first `n`), eight words per line.
fn dump(arg: u64, n: usize) {
    let mut b = [0u8; 288];
    if !crate::uaccess::read_bytes(arg, &mut b[..n]) {
        serial::puts("(arg unreadable)\n");
        return;
    }
    for (i, c) in b[..n].iter().enumerate() {
        serial::put_hexn(u64::from(*c), 2);
        if i % 4 == 3 {
            serial::puts(" ");
        }
    }
    serial::puts("\n");
}

/// Log an `mmap` of a `/dev/kvm` descriptor (fd, address hint, length, prot, flags, offset).
pub fn note_mmap(fd: u64, addr: u64, len: u64, prot: u64, flags: u64, off: u64) {
    serial::puts("[kvmapi] mmap fd=");
    serial::put_dec(fd);
    serial::puts(" addr=0x");
    serial::put_hexn(addr, 16);
    serial::puts(" len=0x");
    serial::put_hexn(len, 8);
    serial::puts(" prot=");
    serial::put_dec(prot);
    serial::puts(" flags=0x");
    serial::put_hexn(flags, 4);
    serial::puts(" off=0x");
    serial::put_hexn(off, 8);
    serial::puts("\n");
}

/// How much of a variable-length argument to dump: the 8-byte header plus up to six entries.
fn var_len(arg: u64, req: u32) -> Option<usize> {
    let (stride, _) = match req & 0xff00_ffff {
        0x4000_ae90 | 0xc000_ae05 | 0x4000_ae91 => (40usize, 0),
        0x4000_ae89 | 0xc000_ae88 => (16, 0),
        _ => return None,
    };
    let n = crate::uaccess::read_val::<u32>(arg)? as usize;
    Some(8 + stride * n.min(6))
}
