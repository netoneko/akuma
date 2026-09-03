//! x86_64 interrupt descriptor table, CPU exception handlers, and demand paging.
//!
//! Stage C. Until this exists, *every* fault is fatal and invisible: with no IDT
//! loaded, a page fault escalates to a double fault, then to a triple fault, and
//! the VMM resets the guest with nothing on the serial line. Every bug in the
//! stages before this one had the same symptom — silence — which is why the
//! earlier modules bounds-check so aggressively. This is what replaces that
//! discipline with a diagnostic.
//!
//! # Why there is no hand-written assembly here
//!
//! Exception entry normally needs stubs: the CPU pushes an error code for some
//! vectors and not others, so a uniform frame has to be synthesised by hand, and
//! returning needs `iretq` rather than `ret`. rustc's `x86-interrupt` calling
//! convention does all of that, so the handlers below are ordinary `fn`s. That is
//! a compiler feature, not a dependency — this crate still has none beyond
//! `akuma-alloc` and `akuma-pmm`.
//!
//! # What is deliberately missing
//!
//! **No TSS and no IST.** A double fault therefore runs on the faulting stack,
//! which is fine while nothing can overflow it and wrong the moment a guard page
//! exists: a stack-overflow double fault would fault again pushing its own frame
//! and triple-fault. The kernel is single-stack and single-core here, so the
//! machinery is not yet earned; when a guard page appears, vector 8 needs an IST
//! entry *before* it.
//!
//! **No hardware interrupts.** Vectors 32+ are unmapped and the PIC is not even
//! masked; `IF` has been 0 since `boot.s`, so nothing can arrive. A timer means
//! LAPIC setup, and that is a later stage.

use crate::paging::{self, Prot};
use crate::serial;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The frame the CPU pushes on exception entry, in push order.
///
/// Defined here rather than pulled from a crate: it is five `u64`s fixed by the
/// architecture, and `x86-interrupt` hands it over by value.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct InterruptStackFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// One 16-byte IDT gate descriptor.
#[derive(Clone, Copy)]
#[repr(C)]
struct Entry {
    offset_low: u16,
    selector: u16,
    /// Interrupt-stack-table index in bits 0:2; 0 means "use the current stack".
    ist: u8,
    /// `0x8E` = present, DPL 0, 64-bit interrupt gate. An *interrupt* gate rather
    /// than a trap gate, so `IF` is cleared on entry and a handler cannot be
    /// re-entered by a device interrupt.
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl Entry {
    const fn empty() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn set(&mut self, handler: usize) {
        let handler = handler as u64;
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        // The 64-bit code selector `boot.s` far-jumped through. Reading it from
        // `cs` would be more general; hardcoding it keeps this honest about the
        // fact that there is exactly one GDT in this kernel and `boot.s` owns it.
        self.selector = 0x08;
        self.ist = 0;
        self.type_attr = 0x8E;
        self.reserved = 0;
    }
}

/// The `lidt` operand: limit then base, packed.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

const IDT_LEN: usize = 256;

/// The table itself.
///
/// `static mut` because `lidt` takes its address and the CPU reads it directly;
/// there is no interior-mutability wrapper that changes what the hardware does.
/// Accessed only through raw pointers (`&raw mut`), never a reference, which is
/// what keeps it sound under the 2024 edition's `static_mut_refs` rule. It is
/// written exactly once, before `lidt`, on one core with interrupts masked.
static mut IDT: [Entry; IDT_LEN] = [Entry::empty(); IDT_LEN];

/// Page faults serviced by demand paging, for the smoke test.
static DEMAND_FAULTS: AtomicUsize = AtomicUsize::new(0);

/// Base of the lazily-backed test region, or 0 if none is armed.
static LAZY_BASE: AtomicU64 = AtomicU64::new(0);
/// Length in bytes of the lazily-backed region.
static LAZY_LEN: AtomicU64 = AtomicU64::new(0);

/// `CR2` holds the faulting linear address after a page fault.
fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: reading CR2 copies a register into a local; it dereferences
    // nothing and has no side effect.
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Print a stack frame and stop.
fn fatal(vector: &str, frame: &InterruptStackFrame, error_code: Option<u64>) -> ! {
    serial::puts("\n[EXCEPTION] ");
    serial::puts(vector);
    if let Some(code) = error_code {
        serial::puts(" err=0x");
        serial::put_hex(code);
    }
    serial::puts("\n  rip=0x");
    serial::put_hex(frame.rip);
    serial::puts(" rsp=0x");
    serial::put_hex(frame.rsp);
    serial::puts("\n  cs=0x");
    serial::put_hex(frame.cs);
    serial::puts(" rflags=0x");
    serial::put_hex(frame.rflags);
    serial::puts("\n  cr2=0x");
    serial::put_hex(read_cr2());
    serial::puts("\n");
    crate::halt();
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    fatal("#DE divide error", &frame, None);
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    fatal("#UD invalid opcode", &frame, None);
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    // Diverging by signature: `iretq` from a double fault is not architecturally
    // defined to work, so there is nothing to return to.
    fatal("#DF double fault", &frame, Some(code));
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, code: u64) {
    fatal("#GP general protection", &frame, Some(code));
}

extern "x86-interrupt" fn unhandled(frame: InterruptStackFrame) {
    fatal("unhandled vector", &frame, None);
}

/// `#PF` — the only handler that can *return*.
///
/// Page-fault error code bits: 0 present, 1 write, 2 user, 3 reserved-bit,
/// 4 instruction fetch.
///
/// A fault inside the armed lazy region with bit 0 clear (the page is not
/// present) is demand paging: allocate a frame, map it, and return. `iretq`
/// re-executes the faulting instruction, which then succeeds. Anything else is
/// a real fault and is fatal — in particular a *protection* fault (bit 0 set)
/// inside the region is not "not mapped yet", it is a write to something
/// deliberately read-only, and servicing it would silently defeat the
/// protection.
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: u64) {
    let addr = read_cr2();
    let base = LAZY_BASE.load(Ordering::Relaxed);
    let len = LAZY_LEN.load(Ordering::Relaxed);

    let in_lazy = base != 0 && addr >= base && addr < base + len;
    let not_present = code & 1 == 0;

    if in_lazy && not_present {
        let page = addr & !0xfff;
        if let Some(frame_pa) = akuma_pmm::alloc_page() {
            // Zero before mapping: a recycled frame otherwise leaks whatever the
            // previous owner left in it to whoever faults next.
            // SAFETY: PMM frames are inside the identity map boot.s built.
            unsafe { core::ptr::write_bytes(frame_pa as *mut u8, 0, 4096) };
            if paging::map_page(page as usize, frame_pa as u64, Prot::KERNEL_RW) {
                DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            akuma_pmm::free_page(frame_pa, 0);
        }
    }

    fatal("#PF page fault", &frame, Some(code));
}

/// Build the IDT and load it.
///
/// `function_casts_as_integer` is allowed here deliberately. The lint exists to
/// catch a function *item* being used where its return value was meant, which is
/// almost always a bug — but putting a handler's address into a gate descriptor
/// is the one thing an IDT is. The five handlers have five different signatures
/// (`x86-interrupt`, with and without an error code, one diverging), so spelling
/// each cast through its exact fn-pointer type would add five lines of ceremony
/// that say nothing the descriptor does not already say.
#[allow(function_casts_as_integer)]
pub fn init() {
    // SAFETY: single core, interrupts masked, and the table is reached only
    // through raw pointers — never a reference to the `static mut`.
    unsafe {
        let idt = &raw mut IDT;
        for i in 0..IDT_LEN {
            (*idt)[i].set(unhandled as usize);
        }
        (*idt)[0].set(divide_error as usize);
        (*idt)[6].set(invalid_opcode as usize);
        (*idt)[8].set(double_fault as usize);
        (*idt)[13].set(general_protection as usize);
        (*idt)[14].set(page_fault as usize);

        let idtr = Idtr {
            limit: (core::mem::size_of::<[Entry; IDT_LEN]>() - 1) as u16,
            base: idt as u64,
        };
        core::arch::asm!("lidt [{}]", in(reg) &raw const idtr, options(readonly, nostack, preserves_flags));
    }
}

/// Arm a lazily-backed virtual range. Pages appear on first touch.
fn arm_lazy(base: u64, len: u64) {
    LAZY_LEN.store(len, Ordering::Relaxed);
    LAZY_BASE.store(base, Ordering::Relaxed);
}

fn disarm_lazy() {
    LAZY_BASE.store(0, Ordering::Relaxed);
    LAZY_LEN.store(0, Ordering::Relaxed);
}

/// Take a fault on purpose and service it.
///
/// Chosen VA is 2 GiB — outside the 1 GiB `boot.s` identity-maps and clear of
/// the 1 GiB address `paging::smoke_test` uses, so nothing here can pass by
/// accidentally hitting an existing mapping.
///
/// Touching four pages proves the handler is re-entrant across faults rather
/// than working once, and reading the values back afterwards proves the mappings
/// survived — a handler that mapped the page and then lost it would still let
/// the faulting store retire.
pub fn smoke_test() {
    const LAZY_BASE_VA: u64 = 2 << 30;
    const PAGES: u64 = 4;
    const LEN: u64 = PAGES * 4096;

    serial::puts("  test: demand paging ");

    let free_before = akuma_pmm::free_count();
    arm_lazy(LAZY_BASE_VA, LEN);

    for i in 0..PAGES {
        let va = (LAZY_BASE_VA + i * 4096) as *mut u64;
        // SAFETY: unmapped on purpose — the #PF handler maps it and `iretq`
        // re-executes this store. That is the behaviour under test.
        unsafe { va.write_volatile(0xfeed_0000 + i) };
    }

    let mut ok = DEMAND_FAULTS.load(Ordering::Relaxed) == PAGES as usize;

    for i in 0..PAGES {
        let va = (LAZY_BASE_VA + i * 4096) as *const u64;
        // SAFETY: mapped by the faults above.
        if unsafe { va.read_volatile() } != 0xfeed_0000 + i {
            ok = false;
        }
    }

    disarm_lazy();

    // Release what the handler allocated, so the frame count returns to where it
    // started — a leak here would be invisible without this check.
    for i in 0..PAGES {
        if let Some(pa) = paging::unmap_page((LAZY_BASE_VA + i * 4096) as usize) {
            akuma_pmm::free_page(pa as usize, 0);
        } else {
            ok = false;
        }
    }

    serial::put_dec(DEMAND_FAULTS.load(Ordering::Relaxed) as u64);
    serial::puts(" faults serviced, frames ");
    serial::put_dec(free_before as u64);
    serial::puts(" -> ");
    serial::put_dec(akuma_pmm::free_count() as u64);
    serial::puts(if ok { "   [OK]\n" } else { "   [FAIL]\n" });
}
