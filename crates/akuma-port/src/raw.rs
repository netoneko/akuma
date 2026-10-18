//! The four instructions. Everything `unsafe` in this crate is in this file.
//!
//! Three backends, chosen at compile time:
//!
//! * **hardware** (`x86_64`, not `cfg(test)`): `in`/`out`.
//! * **empty bus** (any other architecture, not `cfg(test)`): writes vanish and
//!   reads return all ones, which is what a real x86 bus returns for a port
//!   nothing decodes. This exists so the workspace builds and lints on an
//!   AArch64 host; nothing runs it.
//! * **fake** (`cfg(test)`, on any host): a per-thread script and log, so the
//!   device sequences are host-tested instead of being proven only by a boot —
//!   and so a test on an x86 host never executes a real `out`.

pub use imp::{inb, inl, outb, outl};

#[cfg(all(target_arch = "x86_64", not(test)))]
#[allow(unsafe_code)]
mod imp {
    // SAFETY (all four): `in`/`out` touch no Rust-visible memory (`nomem`),
    // use no stack and leave the flags alone. What they *do* is talk to a
    // device, and that obligation is discharged by the callers' types: no
    // function here is reachable from outside the crate, and every caller is a
    // method on a device type that owns the ports it names.
    #[inline]
    pub fn outb(port: u16, val: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") val,
                             options(nomem, nostack, preserves_flags));
        }
    }

    #[inline]
    pub fn inb(port: u16) -> u8 {
        let val: u8;
        unsafe {
            core::arch::asm!("in al, dx", out("al") val, in("dx") port,
                             options(nomem, nostack, preserves_flags));
        }
        val
    }

    #[inline]
    pub fn outl(port: u16, val: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") val,
                             options(nomem, nostack, preserves_flags));
        }
    }

    #[inline]
    pub fn inl(port: u16) -> u32 {
        let val: u32;
        unsafe {
            core::arch::asm!("in eax, dx", out("eax") val, in("dx") port,
                             options(nomem, nostack, preserves_flags));
        }
        val
    }
}

#[cfg(all(not(target_arch = "x86_64"), not(test)))]
mod imp {
    pub fn outb(_port: u16, _val: u8) {}
    pub fn inb(_port: u16) -> u8 {
        0xFF
    }
    pub fn outl(_port: u16, _val: u32) {}
    pub fn inl(_port: u16) -> u32 {
        0xFFFF_FFFF
    }
}

#[cfg(test)]
pub mod imp {
    extern crate std;

    use core::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::vec::Vec;

    /// One recorded access.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Op {
        pub write: bool,
        pub port: u16,
        /// Access width in bytes: 1 or 4.
        pub width: u8,
        pub value: u32,
    }

    #[derive(Default)]
    struct Bus {
        log: Vec<Op>,
        /// Values the next reads of a port return, oldest first. An empty queue
        /// reads as an empty bus: all ones.
        reads: HashMap<u16, VecDeque<u32>>,
    }

    std::thread_local! {
        static BUS: RefCell<Bus> = RefCell::new(Bus::default());
    }

    /// Start a test from a clean bus.
    pub fn reset() {
        BUS.with(|b| *b.borrow_mut() = Bus::default());
    }

    /// The next read of `port` returns `value` (after any already queued).
    pub fn queue_read(port: u16, value: u32) {
        BUS.with(|b| b.borrow_mut().reads.entry(port).or_default().push_back(value));
    }

    /// Everything accessed since the last [`reset`] or [`take_log`].
    pub fn take_log() -> Vec<Op> {
        BUS.with(|b| core::mem::take(&mut b.borrow_mut().log))
    }

    fn record_write(port: u16, width: u8, value: u32) {
        BUS.with(|b| b.borrow_mut().log.push(Op { write: true, port, width, value }));
    }

    fn record_read(port: u16, width: u8) -> u32 {
        BUS.with(|b| {
            let mut b = b.borrow_mut();
            let empty = if width == 1 { 0xFF } else { 0xFFFF_FFFF };
            let value = b.reads.get_mut(&port).and_then(VecDeque::pop_front).unwrap_or(empty);
            b.log.push(Op { write: false, port, width, value });
            value
        })
    }

    pub fn outb(port: u16, val: u8) {
        record_write(port, 1, u32::from(val));
    }
    pub fn inb(port: u16) -> u8 {
        record_read(port, 1) as u8
    }
    pub fn outl(port: u16, val: u32) {
        record_write(port, 4, val);
    }
    pub fn inl(port: u16) -> u32 {
        record_read(port, 4)
    }
}
