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
//! **Vectors 13 and 14 are the exception (2026-09-05).** The page-fault
//! handler must sometimes *rewrite the return address* — to recover a faulting
//! user copy (`akuma-user-access`) instead of halting — and `#GP` must do the
//! same for a non-canonical address in that copy, which never reaches `#PF`. `x86-interrupt`
//! hands the frame over by value and gives no supported way to edit it; the
//! obvious workaround, taking `&mut InterruptStackFrame`, resumed at an address
//! 5 bytes inside an unrelated instruction and raised `#UD`
//! (`docs/archive/AKUMA_USER_ACCESS_GATE_FIX.md`). So both enter through a
//! `global_asm!` stub (`fixable_exception_entry!`) that owns the frame layout
//! and the `iretq`, and calls a plain `extern "C"` Rust function with a pointer
//! to it. Nothing about that stub depends on an unstable ABI's internals. The
//! other vectors stay `x86-interrupt`: they never return anywhere but where
//! they came from, or never return at all.
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

use crate::paging::{self, MemAttr, PageFaultCode, PteProt};
use crate::phys::phys_ptr;
#[cfg(not(feature = "no-tests"))]
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

/// Not-present faults serviced from a **user** `mmap` region — demand paging
/// proper, as opposed to [`DEMAND_FAULTS`]'s armed kernel test window.
///
/// A separate counter, not a shared one, for two reasons. The armed-window test
/// asserts an *exact* equality (`demand_before + 1`), so a user fault landing on
/// the same counter would break a check that has nothing to do with it. And this
/// one is the evidence that the lazy path is live at all: it is asserted
/// non-zero after the boot suite has run real programs
/// (`mm::demand_paging_report`), which is what stops "lazy mmap" quietly
/// regressing to "every mapping happened to be small".
pub static USER_DEMAND_FAULTS: AtomicU64 = AtomicU64::new(0);

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
    //
    // 64 words, not 5: there are no frame pointers in a default build, so the
    // stack is the only backtrace there is — return addresses from every frame
    // the crash interrupted are in there, symbolizable against `nm` output.
    // Learned from the 2026-09-12 ssh-login crash: the faulting rip was a wild
    // `0x86` with no caller named, and 5 words were not enough to find one.
    if frame.rsp != 0 && frame.rsp >= crate::phys::KERNEL_VMA {
        serial::puts("\n  [rsp]");
        for i in 0..64 {
            // SAFETY: checked to be a kernel-window address above; the kernel
            // stack is at least a page, and the read is volatile so nothing
            // reorders it into the prints.
            let w = unsafe { (frame.rsp as *const u64).add(i).read_volatile() };
            if i % 4 == 0 {
                serial::puts("\n   ");
            }
            serial::puts(" ");
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

extern "x86-interrupt" fn unhandled(frame: InterruptStackFrame) {
    fatal("unhandled vector", &frame, None);
}

/// What [`page_fault_entry`] hands to [`page_fault_dispatch`]: the error code
/// the CPU pushes for vector 14, then the ordinary return frame.
///
/// Layout is fixed by the hardware and by the stub's `lea rdi, [rsp + 128]`,
/// which points at the error code past the stub's sixteen pushes. Reordering
/// these fields changes what that assembly reads.
#[repr(C)]
pub struct PageFaultFrame {
    /// Bits: 0 present, 1 write, 2 user, 3 reserved-bit, 4 instruction fetch.
    pub error_code: u64,
    pub frame: InterruptStackFrame,
}

/// The interrupted **general-purpose register file**, saved by the stub below
/// and handed to the dispatcher as its second argument.
///
/// Field order is the stub's push order reversed — `r15` is pushed last and so
/// lands at offset 0 — and it is the ABI between two files. The `const _`
/// under this type asserts it.
///
/// # Why all fifteen, when ten were enough
///
/// Ten (the caller-saved set plus `rbp`) is exactly what a *serviced* fault
/// needs: the dispatcher is `extern "C"`, so it preserves `rbx`/`r12`-`r15`
/// itself, and the interrupted instruction re-executes with everything intact.
///
/// **Delivering a signal needs to read them, not merely preserve them.** A
/// `SIGSEGV` handler is handed a `ucontext_t` whose `uc_mcontext` is the
/// interrupted register file, and `rt_sigreturn` puts that file back — so a
/// register the kernel never wrote down is a register a `longjmp`-free handler
/// returns into garbage. Preserved-in-the-register is not the same as
/// readable-from-Rust: by the time the dispatcher decides to deliver, its own
/// Rust frames have used `rbx` and `r12`-`r15` for their own purposes and
/// restored them only on the way out.
///
/// The sixteenth push is padding, and it is load-bearing: fifteen pushes is
/// 120 bytes, which would leave `rsp` 8 off the 16-byte alignment System V
/// requires at the `call`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapRegs {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
}

/// The stub indexes nothing by hand — it pushes and pops in a fixed order — but
/// **Rust reads this struct off that stack**, so the two orders are one fact in
/// two files. A reordered field here compiles, boots, and hands a signal
/// handler a register file with two values swapped.
const _: () = {
    assert!(core::mem::offset_of!(TrapRegs, r15) == 0);
    assert!(core::mem::offset_of!(TrapRegs, rbp) == 64);
    assert!(core::mem::offset_of!(TrapRegs, rax) == 112);
    assert!(core::mem::size_of::<TrapRegs>() == 120);
};

/// The hand-assembled entry for an exception **with an error code** whose
/// handler may rewrite the return address: `$entry` is the symbol the IDT gate
/// points at, `$dispatch` the `#[unsafe(no_mangle)] extern "C"
/// fn(*mut PageFaultFrame, *mut TrapRegs)` it calls. Used for vectors 13 and
/// 14; see the module header for why those two and no others.
macro_rules! fixable_exception_entry {
    ($entry:literal, $dispatch:literal) => {
        core::arch::global_asm!(concat!(
            /* Naming the section is mandatory — see sched.rs for why a missing
             * `.section` puts code in .bss and fails the link. */
            "    .section .text\n",
            ".global ", $entry, "\n",
            $entry, ":\n",
            /* On entry the CPU has pushed, on a 16-byte-aligned rsp (long mode
             * aligns before pushing, whether or not the privilege level changed):
             *
             *   [rsp +  0]  error code
             *   [rsp +  8]  rip
             *   [rsp + 16]  cs
             *   [rsp + 24]  rflags
             *   [rsp + 32]  rsp
             *   [rsp + 40]  ss
             *
             * If the fault came from ring 3, `%gs` is the program's; kernel code
             * expects its per-CPU block there (`smp.rs`). Swap it in — and only
             * then, because from ring 0 it is already in place and a second
             * `swapgs` would swap it out. The saved CS's RPL is the test. */
            "    test qword ptr [rsp + 16], 3\n",
            "    jz 3f\n",
            "    swapgs\n",
            "3:\n",
            /*
             * Save the **whole** general-purpose register file, as `TrapRegs`.
             *
             * The caller-saved nine plus rbp is what a *serviced* fault needs:
             * the dispatcher is `extern "C"`, so it preserves rbx/r12-r15
             * itself, but it is free to destroy the rest and the interrupted
             * code — which may be `rep movsb` in the middle of a user copy,
             * about to be re-executed after demand paging — is not expecting a
             * call. Delivering a **signal** needs more than that: the handler is
             * handed the interrupted register file as `uc_mcontext`, and
             * `rt_sigreturn` puts it back, so a register the kernel never wrote
             * down is one a handler returns into garbage. See `TrapRegs`.
             *
             * The padding push first: fifteen registers is 120 bytes, and rsp
             * must be 16-aligned at the `call`. Pushing it first rather than
             * last is what puts the register block at [rsp + 0], so the
             * dispatcher's second argument is a plain `lea rsi, [rsp]`.
             *
             * Push order is `TrapRegs` read bottom-up: rax first lands at the
             * highest offset, r15 last lands at 0. */
            "    sub rsp, 8\n",                   /* alignment padding */
            "    push rax\n",
            "    push rbx\n",
            "    push rcx\n",
            "    push rdx\n",
            "    push rsi\n",
            "    push rdi\n",
            "    push rbp\n",
            "    push r8\n",
            "    push r9\n",
            "    push r10\n",
            "    push r11\n",
            "    push r12\n",
            "    push r13\n",
            "    push r14\n",
            "    push r15\n",
            /* Hardware does NOT clear RFLAGS.AC on exception delivery, so a
             * fault taken inside a `stac` window would run the dispatcher with
             * SMAP suspended. Clear it — but only where SMAP is on, because
             * `clac` is #UD on a CPU without it (Haswell). `iretq` restores the
             * saved rflags, so a demand-paged `rep movsb` resumes with AC set. */
            "    cmp byte ptr [rip + SMAP_ACTIVE], 0\n",
            "    je 1f\n",
            "    clac\n",
            "1:\n",
            "    lea rdi, [rsp + 128]\n",         /* &PageFaultFrame: the error code slot */
            "    lea rsi, [rsp]\n",               /* &TrapRegs */
            "    call ", $dispatch, "\n",
            /* The dispatcher returned, so this fault was serviced, fixed up, or
             * **redirected into a signal handler** — it may have rewritten the
             * saved rip and rsp, and the argument registers in the block below.
             * Restore exactly what is there now; a demand-paged store
             * re-executes with the registers it faulted with, and a delivered
             * signal enters with the three the dispatcher wrote. */
            "    pop r15\n",
            "    pop r14\n",
            "    pop r13\n",
            "    pop r12\n",
            "    pop r11\n",
            "    pop r10\n",
            "    pop r9\n",
            "    pop r8\n",
            "    pop rbp\n",
            "    pop rdi\n",
            "    pop rsi\n",
            "    pop rdx\n",
            "    pop rcx\n",
            "    pop rbx\n",
            "    pop rax\n",
            "    add rsp, 8\n",                   /* the alignment padding */
            "    add rsp, 8\n",                   /* drop the error code; iretq does not */
            /* Back to the program's `%gs` if that is where we are going. */
            "    test qword ptr [rsp + 8], 3\n",
            "    jz 4f\n",
            "    swapgs\n",
            "4:\n",
            "    iretq\n",
        ));
    };
}

// The LAPIC timer's entry: hand-assembled like the two above, because its
// handler may **switch tasks** — the `iretq` at the end then resumes a
// different task's frame — and because it must `swapgs` on a ring-3 origin so
// the scheduler it calls can find its per-CPU block.
//
// It saves the same `TrapRegs` the exception stubs do, and for the same reason
// they grew to: since 2026-09-11 the dispatcher may **redirect this frame into
// a signal handler**, which means reading the whole interrupted register file
// (a handler is handed it as `uc_mcontext` and `rt_sigreturn` puts it back) and
// writing three registers back. The tick is the last place a signal could not
// reach — a program that never syscalls and never faults.
//
// **The alignment arithmetic is different here and there is no padding push.**
// There is no error code, so the CPU pushes 40 bytes and `rsp` arrives
// `≡ 8 (mod 16)`; fifteen pushes is 120, which is also `≡ 8`, so the two cancel
// and `rsp` is 16-aligned at the `call` exactly as System V requires. The old
// ten-push form needed a `sub rsp, 8` to get there; adding one here would break
// it. The exception stubs pad because their error code makes the entry
// alignment the other one.
core::arch::global_asm!(
    r#"
    .section .text
.global timer_entry
timer_entry:
    test qword ptr [rsp + 8], 3
    jz 1f
    swapgs
1:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    lea rdi, [rsp + 120]             /* &InterruptStackFrame */
    lea rsi, [rsp]                   /* &TrapRegs */
    call timer_dispatch
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    test qword ptr [rsp + 8], 3
    jz 2f
    swapgs
2:
    iretq
"#
);

fixable_exception_entry!("page_fault_entry", "page_fault_dispatch");
fixable_exception_entry!("general_protection_entry", "general_protection_dispatch");

unsafe extern "C" {
    /// The vector-14 entry point, installed in the IDT by [`init`].
    fn page_fault_entry();
    /// The vector-13 entry point, installed in the IDT by [`init`].
    fn general_protection_entry();
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
extern "C" fn page_fault_dispatch(frame: *mut PageFaultFrame, regs: *mut TrapRegs) {
    // BKL-hold attribution: stamp the core's cache for this transient
    // excursion (the interrupted thread keeps its own tag — see
    // `set_core_tag_transient`), so a hold inside fault service names
    // "fault" rather than the syscall that was interrupted.
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    // SAFETY: the stub passes a pointer into the current stack, to the frame
    // the CPU just pushed; it is live and exclusively ours until `iretq`.
    let pf = unsafe { &mut *frame };
    let code = PageFaultCode::new(pf.error_code);
    let addr = read_cr2();
    let base = LAZY_BASE.load(Ordering::Relaxed);
    let len = LAZY_LEN.load(Ordering::Relaxed);

    let in_lazy = base != 0 && addr >= base && addr < base + len;

    if in_lazy && code.not_present() {
        let page = addr & !0xfff;
        if let Some(frame_pa) = akuma_pmm::alloc_page() {
            // Zero before mapping: a recycled frame otherwise leaks whatever the
            // previous owner left in it to whoever faults next.
            // SAFETY: a PMM frame, reached through the physmap.
            unsafe { core::ptr::write_bytes(phys_ptr::<u8>(frame_pa as u64), 0, 4096) };
            if paging::map_page(page as usize, frame_pa as u64, PteProt::KERNEL_RW, MemAttr::WriteBack) {
                DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            akuma_pmm::free_page(frame_pa, 0);
        }
    }

    // The two arms below service the fault, and servicing it can **flush** —
    // a CoW break rewrites a live PTE and `akuma_mmu`'s `AllCores` flush then
    // broadcasts a shootdown IPI whose acknowledgement wait assumes every
    // sender holds the BKL (the deadlock argument on `set_shootdown_hooks` in
    // `akuma-mmu`: the BKL is outermost, so no peer can be IRQ-masked on a
    // lock the sender holds). A fault from ring 3 arrives without the BKL, so
    // take it for the servicing window and drop it after — the same bracket
    // the syscall path runs its own memory syscalls under. A fault from ring
    // 0 already holds it; `enter_kernel` is reentrant by owner core and this
    // leaves the hold alone.
    let took_bkl = !crate::smp::bkl_held();
    if took_bkl {
        crate::smp::bkl_enter();
    }

    // Demand paging for ring 3, from the per-address-space region table
    // (`mm::fault_in`). A not-present fault inside a mapping this process has
    // been given gets a zeroed frame at the region's own protection; anything
    // outside every region falls through, which is what keeps a wild pointer a
    // fault rather than a free page.
    //
    // Not gated on `is_user_mode`, deliberately. A `copy_to_user` into a lazily
    // mapped buffer faults from **ring 0**, and requiring ring 3 here would send
    // it to the fixup arm below and turn a `read(2)` into an unexplained
    // `EFAULT`. `fault_in` resolves the address against the current process's
    // regions, all of which are user addresses, so a genuine kernel fault still
    // finds nothing and falls through.
    //
    // Placed after the armed test window (which is a kernel mapping and must not
    // be confused with a user region) and before the CoW arm: a page has to be
    // *populated* before anyone can ask whether it is shared.
    if code.not_present() && crate::mm::fault_in(addr) {
        USER_DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
        if took_bkl {
            crate::smp::bkl_leave();
        }
        return;
    }

    // Copy-on-write. A write to a **present** page, from either ring.
    //
    // Placed after demand paging and before the user-copy fixup, and both
    // orderings are deliberate. A lazy page must be *populated* before anyone
    // asks whether it is shared. And a `copy_to_user` landing on a CoW page is
    // a legitimate write the kernel should break the sharing for, not an
    // `EFAULT` to hand back — sending it to the fixup would make `read(2)` into
    // a forked child's buffer fail with no explanation.
    //
    // That last sentence was aspirational until 2026-09-07: this arm required
    // the fault to come from ring 3, and a `copy_to_user` fault comes from ring
    // 0, so the kernel's own writes could never reach the break. They could not
    // reach *anything* — `CR0.WP` was clear too, so the write simply succeeded
    // against a read-only page and the CoW sharing was never broken at all.
    // Both halves are fixed together; `akuma_user_access`'s `CR0_WP` has the
    // measurement.
    if code.is_write_to_present_page() && cow_write_fault(addr) {
        if took_bkl {
            crate::smp::bkl_leave();
        }
        return;
    }

    if took_bkl {
        crate::smp::bkl_leave();
    }

    if let Some(fixup) = akuma_user_access::user_copy_fixup(pf.frame.rip) {
        COPY_FIXUPS.fetch_add(1, Ordering::Relaxed);
        pf.frame.rip = fixup;
        return;
    }

    if pf.frame.cs & 3 == 3 {
        // **A ring-3 fault nothing serviced is a `SIGSEGV`, not a kill** — if
        // the program installed a handler for it. `deliver_fault_signal`
        // rewrites the pushed `rip`/`rsp` and the three argument registers, so
        // the stub's `iretq` below enters the handler instead of resuming the
        // instruction that faulted.
        //
        // Placed after every servicing arm and after the user-copy fixup, which
        // is the only order that works: a demand-paged page or a CoW break is
        // not a fault the program should hear about, and a fault inside
        // `copy_to_user` belongs to the *kernel's* access, not to ring 3.
        //
        // `si_code` is the distinction a handler reads to tell a wild pointer
        // from a permission it does not have, and it is exactly the
        // present bit: `SEGV_MAPERR` for an address with no translation,
        // `SEGV_ACCERR` for one that has a translation refusing the access —
        // which is what an `mprotect` downgrade produces.
        let si_code = if code.not_present() {
            crate::signal::segv::MAPERR
        } else {
            crate::signal::segv::ACCERR
        };
        // SAFETY: the stub's own register block, live until its `pop` sequence;
        // nothing else holds a reference to it.
        let regs = unsafe { &mut *regs };
        // **Under the BKL**, for the reason the servicing arms above state:
        // writing the signal frame to the user stack can itself demand-page or
        // break a CoW page, and a CoW break broadcasts a shootdown IPI whose
        // acknowledgement wait assumes every sender holds the lock. The nested
        // `#PF` would take it anyway (this dispatcher is reentrant by owner
        // core), so this is belt and braces — and the belt is cheap next to a
        // fault that is about to build a 440-byte frame. `kill_current_from_fault`
        // takes it on the other side of this decision for the same reason.
        let took = !crate::smp::bkl_held();
        if took {
            crate::smp::bkl_enter();
        }
        let delivered =
            crate::signal::deliver_fault_signal(&mut pf.frame, regs, SIGSEGV, si_code, addr);
        if took {
            crate::smp::bkl_leave();
        }
        if delivered {
            return;
        }
        describe_page_fault(code);
        user_fault("#PF page fault", &pf.frame, Some(code.raw()));
    }
    describe_page_fault(code);
    fatal("#PF page fault", &pf.frame, Some(code.raw()));
}

/// Say in words what the error code says in bits, before the fatal printer's hex.
///
/// The bits are not memorable and the hex is not readable under pressure. The
/// one that most repays naming is [`PageFaultCode::reserved_bit`]: it is
/// **never** a userspace mistake — a reserved bit set in a paging-structure
/// entry means this kernel's own walker wrote a malformed one, and the usual
/// cause is setting `NX` with `EFER.NXE` clear. Reported as a generic
/// protection fault it looks like a program bug and is searched for in the
/// wrong file.
fn describe_page_fault(code: PageFaultCode) {
    serial::puts("  #PF: ");
    serial::puts(if code.not_present() { "not-present" } else { "protection" });
    serial::puts(if code.is_write() { " write" } else { " read" });
    if code.instruction_fetch() {
        serial::puts(" (instruction fetch)");
    }
    serial::puts(if code.is_user_mode() { " from ring 3" } else { " from ring 0" });
    if code.reserved_bit() {
        serial::puts(" RESERVED BIT SET — a malformed paging-structure entry, i.e. a kernel bug");
    }
    serial::puts("\n");
}

/// A borrowed view of the address space the faulting access actually used.
///
/// # Why `CR3` and not the running process's `Process.space`
///
/// They are the same thing whenever there is a process: the scheduler installs a
/// user task's root before it runs, in kernel mode as well as ring 3, so a
/// `copy_to_user` fault and a ring-3 fault both name the same tables. Reading
/// `CR3` says *which address space the fault happened in* directly, rather than
/// deriving it and hoping the derivation agrees — which is the same discipline
/// `translate`/`prot` follow by walking rather than consulting a shadow record.
///
/// It is also the only shape that keeps the one case with **no** process:
/// `uaccess.rs`'s `CR0.WP` self-test maps a CoW-marked pair into the kernel's own
/// root and drives this path from ring 0, and it is the sole coverage the break
/// has that does not need a live user program. Resolving through the process
/// table would make that test silently stop testing anything.
///
/// [`UserAddressSpace::new_shared`] is exactly the right constructor: a view
/// that names an existing L0 and whose ledger **owns nothing**, so it frees
/// nothing when it drops. Nothing here allocates a page table either — every
/// arm rewrites a leaf that is already present — so the view's throwaway ledger
/// never has anything to lose. The *real* ledger update is
/// [`crate::usermode::cow_swap_frame`], against the running process, which is
/// where a replaced frame has to be recorded.
fn faulting_address_space() -> akuma_mmu::UserAddressSpace {
    // `new_shared` cannot fail on this target — it allocates nothing — but it
    // returns `Option` because the shared callers in `akuma-exec` are written
    // against a fallible constructor. An `expect` here would be a panic in the
    // page-fault handler; the empty view refuses every lookup instead, which
    // falls through to the ordinary fatal path.
    akuma_mmu::UserAddressSpace::new_shared(paging::active_root() as usize)
        .unwrap_or_else(|| akuma_mmu::UserAddressSpace::new_shared(0).unwrap())
}

/// Break copy-on-write sharing for the page containing `addr`.
///
/// Returns `true` when the faulting instruction can be re-executed. `false`
/// falls through to the ordinary fatal path, which is right for a write to a
/// page that is read-only on purpose and for an out-of-memory copy alike — in
/// both cases the write genuinely cannot be allowed to proceed.
///
/// The **decision** is `akuma_cow`, host-tested and shared with the AArch64
/// kernel; everything here is the mechanism. Reading the live PTE rather than
/// trusting the fault's error code is part of that contract: see
/// [`akuma_cow::CowFault::pte_writable`].
fn cow_write_fault(addr: u64) -> bool {
    use akuma_cow::{CowAction, CowFault};

    let page = (addr as usize) & !0xfff;
    let mut faulted = faulting_address_space();
    let Some((prot, marked)) = faulted.pte_prot(page) else {
        return false; // not mapped — not a CoW break
    };
    if !prot.user {
        return false; // a kernel page; ring 3 had no business writing it
    }
    let Some(pa) = faulted.translate(page).map(|pa| pa & !0xfff) else {
        return false;
    };

    let action = CowFault {
        pte_writable: prot.write,
        marked,
        refs: akuma_pmm::cow_ref_get(pa),
    }
    .decide();

    match action {
        // Someone repaired the page between the trap and here. Nothing to do —
        // and killing the process would be the spurious-SIGSEGV bug.
        CowAction::Retry => {
            COW_RETRIES.fetch_add(1, Ordering::Relaxed);
            true
        }
        CowAction::Fault => false,
        // Sole owner: clear the marker and grant the write. No allocation, no
        // copy, no memory pressure — and this is the common case, because a
        // `fork` child usually `execve`s and leaves the parent alone with
        // everything.
        CowAction::TakeInPlace => {
            let writable = PteProt { write: true, ..prot };
            if !faulted.map_page_pte(page, pa, writable, false) {
                return false;
            }
            // The frame stops being shared, so drop this address space's claim
            // on the share count. `cow_ref_dec` reports whether we now own the
            // free; we do not free — we are still mapping it — so the answer is
            // deliberately discarded, and the page is simply ours outright.
            let _ = akuma_pmm::cow_ref_dec(pa);
            COW_TAKEN.fetch_add(1, Ordering::Relaxed);
            true
        }
        // Genuinely shared: private copy.
        CowAction::Copy => {
            let Some(fresh) = akuma_pmm::alloc_page() else {
                return false; // OOM: fall through to the fatal path
            };
            // SAFETY: both frames are live PMM pages reached through the
            // physmap, and exactly one page is copied. The destination is not
            // published in any page table until the `map_page_in` below.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_ptr::<u8>(pa as u64),
                    phys_ptr::<u8>(fresh as u64),
                    4096,
                );
            }
            let writable = PteProt { write: true, ..prot };
            if !faulted.map_page_pte(page, fresh, writable, false) {
                akuma_pmm::free_page(fresh, 0);
                return false;
            }
            // The old frame loses this address space. Only the last holder
            // frees it — the whole point of the count.
            if akuma_pmm::cow_ref_dec(pa) {
                akuma_pmm::free_page(pa, 0);
            }
            // The private copy is tracked by this process's ledger so teardown
            // gives it back; the frame it replaced was removed from the ledger
            // by the same call that decremented above.
            crate::usermode::cow_swap_frame(pa, fresh);
            COW_COPIES.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

/// Copy-on-write outcome counters, reported by the boot self-tests.
pub static COW_COPIES: AtomicU64 = AtomicU64::new(0);
pub static COW_TAKEN: AtomicU64 = AtomicU64::new(0);
pub static COW_RETRIES: AtomicU64 = AtomicU64::new(0);

/// A fault taken **in ring 3** that no handler wanted: report it and kill the
/// process, not the core.
///
/// Halting was the right answer while every fault was the kernel's own bug to
/// see. A program's segfault is not: on one core it took the whole machine
/// down, and on several it silently parked one core with the others carrying
/// on — which is how a `#GP` in busybox read as "busybox exited -1".
///
/// # The status is **negative**, and that is the whole difference
///
/// This passed `128 + SIGSEGV` = 139, on the reasoning that it is "the status a
/// Linux parent would see". It is not: 139 is what a **shell** prints, computed
/// from `WTERMSIG` *by the shell*. What `waitpid` reports is a *signalled*
/// status, and this tree's encoding for that is a negative exit code —
/// `encode_wait_status` turns `-11` into `WIFSIGNALED`/`WTERMSIG == 11` and
/// turns `139` into `WIFEXITED` with code 139.
///
/// So every segfault on this target was reported to its parent as a **clean
/// exit**, and the difference is not cosmetic: `waitpid`-based supervision
/// cannot tell a crash from a program that chose to exit 139, and
/// `userspace/forktest/c_stress/eager_mprotect_probe.c` — whose entire job is
/// to assert that an `mprotect` downgrade produces a `SIGSEGV` — could never
/// pass, which `amd64_mem_trials.py` recorded as an expected failure.
fn user_fault(vector: &str, frame: &InterruptStackFrame, error_code: Option<u64>) -> ! {
    serial::puts("\n[Fault] ");
    serial::puts(vector);
    serial::puts(" in ring 3 on cpu ");
    serial::put_dec(crate::smp::cpu_index() as u64);
    if let Some(code) = error_code {
        serial::puts(" err=0x");
        serial::put_hex(code);
    }
    serial::puts(" rip=0x");
    serial::put_hex(frame.rip);
    serial::puts(" rsp=0x");
    serial::put_hex(frame.rsp);
    serial::puts(" cr2=0x");
    serial::put_hex(read_cr2());
    // The wrong-root diagnostic for the `cowstale` race
    // (`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` § "The open issue"):
    // which root is actually active, which task slot is published on this
    // core, and which process pid the slot maps to. A reader that sees bss as
    // zeros names its root here — compare against the fork child's.
    serial::puts(" cr3=0x");
    serial::put_hex(crate::paging::active_root());
    serial::puts(" task=");
    serial::put_dec(crate::smp::current_task() as u64);
    serial::puts(" pid=");
    serial::put_dec(u64::from(crate::usermode::current_pid()));
    serial::puts(" — killing the process\n");
    crate::usermode::kill_current_from_fault(SIGSEGV_STATUS);
}

/// The signal a ring-3 fault raises.
const SIGSEGV: u32 = 11;

/// The exit status a fault-killed process leaves with: `-SIGSEGV`, the tree's
/// "killed by signal" encoding. See [`user_fault`] for why it is negative.
const SIGSEGV_STATUS: u64 = -(SIGSEGV as i64) as u64;

/// `#GP` — fatal, except inside the user-copy loop.
///
/// A **non-canonical** address (bit 47 not sign-extended into 48..63) is not a
/// page fault: the CPU rejects it before translation, as `#GP` with error code
/// 0. `crate::uaccess::range_ok` refuses such pointers before any copy, so this
/// arm is the second line — a direct `copy_from_user_safe` caller, or a bug in
/// the range check, still gets `EFAULT` and not a halt. Same stub, same frame,
/// same fixup query as [`page_fault_dispatch`]; no demand paging, because a
/// `#GP` is never "not mapped yet".
#[unsafe(no_mangle)]
extern "C" fn general_protection_dispatch(frame: *mut PageFaultFrame, regs: *mut TrapRegs) {
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    // SAFETY: as `page_fault_dispatch`.
    let pf = unsafe { &mut *frame };
    if let Some(fixup) = akuma_user_access::user_copy_fixup(pf.frame.rip) {
        COPY_FIXUPS.fetch_add(1, Ordering::Relaxed);
        pf.frame.rip = fixup;
        return;
    }
    if pf.frame.cs & 3 == 3 {
        // The same `SIGSEGV` route as a `#PF`, with `SI_KERNEL` and no address:
        // a `#GP` has no faulting *address* to report (the CPU rejected the
        // operand before translation), and Linux reports it the same way.
        // SAFETY: as in `page_fault_dispatch`, and the BKL for the same reason.
        let regs = unsafe { &mut *regs };
        let took = !crate::smp::bkl_held();
        if took {
            crate::smp::bkl_enter();
        }
        let delivered = crate::signal::deliver_fault_signal(
            &mut pf.frame, regs, SIGSEGV, crate::signal::segv::SI_KERNEL, 0,
        );
        if took {
            crate::smp::bkl_leave();
        }
        if delivered {
            return;
        }
        user_fault("#GP general protection", &pf.frame, Some(pf.error_code));
    }
    fatal("#GP general protection", &pf.frame, Some(pf.error_code));
}

unsafe extern "C" {
    /// The LAPIC timer entry point, installed in the IDT by `lapic::init`.
    fn timer_entry();
}

/// The LAPIC timer vector's body, called by [`timer_entry`] with the frame.
///
/// Counts, acknowledges, and may **switch tasks** — the switch happens here, on
/// the interrupted task's own trap stack, which is what makes preemption
/// preemption. Everything it does is bounded and allocation-free; a handler runs
/// with `IF` clear (these are interrupt gates, not trap gates), so it cannot
/// nest.
///
/// Whether to preempt is decided by where the tick landed, and the saved `CS`
/// says where: ring 3, or the idle loop, may be switched away from; other
/// kernel code is only asked (`need_resched`) and switches at its next yield.
/// See `sched::preempt_if_needed` for why.
#[unsafe(no_mangle)]
extern "C" fn timer_dispatch(frame: *mut InterruptStackFrame, regs: *mut TrapRegs) {
    // BKL-hold attribution: a tick that lands mid-hold is the IRQ/scheduler,
    // not the interrupted thread (transient stamp; the thread's tag survives).
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_IRQ,
    );
    // SAFETY: the stub passes pointers into the current stack, to the frame the
    // CPU just pushed and to its own register block; both are live until
    // `iretq`.
    let from_user = unsafe { (*frame).cs & 3 } == 3;
    // A tick can land inside a `stac` window; the scheduler must not inherit it.
    crate::uaccess::clac_if_enabled();
    crate::lapic::on_tick();
    // **Pending signals, for a tick that interrupted ring 3.** The third and
    // last place a signal is looked at, after a syscall return and a fault —
    // and the one that reaches a program doing neither.
    //
    // `from_user` is the gate and it is load-bearing rather than an
    // optimisation: the interrupted code is then provably not holding the BKL,
    // so taking it here cannot deadlock against the very code it interrupted.
    // (There is no register file worth redirecting on a ring-0 tick either —
    // the frame belongs to kernel code.)
    //
    // Before `preempt_if_needed`, so the redirect is in this task's frame
    // whether or not the tick also takes it off the CPU; the switch saves and
    // restores this kernel stack, and the `iretq` below is still this task's.
    if from_user {
        let took = !crate::smp::bkl_held();
        if took {
            crate::smp::bkl_enter();
        }
        // SAFETY: as above; nothing else holds a reference to either.
        let outcome = crate::signal::deliver_pending_on_tick(
            unsafe { &mut *frame },
            unsafe { &mut *regs },
        );
        if took {
            crate::smp::bkl_leave();
        }
        // **After the release, and that is the whole reason this is not done
        // inside.** `kill_current_from_fault` takes the BKL itself and never
        // returns, so killing while still holding the bracket above would leave
        // the lock one level deep for the rest of the boot — the task unwinds
        // into `run_process`, which expects to hold it exactly once.
        if let crate::signal::TickOutcome::Fatal(sig) = outcome {
            crate::signal::kill_current_from_tick(sig);
        }
    }
    // **ITIMER_REAL / `alarm` expiry** (C3, 2026-09-12). Ungated by
    // `from_user`, unlike the signal delivery above, and that is the point: an
    // `alarm(5)` is most often set by a process that then *blocks*, so a check
    // that only ran on ticks interrupting ring 3 would never fire for the
    // caller it was written for. It is safe ungated because the work is
    // atomics plus one `try_lock` that falls back rather than spinning — see
    // `akuma_syscalls_time::wants_force_interrupt`. It only pends a signal;
    // delivery still happens at this task's own next return to ring 3.
    //
    // Before `preempt_if_needed`, so a thread this readies is a candidate for
    // the switch that follows rather than waiting a further tick.
    //
    // Called directly and not through `akuma_exec::runtime().check_itimers`,
    // which is the same function: `runtime()` is `require()` and **panics**
    // when nothing is registered, and this vector is live from
    // `boot::late_init`'s `sti` — which the self-test path reaches by more
    // than one route. The direct call degrades instead: with no clock
    // registered `uptime_us()` is `0`, no deadline is `<= 0`, and the walk is
    // a few hundred relaxed loads that find nothing.
    akuma_syscalls_glue::check_itimers();

    // Preemption. EOI has already been sent, so the LAPIC can deliver the next
    // tick to whichever task runs after this returns.
    crate::sched::preempt_if_needed(from_user);
}

/// Address of [`timer_entry`], for [`set_handler`].
///
/// A function rather than a `pub` symbol because the entry's ABI is an
/// implementation detail of this module — the caller wants "the timer entry
/// point", not a typed `extern "C" fn` it would then have to spell.
#[allow(function_casts_as_integer)]
#[must_use]
pub fn timer_interrupt_entry() -> usize {
    timer_entry as usize
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
        // The two hand-assembled entries; see the module header.
        (*idt)[13].set(general_protection_entry as usize);
        (*idt)[14].set(page_fault_entry as usize);
    }
    load();
}

/// Load the (one, shared) IDT on the calling core.
///
/// `IDTR` is a per-core register, so every AP must do this; the table it points
/// at is the same one, and by the time a secondary runs it is complete.
pub fn load() {
    // SAFETY: the table is fully built by `init` before any AP starts, and
    // `lidt` only records its address.
    unsafe {
        let idt = &raw mut IDT;
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

#[cfg(not(feature = "no-tests"))]
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

#[cfg(not(feature = "no-tests"))]
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
/// 5. A copy from a non-canonical address returns `EFAULT` — that is a `#GP`,
///    fixed up by the vector-13 stub; the `#PF` path never sees it.
/// 6. A copy out of the armed lazy region succeeds and reads zeroes — the
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
        if paging::map_page(EDGE_VA as usize, pa as u64, PteProt::KERNEL_RW, MemAttr::WriteBack) {
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

    // 5. a NON-CANONICAL source: `#GP`, not `#PF`, fixed up by the vector-13
    //     stub. `uaccess::range_ok` refuses this address before any syscall
    //     copy reaches the loop; this goes through the raw primitive on purpose
    //     to prove the second line holds too.
    const NON_CANONICAL: u64 = 0x0000_8000_0000_0000;
    // SAFETY: the source is unaddressable ON PURPOSE; the #GP handler must
    // redirect the copy loop to its trampoline.
    let r = unsafe { copy_from_user_safe(dst.as_mut_ptr(), NON_CANONICAL as *const u8, 8) };
    t.check_eq("user copy: non-canonical source returns EFAULT via #GP", r.err().unwrap_or(0), EFAULT);
    t.check(
        "uaccess: range check refuses non-canonical, null-page and wrapping ranges",
        !crate::uaccess::range_ok(NON_CANONICAL, 8)
            && !crate::uaccess::range_ok(NON_CANONICAL - 4, 8)
            && !crate::uaccess::range_ok(0x800, 8)
            && !crate::uaccess::range_ok(u64::MAX - 4, 16)
            && crate::uaccess::range_ok(NON_CANONICAL - 8, 8)
            && crate::uaccess::range_ok(0x1000, 4096),
    );

    // 6. demand paging beats fixup
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

    // Exactly four fixups: cases 2, 3, 4 (#PF) and 5 (#GP). Case 6 must not
    // have counted.
    t.check_eq(
        "user copy: exactly four faults were fixed up",
        (COPY_FIXUPS.load(Ordering::Relaxed) - fixups_before) as u64,
        4,
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
