//! GDT and TSS: the descriptors ring 3 needs.
//!
//! `boot.s` builds a three-entry GDT — null, kernel code, kernel data — which is
//! everything long mode requires and nothing userspace does. This replaces it
//! with the full table.
//!
//! # Layout, and why the order is not free
//!
//! ```text
//! 0x00  null
//! 0x08  kernel code   (64-bit, DPL 0)
//! 0x10  kernel data   (DPL 0)
//! 0x18  user data     (DPL 3)
//! 0x20  user code     (64-bit, DPL 3)
//! 0x28  TSS           (system descriptor, 16 bytes — occupies 0x28 and 0x30)
//! ```
//!
//! `sysret` does not take selectors; it *computes* them from `IA32_STAR[63:48]`,
//! loading `CS = base + 16` and `SS = base + 8`. With base `0x10` that yields
//! `CS = 0x20` and `SS = 0x18`, which is why user **data** must sit immediately
//! below user **code** rather than the intuitive other way round. Swap those two
//! rows and `sysret` lands in ring 3 with a data selector in `CS`.
//!
//! `syscall` is the mirror image: it loads `CS = IA32_STAR[47:32]` and
//! `SS = that + 8`, so kernel code at `0x08` and kernel data at `0x10` are also
//! forced adjacent. Both constraints are on *this* table, and neither is checked
//! by anything at load time — a wrong order faults on the first transition.
//!
//! # Why the kernel entries are byte-identical to `boot.s`
//!
//! Deliberate: it means `CS` stays valid across the `lgdt`, so there is no need
//! to reload it with a far return. The only reload is `ltr`.

/// 64-bit TSS. 104 bytes, and the only field this kernel sets is `rsp0`.
///
/// `rsp0` is the stack the CPU switches to when an interrupt or exception is
/// taken **while in ring 3**. Without it, the first ring-3 page fault pushes its
/// frame onto the user stack — or onto garbage — and escalates to a triple
/// fault. It is the single reason a TSS exists here; nothing uses the IST slots
/// yet (see `idt.rs` on double faults).
#[repr(C, packed)]
struct Tss {
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

impl Tss {
    const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: core::mem::size_of::<Self>() as u16,
        }
    }
}

/// Stack the CPU switches to for a ring-3 → ring-0 trap. Separate from the boot
/// stack because it must be valid whatever userspace did to its own.
const KERNEL_TRAP_STACK_SIZE: usize = 16 * 1024;

/// The field is never *read* — the stack is addressed through a raw pointer to
/// the whole struct — but it must exist to reserve the bytes, and `repr(align)`
/// is what the CPU requires of a stack it switches to.
#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; KERNEL_TRAP_STACK_SIZE]);

static mut TRAP_STACK: TrapStack = TrapStack([0; KERNEL_TRAP_STACK_SIZE]);
static mut TSS: Tss = Tss::new();
static mut GDT: [u64; 7] = [0; 7];

pub const KERNEL_CODE: u16 = 0x08;
/// Kernel data. Not referenced by name anywhere: `syscall` derives `SS` as
/// `IA32_STAR[47:32] + 8`, so the hardware computes this selector rather than
/// being handed it. Kept because the *value* is a constraint on the table above
/// — moving kernel data off `0x10` silently breaks `syscall`.
#[allow(dead_code)]
pub const KERNEL_DATA: u16 = 0x10;
/// `IA32_STAR[63:48]`. `sysret` derives user CS/SS from it; see the module note.
pub const SYSRET_BASE: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x28;

/// Descriptors, matching `boot.s` for the two kernel entries.
const D_KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
const D_KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;
/// DPL 3 versions: access byte `0x9A|0x60` = `0xFA`, `0x92|0x60` = `0xF2`.
const D_USER_DATA: u64 = 0x00CF_F200_0000_FFFF;
const D_USER_CODE: u64 = 0x00AF_FA00_0000_FFFF;

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// Build the GDT and TSS, load both.
pub fn init() {
    // SAFETY: single core, interrupts masked, written once before `lgdt`. The
    // statics are reached only through raw pointers, never references.
    unsafe {
        let tss = &raw mut TSS;
        let stack = &raw mut TRAP_STACK;
        (*tss).rsp0 = stack.cast::<u8>().add(KERNEL_TRAP_STACK_SIZE) as u64;

        let tss_base = tss as u64;
        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u64;

        // System descriptor, type 0x9 = available 64-bit TSS. Unlike a segment
        // descriptor this is 16 bytes: the base is 64-bit and spills into the
        // following slot.
        let low = tss_limit & 0xFFFF
            | (tss_base & 0xFF_FFFF) << 16
            | 0x9 << 40
            | 1 << 47
            | (tss_limit & 0xF_0000) << 32
            | ((tss_base >> 24) & 0xFF) << 56;
        let high = tss_base >> 32;

        let gdt = &raw mut GDT;
        (*gdt)[0] = 0;
        (*gdt)[1] = D_KERNEL_CODE;
        (*gdt)[2] = D_KERNEL_DATA;
        (*gdt)[3] = D_USER_DATA;
        (*gdt)[4] = D_USER_CODE;
        (*gdt)[5] = low;
        (*gdt)[6] = high;

        let gdtr = Gdtr {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: gdt as u64,
        };

        // No far return to reload CS: entries 1 and 2 are byte-identical to the
        // ones `boot.s` installed, so the selectors already loaded stay valid.
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "ltr {tr:x}",
            gdtr = in(reg) &raw const gdtr,
            tr = in(reg) TSS_SELECTOR,
            options(readonly, nostack, preserves_flags)
        );
    }
}
