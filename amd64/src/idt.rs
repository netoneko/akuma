//! x86_64 interrupt descriptor table, CPU exception handlers, and demand paging.
//!
//! Stage C. Until this exists, *every* fault is fatal and invisible: with no IDT
//! loaded, a page fault escalates to a double fault, then to a triple fault, and
//! the VMM resets the guest with nothing on the serial line. Every bug in the
//! stages before this one had the same symptom — silence — which is why the
//! earlier modules bounds-check so aggressively. This is what replaces that
//! discipline with a diagnostic.
//!
//! # Why there is almost no hand-written assembly here
//!
//! Exception entry normally needs stubs: the CPU pushes an error code for some
//! vectors and not others, so a uniform frame has to be synthesised by hand, and
//! returning needs `iretq` rather than `ret`. rustc's `x86-interrupt` calling
//! convention does all of that, so every handler below but one is an ordinary
//! `fn`. That is a compiler feature, not a dependency — this crate still has none
//! beyond `akuma-alloc` and `akuma-pmm`.
//!
//! **Vector 14 is the exception (2026-09-05).** The page-fault handler is the
//! one handler that must sometimes *rewrite the return address* — to recover a
//! faulting user copy (`akuma-user-access`) instead of halting. `x86-interrupt`
//! hands the frame over by value and gives no supported way to edit it; the
//! obvious workaround, taking `&mut InterruptStackFrame`, resumed at an address
//! 5 bytes inside an unrelated instruction and raised `#UD`
//! (`docs/archive/AKUMA_USER_ACCESS_GATE_FIX.md`). So `#PF` enters through
//! [`page_fault_entry`], a `global_asm!` stub that owns the frame layout and the
//! `iretq`, and calls a plain `extern "C"` Rust function with a pointer to it.
//! Nothing about that stub depends on an unstable ABI's internals. The other
//! vectors stay `x86-interrupt`: they never return anywhere but where they came
//! from, or never return at all.
//!
//! # What is deliberately missing
//!
//! **No IST.** (`gdt.rs` has grown a TSS since this was written — ring 3 needs
//! `rsp0` — but its IST slots are unused.) A double fault therefore runs on the
//! faulting stack, which is fine while nothing can overflow it and wrong the
//! moment a guard page exists: a stack-overflow double fault would fault again
//! pushing its own frame and triple-fault. When a guard page appears, vector 8
//! needs an IST entry *before* it.
//!
//! **No hardware interrupts.** Vectors 32+ are unmapped and the PIC is not even
//! masked; `IF` has been 0 since `boot.s`, so nothing can arrive. A timer means
//! LAPIC setup, and that is a later stage.

use crate::paging::{self, MemAttr, Prot};
use crate::phys::phys_ptr;
use akuma_selftest::Suite;

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

/// Page faults redirected to the user-copy fixup, for the smoke test.
static COPY_FIXUPS: AtomicUsize = AtomicUsize::new(0);

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

    // Dump the words at the faulting rsp. For a fault *on* an `iretq` this is
    // the return frame the CPU was rejecting — rip, cs, rflags, rsp, ss — which
    // is the only way to see which selector it actually objected to rather than
    // inferring it from the error code.
    if frame.rsp != 0 && frame.rsp >= crate::phys::KERNEL_VMA {
        serial::puts("\n  [rsp]=");
        for i in 0..5 {
            // SAFETY: checked to be a kernel-window address above.
            let w = unsafe { (frame.rsp as *const u64).add(i).read_volatile() };
            serial::puts(" 0x");
            serial::put_hex(w);
        }
    }
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

/// What [`page_fault_entry`] hands to [`page_fault_dispatch`]: the error code
/// the CPU pushes for vector 14, then the ordinary return frame.
///
/// Layout is fixed by the hardware and by the stub's `lea rdi, [rsp + 80]`,
/// which points at the error code after the stub's ten pushes. Reordering these
/// fields changes what that assembly reads.
#[repr(C)]
pub struct PageFaultFrame {
    /// Bits: 0 present, 1 write, 2 user, 3 reserved-bit, 4 instruction fetch.
    pub error_code: u64,
    pub frame: InterruptStackFrame,
}

core::arch::global_asm!(
    r#"
    /* Naming the section is mandatory — see sched.rs for why a missing
     * `.section` puts code in .bss and fails the link. */
    .section .text
.global page_fault_entry
page_fault_entry:
    /* On entry the CPU has pushed, on a 16-byte-aligned rsp (long mode aligns
     * before pushing, whether or not the privilege level changed):
     *
     *   [rsp +  0]  error code
     *   [rsp +  8]  rip
     *   [rsp + 16]  cs
     *   [rsp + 24]  rflags
     *   [rsp + 32]  rsp
     *   [rsp + 40]  ss
     *
     * Save every caller-saved register: `page_fault_dispatch` is `extern "C"`,
     * so it preserves rbx/rbp/r12-r15 itself, but it is free to destroy these
     * nine and the interrupted code — which may be `rep movsb` in the middle of
     * a user copy, about to be re-executed after demand paging — is not
     * expecting a call. rbp is pushed too, as the tenth: ten pushes is 80
     * bytes, which keeps rsp 16-aligned at the `call`, as System V requires,
     * and gives a debugger a frame chain for free. */
    push rbp
    push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11

    lea rdi, [rsp + 80]             /* &PageFaultFrame: the error code slot */
    call page_fault_dispatch

    /* The dispatcher returned, so this fault was serviced or fixed up — it may
     * have rewritten the saved rip. Restore exactly what was saved; a demand-
     * paged store re-executes with the registers it faulted with. */
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    pop rbp

    add rsp, 8                      /* drop the error code; iretq does not */
    iretq
"#
);

unsafe extern "C" {
    /// The vector-14 entry point, installed in the IDT by [`init`].
    fn page_fault_entry();
}

/// `#PF` — the only handler that can *return*, and the only one that can return
/// *somewhere else*.
///
/// Called by [`page_fault_entry`] with the hardware frame; whatever this leaves
/// in `frame.rip` is where `iretq` resumes. Three outcomes, in this order:
///
/// 1. **Demand paging.** A fault inside the armed lazy region with bit 0 clear
///    (the page is not present): allocate a frame, map it, and return with the
///    frame untouched, so the faulting instruction re-executes and succeeds.
///    Anything else in the region is a real fault — in particular a *protection*
///    fault (bit 0 set) is a write to something deliberately read-only, and
///    servicing it would silently defeat the protection.
/// 2. **User-copy fixup.** The faulting `rip` is inside `akuma-user-access`'s
///    copy loop (`user_copy_fixup`): rewrite `rip` to its `EFAULT` trampoline
///    and return. That is the whole fault-recovery mechanism, and it is checked
///    *after* demand paging on purpose — a copy into a lazy page must be
///    serviced, not failed.
/// 3. **Fatal.** Everything else, as before.
///
/// `#[unsafe(no_mangle)]` and `extern "C"` because the stub `call`s it by name.
/// A plain function, not `x86-interrupt`: the stub already did the entry work,
/// and this must be free to edit the frame — see the module header.
#[unsafe(no_mangle)]
extern "C" fn page_fault_dispatch(frame: *mut PageFaultFrame) {
    // SAFETY: the stub passes a pointer into the current stack, to the frame
    // the CPU just pushed; it is live and exclusively ours until `iretq`.
    let pf = unsafe { &mut *frame };
    let code = pf.error_code;
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
            // SAFETY: a PMM frame, reached through the physmap.
            unsafe { core::ptr::write_bytes(phys_ptr::<u8>(frame_pa as u64), 0, 4096) };
            if paging::map_page(page as usize, frame_pa as u64, Prot::KERNEL_RW, MemAttr::WriteBack) {
                DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            akuma_pmm::free_page(frame_pa, 0);
        }
    }

    if let Some(fixup) = akuma_user_access::user_copy_fixup(pf.frame.rip) {
        COPY_FIXUPS.fetch_add(1, Ordering::Relaxed);
        pf.frame.rip = fixup;
        return;
    }

    fatal("#PF page fault", &pf.frame, Some(code));
}

/// The LAPIC timer vector.
///
/// Counts, acknowledges, and may **switch tasks** — the switch happens here, on
/// the interrupted task's own trap stack, which is what makes preemption
/// preemption. Everything it does is bounded and allocation-free; a handler runs
/// with `IF` clear (these are interrupt gates, not trap gates), so it cannot
/// nest.
extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    crate::lapic::on_tick();
    // Preemption. EOI has already been sent, so the LAPIC can deliver the next
    // tick to whichever task runs after this returns.
    crate::sched::preempt_if_needed();
}

/// Address of [`timer_interrupt`], for [`set_handler`].
///
/// A function rather than a `pub` handler because the handler's ABI is an
/// implementation detail of this module — the caller wants "the timer entry
/// point", not a typed `extern "x86-interrupt" fn` it would then have to spell.
#[allow(function_casts_as_integer)]
#[must_use]
pub fn timer_interrupt_entry() -> usize {
    timer_interrupt as usize
}

/// Install a handler for one vector, after [`init`].
///
/// The IDT is read by the CPU on every interrupt, so this edits a live table.
/// Safe only because it is called with interrupts masked and before the vector
/// it installs can be raised — the LAPIC timer is configured *after* its handler
/// is in place.
pub fn set_handler(vector: u8, handler: usize) {
    // SAFETY: reached only through a raw pointer, never a reference; single
    // core, interrupts masked.
    //
    // Bound to a local first rather than written `(*(&raw mut IDT))[..]`, which
    // trips `clippy::deref_addrof`. Clippy's suggested fix there — index `IDT`
    // directly — would reintroduce the `static_mut_refs` violation this pointer
    // exists to avoid, so the lint is right about the shape and wrong about the
    // remedy.
    unsafe {
        let idt = &raw mut IDT;
        (*idt)[vector as usize].set(handler);
    }
}

/// Build the IDT and load it.
///
/// `function_casts_as_integer` is allowed here deliberately. The lint exists to
/// catch a function *item* being used where its return value was meant, which is
/// almost always a bug — but putting a handler's address into a gate descriptor
/// is the one thing an IDT is. The handlers have five different signatures
/// (`x86-interrupt`, with and without an error code, one diverging, and the
/// bare `extern "C"` asm entry for `#PF`), so spelling each cast through its
/// exact fn-pointer type would add lines of ceremony that say nothing the
/// descriptor does not already say.
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
        // The one hand-assembled entry; see the module header.
        (*idt)[14].set(page_fault_entry as usize);

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
/// Chosen VA is 2 GiB — outside the identity map *and* clear of the 1 GiB
/// address `paging::smoke_test` uses, so nothing here can pass by accidentally
/// hitting an existing mapping.
///
/// Touching four pages proves the handler is re-entrant across faults rather
/// than working once, and reading the values back afterwards proves the mappings
/// survived — a handler that mapped the page and then lost it would still let
/// the faulting store retire.
pub fn smoke_test(t: &mut Suite) {
    const LAZY_BASE_VA: u64 = 2 << 30;
    const PAGES: u64 = 4;
    const LEN: u64 = PAGES * 4096;

    let free_before = akuma_pmm::free_count();
    arm_lazy(LAZY_BASE_VA, LEN);

    for i in 0..PAGES {
        let va = (LAZY_BASE_VA + i * 4096) as *mut u64;
        // SAFETY: unmapped on purpose — the #PF handler maps it and `iretq`
        // re-executes this store. That is the behaviour under test.
        unsafe { va.write_volatile(0xfeed_0000 + i) };
    }

    t.check_eq(
        "demand paging: faults serviced",
        DEMAND_FAULTS.load(Ordering::Relaxed) as u64,
        PAGES,
    );

    let mut readback_ok = true;
    for i in 0..PAGES {
        let va = (LAZY_BASE_VA + i * 4096) as *const u64;
        // SAFETY: mapped by the faults above.
        if unsafe { va.read_volatile() } != 0xfeed_0000 + i {
            readback_ok = false;
        }
    }
    t.check("demand paging: mappings survive the fault", readback_ok);

    disarm_lazy();

    // Release what the handler allocated, so the frame count returns to where it
    // started — a leak here would be invisible without this check.
    let mut unmapped = 0;
    for i in 0..PAGES {
        if let Some(pa) = paging::unmap_page((LAZY_BASE_VA + i * 4096) as usize) {
            akuma_pmm::free_page(pa as usize, 0);
            unmapped += 1;
        }
    }
    t.check_eq("demand paging: pages unmapped", unmapped, PAGES);

    // Exactly two frames stay out: the page directory and the page table that
    // had to be allocated to describe the 2 GiB region. `unmap_page` clears the
    // leaf and deliberately does not reclaim the tables above it — doing so
    // safely needs a per-table live-entry count, since another mapping may still
    // sit in the same table.
    //
    // This is pinned rather than tolerated. The previous version of this test
    // *printed* `126348 -> 126346` and scored itself `[OK]` anyway, because the
    // frame count was in the output and not in the condition. If table reclaim
    // is ever implemented, this number becomes 0 and the test says so.
    const RETAINED_TABLES: u64 = 2;
    t.check_eq(
        "demand paging: only the two intermediate tables retained",
        (free_before - akuma_pmm::free_count()) as u64,
        RETAINED_TABLES,
    );
}

/// Exercise the user-copy fault recovery for real: take a page fault inside
/// `__arch_copy_user_memory` on purpose and check the kernel gets `EFAULT` back
/// instead of halting.
///
/// A build that links is no evidence here. The failed first attempt at this
/// mechanism compiled, linked, and resumed at a garbage address
/// (`docs/archive/AKUMA_USER_ACCESS_GATE_FIX.md`); only a caught fault proves
/// the stub's frame offsets, the `rip` rewrite and the trampoline's `ret` all
/// agree. Runs after [`smoke_test`] so demand paging is already known-good.
///
/// Five things, each of which fails independently:
///
/// 1. A copy between two kernel buffers is byte-exact and returns `Ok` — the
///    plain path, with no fault taken.
/// 2. A copy whose *source* is an unmapped lower-half address returns
///    `Err(EFAULT)` — a load fault, fixed up.
/// 3. A copy whose *destination* is unmapped returns `Err(EFAULT)` — a store
///    fault, the other operand.
/// 4. A copy that starts on a mapped page and runs off its end fails with
///    `EFAULT` **after** copying the mapped prefix — proof the fault was taken
///    mid-`rep movsb` and the CPU's own progress in rcx/rsi/rdi was honoured,
///    not that the copy was refused up front.
/// 5. A copy out of the armed lazy region succeeds and reads zeroes — the
///    ordering in [`page_fault_dispatch`]: demand paging wins over fixup when
///    both apply. Fixup first would have failed this copy.
///
/// Plus the differential sweep from the crate itself, which pins `rep movsb`
/// against a byte loop over every alignment and tier-boundary length.
pub fn user_copy_smoke_test(t: &mut Suite) {
    use akuma_user_access::copy_from_user_safe;
    const EFAULT: u64 = 14;
    /// Lower half, well clear of anything a user program or test maps.
    const UNMAPPED_VA: u64 = 0x10_0000_0000;
    /// One page mapped on purpose, with the next page left unmapped.
    const EDGE_VA: u64 = 0x11_0000_0000;
    /// A lazy region for case 5, distinct from `smoke_test`'s 2 GiB.
    const LAZY_VA: u64 = 3 << 30;

    let free_before = akuma_pmm::free_count();
    let fixups_before = COPY_FIXUPS.load(Ordering::Relaxed);

    let mut src = [0u8; 256];
    for (i, b) in src.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    let mut dst = [0xA5u8; 256];

    // 1. valid copy
    // SAFETY: both are live kernel stack buffers of the stated length.
    let r = unsafe { copy_from_user_safe(dst.as_mut_ptr(), src.as_ptr(), src.len()) };
    t.check("user copy: kernel-to-kernel copy returns Ok", r.is_ok());
    t.check("user copy: kernel-to-kernel copy is byte-exact", dst == src);

    // 2. unmapped source
    dst.fill(0xA5);
    // SAFETY: the source is unmapped ON PURPOSE — the #PF handler must redirect
    // the copy loop to its trampoline. That redirection is what is under test.
    let r = unsafe { copy_from_user_safe(dst.as_mut_ptr(), UNMAPPED_VA as *const u8, 64) };
    t.check_eq("user copy: unmapped source returns EFAULT", r.err().unwrap_or(0), EFAULT);
    t.check("user copy: unmapped source wrote nothing", dst.iter().all(|&b| b == 0xA5));

    // 3. unmapped destination
    // SAFETY: as above, with the store faulting instead of the load.
    let r = unsafe { copy_from_user_safe(UNMAPPED_VA as *mut u8, src.as_ptr(), 64) };
    t.check_eq("user copy: unmapped destination returns EFAULT", r.err().unwrap_or(0), EFAULT);

    // The kernel is still running — that is the point — and a copy after a
    // recovered fault behaves like one before it.
    dst.fill(0xA5);
    // SAFETY: as case 1.
    let r = unsafe { copy_from_user_safe(dst.as_mut_ptr(), src.as_ptr(), src.len()) };
    t.check("user copy: copy after a recovered fault still works", r.is_ok() && dst == src);

    // 4. fault mid-copy: one mapped page, then the edge
    let mut edge_ok = false;
    if let Some(pa) = akuma_pmm::alloc_page() {
        // SAFETY: a fresh PMM frame, reached through the physmap.
        unsafe {
            let p = phys_ptr::<u8>(pa as u64);
            for i in 0..4096 {
                p.add(i).write_volatile((i as u8) ^ 0x5C);
            }
        }
        if paging::map_page(EDGE_VA as usize, pa as u64, Prot::KERNEL_RW, MemAttr::WriteBack) {
            static mut BIG: [u8; 8192] = [0; 8192];
            // SAFETY: single-threaded boot test; private to this fn.
            let big = unsafe { &mut *core::ptr::addr_of_mut!(BIG) };
            big.fill(0xEE);
            // SAFETY: the first 4096 bytes are mapped, the next 4096 are not; the
            // fault is intended and recovered.
            let r = unsafe { copy_from_user_safe(big.as_mut_ptr(), EDGE_VA as *const u8, 8192) };
            let prefix_ok = big[..4096].iter().enumerate().all(|(i, &b)| b == (i as u8) ^ 0x5C);
            let tail_untouched = big[4096..].iter().all(|&b| b == 0xEE);
            edge_ok = r == Err(EFAULT) && prefix_ok && tail_untouched;
            if let Some(pa) = paging::unmap_page(EDGE_VA as usize) {
                akuma_pmm::free_page(pa as usize, 0);
            }
        } else {
            akuma_pmm::free_page(pa, 0);
        }
    }
    t.check("user copy: fault off the end of a mapped page copies the prefix, then EFAULT", edge_ok);

    // 5. demand paging beats fixup
    arm_lazy(LAZY_VA, 4096);
    dst.fill(0xA5);
    let demand_before = DEMAND_FAULTS.load(Ordering::Relaxed);
    // SAFETY: the source is unmapped but inside the armed lazy region; the #PF
    // handler maps a zeroed page and re-executes the copy.
    let r = unsafe { copy_from_user_safe(dst.as_mut_ptr(), LAZY_VA as *const u8, dst.len()) };
    disarm_lazy();
    t.check(
        "user copy: a lazy-region fault inside the loop is demand-paged, not fixed up",
        r.is_ok()
            && dst.iter().all(|&b| b == 0)
            && DEMAND_FAULTS.load(Ordering::Relaxed) == demand_before + 1,
    );
    if let Some(pa) = paging::unmap_page(LAZY_VA as usize) {
        akuma_pmm::free_page(pa as usize, 0);
    }

    // Exactly three fixups: cases 2, 3 and 4. Case 5 must not have counted.
    t.check_eq(
        "user copy: exactly three faults were fixed up",
        (COPY_FIXUPS.load(Ordering::Relaxed) - fixups_before) as u64,
        3,
    );

    // The differential sweep, on kernel memory: `rep movsb` vs. the byte loop.
    let (checked, bad, first_bad) = akuma_user_access::copy_loop_differential_sweep();
    t.check("user copy: differential sweep ran", checked > 100_000);
    if !t.check_eq("user copy: rep movsb agrees with the byte loop", u64::from(bad), 0) {
        serial::puts("  first mismatch (src_align<<32|dst_align<<16|len)=0x");
        serial::put_hex(first_bad);
        serial::puts("\n");
    }

    // Two intermediate tables each for EDGE_VA and LAZY_VA (`unmap_page` keeps
    // them, as `smoke_test` explains), so four frames stay out. Pinned, like
    // there, so a leak in the fixup path shows up as a number and not a note.
    const RETAINED_TABLES: u64 = 4;
    t.check_eq(
        "user copy: only the intermediate tables retained",
        (free_before - akuma_pmm::free_count()) as u64,
        RETAINED_TABLES,
    );
}
