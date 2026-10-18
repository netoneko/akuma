#![forbid(unsafe_code)]
#![no_std]
//! The GDT and TSS, as data.
//!
//! `amd64/src/gdt.rs` owns the per-core `static`s and the two instructions that
//! load them (`lgdt`, `ltr`) — the only genuinely `unsafe` content. Everything
//! else in that file was arithmetic: which selector is which, how a 16-byte TSS
//! descriptor packs a 64-bit base into three scattered fields, and the constraint
//! `syscall`/`sysret` put on the *order* of the rows. That arithmetic is here, so
//! it is checked at compile time and on the host instead of by the first
//! transition into ring 3.
//!
//! # Why the order of the rows is not free
//!
//! `sysret` does not take selectors; it *computes* them from `IA32_STAR[63:48]`,
//! loading `CS = base + 16` and `SS = base + 8`. `syscall` is the mirror image:
//! `CS = IA32_STAR[47:32]`, `SS = that + 8`. So user **data** must sit
//! immediately below user **code**, and kernel code immediately below kernel
//! data — constraints on *this table* that nothing checks when it is loaded. A
//! wrong order faults on the first transition, and on the metal rather than under
//! emulation (see [`SYSRET_BASE`] for the RPL half of that, which QEMU hides).
//! The `const` assertions below are those constraints, stated.

/// Entries in one core's table: null, kernel code, kernel data, user data, user
/// code, and the 16-byte TSS descriptor, which takes two.
pub const SLOTS: usize = 7;

const IDX_KERNEL_CODE: u16 = 1;
const IDX_KERNEL_DATA: u16 = 2;
const IDX_USER_DATA: u16 = 3;
const IDX_USER_CODE: u16 = 4;
const IDX_TSS: u16 = 5;

/// A selector's table-index field starts at bit 3 (bits 0-1 are RPL, bit 2 TI).
const fn selector(index: u16, rpl: u16) -> u16 {
    index << 3 | rpl
}

pub const KERNEL_CODE: u16 = selector(IDX_KERNEL_CODE, 0);
/// Kernel data.
///
/// Not referenced by name anywhere: `syscall` derives `SS` as
/// `IA32_STAR[47:32] + 8`, so the hardware computes this selector rather than
/// being handed it. Kept because the *value* is a constraint on the table.
pub const KERNEL_DATA: u16 = selector(IDX_KERNEL_DATA, 0);
pub const TSS_SELECTOR: u16 = selector(IDX_TSS, 0);

/// `IA32_STAR[63:48]`. `sysret` derives user CS/SS from it: `CS = base + 16`,
/// `SS = base + 8`.
///
/// **The `| 3` is the RPL and it is load-bearing.** With a bare `0x10` the
/// computed selectors are `CS = 0x20` and `SS = 0x18` — correct indices, but
/// `SS` carries RPL 0 while the CPU forces only `CS.RPL` to 3. `iretq` back to
/// ring 3 then rejects the frame with `#GP(0x18)`, because it requires
/// `SS.RPL == CS.RPL`.
///
/// Emulation hides this: QEMU forces RPL 3 onto both selectors, so `0x10` works
/// there and faults on real AMD hardware at the first interrupt taken in ring 3.
/// Measured 2026-09-04 on a Ryzen 7 8845HS under Firecracker; the IRET frame read
/// `cs=0x23 ss=0x18`, which is what named the bug.
pub const SYSRET_BASE: u16 = IDX_KERNEL_DATA << 3 | 3;

/// The user code selector `sysret` produces: `0x23`.
pub const USER_CS: u16 = SYSRET_BASE + 16;
/// The user data/stack selector `sysret` produces: `0x1b`.
pub const USER_SS: u16 = SYSRET_BASE + 8;

// `sysret` computes these, and nothing at load time checks that they land on the
// rows this table actually has.
const _: () = assert!(USER_CS == selector(IDX_USER_CODE, 3), "sysret CS must name the user code row, RPL 3");
const _: () = assert!(USER_SS == selector(IDX_USER_DATA, 3), "sysret SS must name the user data row, RPL 3");
// `syscall` computes SS = CS + 8.
const _: () = assert!(KERNEL_DATA == KERNEL_CODE + 8, "syscall SS must be the row after kernel code");
// A 64-bit TSS descriptor is 16 bytes: two rows.
const _: () = assert!(IDX_TSS as usize + 2 == SLOTS, "the TSS descriptor must be the last two rows");

/// `IA32_STAR` for this table: `sysret` base in bits 63:48, `syscall` kernel CS
/// in 47:32. The low 32 bits are the legacy-mode EIP and are unused in long mode.
#[must_use]
pub const fn star() -> u64 {
    (SYSRET_BASE as u64) << 48 | (KERNEL_CODE as u64) << 32
}

/// Kernel code: 64-bit (L), DPL 0. Byte-identical to what `boot.s` installs, so
/// `CS` stays valid across the `lgdt` and nothing reloads it.
pub const D_KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
/// Kernel data, DPL 0. Byte-identical to `boot.s`, for the same reason.
pub const D_KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;
/// DPL 3 versions: access byte `0x9A|0x60` = `0xFA`, `0x92|0x60` = `0xF2`.
pub const D_USER_DATA: u64 = 0x00CF_F200_0000_FFFF;
pub const D_USER_CODE: u64 = 0x00AF_FA00_0000_FFFF;

/// 64-bit TSS. 104 bytes, and the only field this kernel sets is `rsp0`.
///
/// `rsp0` is the stack the CPU switches to when an interrupt or exception is
/// taken **while in ring 3**. Without it, the first ring-3 page fault pushes its
/// frame onto the user stack — or onto garbage — and escalates to a triple
/// fault. It is the single reason a TSS exists here; nothing uses the IST slots
/// yet.
#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// Past the end of the segment: no I/O permission bitmap, so ring 3 gets no
    /// port access at all. `in`/`out` from userspace raises `#GP`, which is what
    /// we want — the serial port is the kernel's.
    iomap_base: u16,
}

/// The architectural size of a 64-bit TSS.
pub const TSS_SIZE: usize = 104;
const _: () = assert!(core::mem::size_of::<Tss>() == TSS_SIZE);

impl Tss {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: TSS_SIZE as u16,
        }
    }

    /// Point the ring-3-trap stack at `rsp0`.
    pub fn set_rsp0(&mut self, rsp0: u64) {
        self.rsp0 = rsp0;
    }

    /// The ring-3-trap stack.
    #[must_use]
    pub fn rsp0(&self) -> u64 {
        self.rsp0
    }
}

impl Default for Tss {
    fn default() -> Self {
        Self::new()
    }
}

/// The 16-byte system descriptor for an available 64-bit TSS at `base`
/// (`type = 0x9`, present).
///
/// Unlike a segment descriptor the base is 64-bit and spills into the second
/// row, and within the first row it is scattered: bits 0-23 at 16, bits 24-31 at
/// 56. The limit is `TSS_SIZE - 1`.
#[must_use]
pub const fn tss_descriptor(base: u64) -> [u64; 2] {
    let limit = (TSS_SIZE - 1) as u64;
    let low = limit & 0xFFFF
        | (base & 0xFF_FFFF) << 16
        | 0x9 << 40
        | 1 << 47
        | (limit & 0xF_0000) << 32
        | ((base >> 24) & 0xFF) << 56;
    [low, base >> 32]
}

/// One core's complete table, with its TSS descriptor pointing at `tss_base`.
#[must_use]
pub const fn table(tss_base: u64) -> [u64; SLOTS] {
    let tss = tss_descriptor(tss_base);
    [0, D_KERNEL_CODE, D_KERNEL_DATA, D_USER_DATA, D_USER_CODE, tss[0], tss[1]]
}

/// The value `lgdt` takes as a limit for one table: bytes minus one.
pub const GDT_LIMIT: u16 = (SLOTS * 8 - 1) as u16;

#[cfg(test)]
mod tests;
