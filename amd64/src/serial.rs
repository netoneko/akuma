//! 16550 UART on the legacy COM1 port block, via port I/O.
//!
//! This is the amd64 counterpart of `akuma-uart`, and it is a separate
//! implementation rather than a `cfg` arm of that crate on purpose: the two
//! share a register *layout* (the 16550 programming model) but nothing else.
//! AArch64 reaches its UART through an MMIO window the MMU has to have mapped;
//! x86 reaches this one through `in`/`out` on an I/O port, which needs no
//! mapping and works before paging is on. Merging them would mean an
//! abstraction over "how a byte reaches a register", which is the
//! `trait Arch` shape `REDUCING_PLATFORM_DEPENDENCY.md` §7 argues against.
//!
//! No heap, no formatting machinery, no `core::fmt` — same rule as the kernel's
//! `safe_print!`: the console is what survives when everything else is broken,
//! so it may not depend on anything that can fail.

/// COM1. Fixed by the PC architecture since the IBM PC; not discovered.
const COM1: u16 = 0x3F8;

const DATA: u16 = 0;
const INT_ENABLE: u16 = 1;
const FIFO_CTRL: u16 = 2;
const LINE_CTRL: u16 = 3;
const MODEM_CTRL: u16 = 4;
const LINE_STATUS: u16 = 5;

/// Scratch register: a byte the chip stores and hands back, and does nothing
/// else with. The probe register, for exactly that reason.
const SCRATCH: u16 = 7;

/// Line status bit 5: transmit holding register empty.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// Data Ready — a byte is waiting in the receive buffer.
const LSR_DATA_READY: u8 = 1 << 0;

use crate::port::{inb, outb};
use akuma_dmesg::Ring;
use core::sync::atomic::{AtomicBool, Ordering};
use spinning_top::Spinlock;
use spinning_top::guard::SpinlockGuard;

/// One writer at a time, per call. With several cores printing, two `puts`
/// interleaved byte by byte are unreadable; held per *call* rather than per
/// line, so a line assembled from `puts`/`put_hex`/`put_dec` can still be
/// interleaved with another core's — kernel code prints under the BKL and is
/// serialised anyway, and this covers the bring-up window where it is not.
///
/// Best-effort on purpose: a core that cannot get the lock within the budget
/// prints anyway. The console is what a crashed core reports through, and a
/// lock held by a core that died mid-line must not silence the report.
static LOCK: AtomicBool = AtomicBool::new(false);
const LOCK_BUDGET: u32 = 1 << 22;

/// A copy of the last [`KLOG_CAP`] console bytes, so `dmesg` works over ssh.
///
/// This target has no UART on the reference box and no `/proc/kmsg`, so once the
/// framebuffer scrolls a diagnostic away there is no way to get it back — which
/// on a headless box being driven entirely over ssh means "the kernel said why
/// and nobody can read it". Every byte that goes to [`putb_raw`] is also written
/// here, and `sys_syslog` (syscall 103) reads it back.
///
/// The wrap/skip arithmetic is [`akuma_dmesg::Ring`], shared with the AArch64
/// kernel's `console::dmesg_*` and host-tested — it started here as a
/// `static mut [u8; 64 * 1024]` with four raw-pointer helpers, and its single
/// pure-function bug (a staging buffer that silently became the ceiling on
/// `dmesg`) cost a reboot of the bare-metal box to find. What lives here is the
/// `static` and the lock.
///
/// 64 KiB covers a whole boot's worth of output comfortably and costs nothing
/// but `.bss`.
const KLOG_CAP: usize = 64 * 1024;

/// # Why its own lock, and why `try_lock` on the write path
///
/// [`putb_raw`] already runs under [`LOCK`], but that lock is *best-effort*: a
/// core that cannot take it within [`LOCK_BUDGET`] prints anyway, so two cores
/// can genuinely be inside `putb_raw` at once and the ring needs its own
/// serialisation. The acquire there is a single non-blocking attempt, for the
/// same reason the console lock is bounded — a panic or a fault landing while
/// this core is mid-`putb_raw` must not spin forever on a lock this core
/// itself holds. Losing a few bytes of *replay* beats losing the live output.
///
/// Readers ([`klog_snapshot_from`], [`klog_len`]) retry, bounded: a spurious
/// `0` from a transient contention would break `sys_syslog`'s drain loop and
/// truncate `dmesg` silently, which is precisely the failure this ring exists
/// to stop.
static KLOG: Spinlock<Ring<KLOG_CAP>> = Spinlock::new(Ring::new());

/// Bounded acquire of [`KLOG`] for a reader. `None` means "give up rather than
/// wedge"; callers report an empty/short answer, never a hang.
fn klog_lock() -> Option<SpinlockGuard<'static, Ring<KLOG_CAP>>> {
    let mut spins = 0u32;
    loop {
        if let Some(g) = KLOG.try_lock() {
            return Some(g);
        }
        spins += 1;
        if spins >= LOCK_BUDGET {
            return None;
        }
        core::hint::spin_loop();
    }
}

/// Append one byte to [`KLOG`]. Never blocks; see [`KLOG`].
fn klog_push(byte: u8) {
    if let Some(mut ring) = KLOG.try_lock() {
        ring.push(byte);
    }
}

/// Copy console history into `out`, starting `skip` bytes past the oldest byte
/// still retrievable. Returns how many bytes were written.
///
/// The `skip` is what lets a caller with a small staging buffer deliver the
/// *whole* ring, a chunk at a time. Without it, `sys_syslog`'s 4 KiB staging
/// buffer was also the hard ceiling on `dmesg`: `SIZE_BUFFER` advertised the
/// full 64 KiB, `dmesg` allocated that and asked for it, and got the last 4 KiB
/// back — fifteen sixteenths of the log unreachable, with nothing reporting a
/// short read. On a machine whose console is a television with no scrollback,
/// `dmesg` is the only way to read a boot, and it was silently truncating it to
/// the last few seconds: an xHCI bring-up printed its whole trace and the NIC's
/// stall dumps then pushed it out of reach.
///
/// `skip == 0` is the oldest retrievable byte, so successive calls walk forward
/// through history.
#[must_use]
pub fn klog_snapshot_from(skip: usize, out: &mut [u8]) -> usize {
    klog_lock().map_or(0, |r| r.snapshot_from(skip, out))
}

/// Total bytes currently retrievable from [`klog_snapshot_from`]. For
/// `SYSLOG_ACTION_SIZE_UNREAD` / `SIZE_BUFFER`.
#[must_use]
pub fn klog_len() -> usize {
    klog_lock().map_or(0, |r| r.len())
}

/// Discard the buffered console history. For `SYSLOG_ACTION_CLEAR`.
pub fn klog_clear() {
    if let Some(mut ring) = klog_lock() {
        ring.clear();
    }
}

/// Append a string to the `dmesg` ring **only** — not to the framebuffer or the
/// port. For a high-frequency diagnostic (the memory ticker) that belongs in
/// `dmesg` but must not scroll the television on a box being watched.
pub fn klog_only(s: &str) {
    if let Some(mut ring) = klog_lock() {
        ring.push_str_crlf(s);
    }
}

/// [`put_dec`] into the ring only. Companion to [`klog_only`].
pub fn klog_only_dec(mut val: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        if val == 0 {
            break;
        }
    }
    if let Some(mut ring) = klog_lock() {
        ring.push_bytes(&buf[i..]);
    }
}

/// Did a 16550 answer the probe in [`init`]?
///
/// **An absent x86 I/O port reads `0xFF`.** On a machine with no UART — the
/// bare-metal HP box, whose console is a framebuffer — polling `LSR` for
/// data-ready therefore says "a byte is waiting", forever, and the byte is
/// `0xFF`: a phantom keyboard typing an endless stream of one character. The
/// first `sh` on that machine filled its screen with replacement glyphs from a
/// stdin that never stopped (`docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md`,
/// "not a kernel bug at all"). Every read path checks this and reports "no
/// data" when nothing is there; the write path still mirrors to the
/// framebuffer and skips the port.
static PRESENT: AtomicBool = AtomicBool::new(false);

struct Guard;

fn lock() -> Guard {
    let mut spins = 0u32;
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spins += 1;
        if spins >= LOCK_BUDGET {
            break;
        }
        core::hint::spin_loop();
    }
    Guard
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

/// Configure COM1 for 115200 8N1 with FIFOs on.
///
/// Divisor 1 against the 115200 Hz base clock. QEMU does not care about the
/// baud rate, but a real machine does and a wrong divisor is invisible under
/// emulation — which is exactly the class of bug that only shows up on
/// hardware, so it is set correctly here rather than left at the default.
pub fn init() {
    // Probe first: write two patterns to the scratch register and read them
    // back. A real chip returns what it was given; an empty bus returns 0xFF
    // for both — and a value of 0xFF from a real chip for the *first* pattern
    // would be caught by the second. Only a chip that answers gets configured
    // or read from.
    // SAFETY: the scratch register is a plain storage byte with no side effect
    // on a 16550, and an absent port ignores writes.
    let present = unsafe {
        outb(COM1 + SCRATCH, 0x5A);
        let a = inb(COM1 + SCRATCH);
        outb(COM1 + SCRATCH, 0xA5);
        let b = inb(COM1 + SCRATCH);
        a == 0x5A && b == 0xA5
    };
    PRESENT.store(present, Ordering::Release);
    if !present {
        return;
    }
    // SAFETY: COM1's register block is fixed by the PC architecture.
    unsafe {
        outb(COM1 + INT_ENABLE, 0x00); // no interrupts; this driver polls
        outb(COM1 + LINE_CTRL, 0x80); // DLAB on: the next two ports are the divisor
        outb(COM1 + DATA, 0x01); // divisor low  = 1  -> 115200 baud
        outb(COM1 + INT_ENABLE, 0x00); // divisor high = 0
        outb(COM1 + LINE_CTRL, 0x03); // DLAB off, 8 bits, no parity, 1 stop
        outb(COM1 + FIFO_CTRL, 0xC7); // FIFO on, cleared, 14-byte trigger
        outb(COM1 + MODEM_CTRL, 0x0B); // DTR + RTS + OUT2
    }
}

/// Emit one byte, spinning until the transmit holding register drains.
///
/// Unbounded spin. On real hardware with no cable this blocks forever, which is
/// the wrong trade for a production console and the right one for a bring-up
/// console: a dropped byte during boot is a bug you cannot see, and a stall is
/// a bug you can.
pub fn putb(byte: u8) {
    let _g = lock();
    putb_raw(byte);
}

/// [`putb`] without the lock, for callers that hold it across a whole string.
fn putb_raw(byte: u8) {
    // Keep a copy for `dmesg` (see `KLOG`). First, so a byte survives even if
    // the mirror or the port below hangs.
    klog_push(byte);

    // Mirror to the framebuffer console, if one is up.
    //
    // FIRST, before the wait below. On the bare-metal target there is no UART at
    // all: `inb` on an absent port reads 0xFF, so `LSR_THR_EMPTY` appears set and
    // the loop happens to fall through -- but that is the I/O bus being
    // forgiving, not a guarantee. Drawing before the wait means a machine whose
    // absent port ever read 0 would still have said what it was about to hang
    // on.
    crate::multiboot2::mirror_byte(byte);

    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: COM1's register block is fixed by the PC architecture.
    unsafe {
        while inb(COM1 + LINE_STATUS) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        outb(COM1 + DATA, byte);
    }
}

/// Did [`init`] find a UART? `false` means every read reports no data.
#[must_use]
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Emit a string, translating `\n` to CRLF.
/// Take a byte from the receive buffer, or `None` if none is waiting.
///
/// Polled, like the transmit side: this target takes no device interrupts, so
/// there is no IRQ to arrive and no buffer for one to fill. A caller that wants
/// to block spins on this — which is honest here, because there is nothing else
/// for the CPU to do while a shell waits for a key.
///
/// Non-blocking rather than blocking as the primitive, so a `read` with no data
/// can return `EAGAIN` and a scheduler can be added later without changing the
/// device layer.
#[must_use]
pub fn getb() -> Option<u8> {
    if !PRESENT.load(Ordering::Relaxed) {
        return None; // no chip: no data, not an endless 0xFF
    }
    // SAFETY: two reads of the 16550's own port range, which `init` configured.
    unsafe {
        if inb(COM1 + LINE_STATUS) & LSR_DATA_READY == 0 {
            return None;
        }
        Some(inb(COM1 + DATA))
    }
}

/// Is a byte waiting on the receive side? Non-destructive — for `poll(2)` on a
/// console fd, where consuming the byte would lose it before the following
/// `read`.
#[must_use]
pub fn has_byte() -> bool {
    if !PRESENT.load(Ordering::Relaxed) {
        return false;
    }
    // SAFETY: one read of the 16550's line-status port, configured by `init`.
    unsafe { inb(COM1 + LINE_STATUS) & LSR_DATA_READY != 0 }
}

pub fn puts(s: &str) {
    let _g = lock();
    for &b in s.as_bytes() {
        if b == b'\n' {
            putb_raw(b'\r');
        }
        putb_raw(b);
    }
}

/// Emit a `u64` as zero-padded 16-digit hex.
///
/// Fixed width rather than trimmed: leading zeros make addresses line up in a
/// boot log, and a variable-width printer needs a branch this does not.
pub fn put_hex(mut val: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 16];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(val & 0xF) as usize];
        val >>= 4;
    }
    let _g = lock();
    for &b in &out {
        putb_raw(b);
    }
}

/// Emit the low `nibbles` hex digits of `val`, zero-padded, no `0x`.
///
/// For fields that are not addresses — a PCI class byte, a 16-bit vendor id, a
/// MAC octet — where the full 16-digit [`put_hex`] is noise.
pub fn put_hexn(val: u64, nibbles: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let n = nibbles.clamp(1, 16);
    let _g = lock();
    for shift in (0..n).rev() {
        putb_raw(DIGITS[((val >> (shift * 4)) & 0xF) as usize]);
    }
}

/// Emit a `u64` in decimal.
pub fn put_dec(val: u64) {
    if val == 0 {
        putb(b'0');
        return;
    }
    // 20 digits is the width of u64::MAX; the buffer can never overflow.
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _g = lock();
    for &b in &buf[i..] {
        putb_raw(b);
    }
}
