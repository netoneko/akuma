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
//!
//! # Every hand-written entry clears `DF` (2026-09-23)
//!
//! Exception and interrupt delivery clears `TF`, `RF`, `NT`, `VM` and (through
//! an interrupt gate) `IF`. It does **not** clear the direction flag, and the
//! SysV ABI only promises `DF=0` at a *call* — so a fault delivered mid
//! `std; rep movsb; cld` (musl's x86_64 `memmove`, verbatim) enters the kernel
//! with `DF=1`, and so does an IRQ that lands inside the kernel's own
//! `memmove`, which uses the same sequence. Every `memcpy`/`memset` this kernel
//! links is a `rep` string op, so from that point on each one runs *backwards*
//! from its base: the demand-page `write_bytes` and the CoW `copy_nonoverlapping`
//! in [`page_fault_dispatch`] scribble the previous physical frame — whichever
//! heap page, page table or kernel stack that happens to be. rustc emits `cld`
//! in every `x86-interrupt` prologue for exactly this reason; the five
//! `global_asm!` entries (`fixable_exception_entry!` ×2, `timer_entry`,
//! `debug_entry`, `invalid_opcode_entry`, and `shootdown_entry` next door) did
//! not, and `syscall_entry` is covered by `DF` in `IA32_FMASK`. Each dispatcher
//! now reports if it ever runs with `DF` set (`trap_entry_df_seen`, pinned to 0
//! by `user_copy_smoke_test`). Add an entry stub, add the `cld`.
//! `docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md`.

use crate::paging::{self, MemAttr, PageFaultCode, PteProt};
use crate::phys::phys_ptr;
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::serial;
use akuma_idt_amd64::Idt;
use akuma_trap_amd64::{self as trap, Kind, Trap};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// The frames moved to `akuma-trap-amd64` with the entry stubs that build them;
// these re-exports keep `signal.rs`/`shootdown.rs` spelling them `idt::…`.
pub use akuma_trap_amd64::{InterruptStackFrame, TrapRegs};

/// The table the CPU reads. Atomics inside (`akuma-idt-amd64`), so filling it is
/// safe; *when* a gate may be installed is [`akuma_idt_amd64::Idt::set`]'s rule.
static IDT: Idt = Idt::new();

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

/// Ring-3 demand-fault cost, in TSC cycles, for the `[FAULTSTAT]` line the
/// 30 s sweep prints: `[0]` faults serviced by `mm::fault_in`, `[1]` cycles
/// spent acquiring the BKL for them, `[2]` cycles in `fault_in` itself. Relaxed
/// adds, three per fault — the question this answers (what does a fault cost,
/// and how much of it is waiting for the lock) had no instrument.
pub static FAULT_STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

#[inline(always)]
fn tsc() -> u64 {
    akuma_cpu::tsc::rdtsc()
}

/// Base of the lazily-backed test region, or 0 if none is armed.
static LAZY_BASE: AtomicU64 = AtomicU64::new(0);
/// Length in bytes of the lazily-backed region.
static LAZY_LEN: AtomicU64 = AtomicU64::new(0);

/// Page-frame bits of `CR3` — the low 12 are `PWT`/`PCD` flags, not address.
/// Matches `sched::L0_BASE_MASK` and `akuma-mmu`'s own; the freed-L0 ring is
/// keyed on the frame.
const CR3_FRAME_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// `CR2` holds the faulting linear address after a page fault.
fn read_cr2() -> u64 {
    akuma_cpu::ctlreg::cr2()
}

/// Print a stack frame and stop.
/// Set for the lifetime of a fatal exception (which is the life of the
/// machine — `fatal` never returns). Printers that can fire from other cores
/// during the dump check this and stand down.
static FATAL_IN_PROGRESS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether some core is printing a fatal exception dump.
#[must_use]
pub fn fatal_in_progress() -> bool {
    FATAL_IN_PROGRESS.load(core::sync::atomic::Ordering::Acquire)
}

/// The qword `idx` words from `rsp` (negative is below it), for the crash dump.
///
/// Callers have checked `rsp >= PHYSMAP_BASE`, so the address is in the kernel
/// window; a kernel stack is at least a page, so the 64 words either side of a live
/// stack pointer are the same stack. Volatile so nothing reorders the read into
/// the prints. This is the dump's one dereference: it runs on a dying core and
/// must not itself be able to do more than fault.
fn stack_word(rsp: u64, idx: isize) -> u64 {
    let addr = (rsp as usize).wrapping_add_signed(idx * 8);
    // SAFETY: as above.
    unsafe { (addr as *const u64).read_volatile() }
}

fn fatal(vector: &str, frame: &InterruptStackFrame, error_code: Option<u64>) -> ! {
    // Claim the console before the first byte: peer cores' `[BKL] stuck` and
    // `[TLB] stuck` prints would otherwise interleave with this dump and
    // shred the register values (both captures of the 2026-09-12 ssh-wedge
    // crash were torn past reading). No release — the caller is `!`.
    FATAL_IN_PROGRESS.store(true, core::sync::atomic::Ordering::Release);
    let _ = akuma_primitives::console::claim_console_exclusive();
    // And the port itself, byte for byte. `claim_console_exclusive` silences
    // the `safe_print!` storms; it does not silence another core that is also
    // in `fatal`, and two dumps sharing a UART interleave *inside* numbers —
    // `serial::LOCK` is best-effort and `put_hex` emits sixteen digits under one
    // acquire. If a peer claimed it first, this core has nothing to add that is
    // worth shredding the first dump for: stop.
    if serial::begin_fatal() {
        crate::halt();
    }
    serial::fatal_puts("\n[EXCEPTION] ");
    serial::fatal_puts(vector);
    if let Some(code) = error_code {
        serial::fatal_puts(" err=0x");
        serial::fatal_hex(code);
    }
    serial::fatal_puts("\n  rip=0x");
    serial::fatal_hex(frame.rip);
    serial::fatal_puts(" rsp=0x");
    serial::fatal_hex(frame.rsp);
    serial::fatal_puts("\n  cs=0x");
    serial::fatal_hex(frame.cs);
    serial::fatal_puts(" rflags=0x");
    serial::fatal_hex(frame.rflags);
    serial::fatal_puts("\n  cr2=0x");
    serial::fatal_hex(read_cr2());

    // The address space the fault happened in, and where that root came from.
    //
    // `CR3` is a bare number in the register file: nothing in a dump says
    // whether it is the kernel root, a live process's, or a page-table frame
    // the free path already handed back to the PMM. The last of those is the
    // ssh-wedge crash's shape — a ring-0 not-present fault on a kernel address
    // a `mov cr3` ago was mapped — and the three words below are what tell it
    // apart from the other two in one line: `freed=1` means the root names a
    // torn-down address space, and `slot_root` names the task slot that was
    // carrying it (`sched::hook_switch_to` installs exactly that value).
    //
    // Guarded on `percpu_installed`: `cpu_index` reads `gs:[0]`, and on a core
    // that faulted before `install_percpu` that is a dereference of 0 — a
    // second fault inside the dump, which is how evidence gets lost.
    let cr3 = crate::paging::active_root();
    serial::fatal_puts(" cr3=0x");
    serial::fatal_hex(cr3);
    serial::fatal_puts(" freed=");
    serial::fatal_dec(u64::from(akuma_mmu::l0_recently_freed(cr3 & CR3_FRAME_MASK)));
    if crate::smp::percpu_installed() {
        let slot = crate::smp::current_task();
        serial::fatal_puts("\n  core=");
        serial::fatal_dec(crate::smp::cpu_index() as u64);
        serial::fatal_puts(" task_slot=");
        serial::fatal_dec(slot as u64);
        serial::fatal_puts(" slot_root=0x");
        serial::fatal_hex(crate::sched::task_space_root(slot));
    }

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
    //
    // Gated on `PHYSMAP_BASE`, not `KERNEL_VMA`: kernel stacks are `Vec`
    // allocations reached through the physmap (`0xFFFF_8000_…`), which is
    // *below* the kernel image window, so a `>= KERNEL_VMA` test excluded every
    // stack this dump was added to read. Measured 2026-09-12 — a crash with
    // `rsp=0xffff80000080e1d8` printed no stack at all. The physmap base is the
    // real floor: below it is non-canonical or user, and both are unreadable
    // here.
    if frame.rsp >= crate::phys::PHYSMAP_BASE {
        // Eight words BELOW rsp. This was added to read the frame a switch had
        // just popped — and what it actually shows is **the CPU's own exception
        // frame**, which lands on exactly those bytes: `rip`, `cs`, `rflags`,
        // `rsp`, `ss` in order, verbatim, at `rsp-48`..`rsp-8`. That is the
        // reason a restored frame can never be recovered after the fault and
        // has to be read before the `popfq` (`akuma-threading`'s
        // `x86_check_incoming_frame`). Kept because seeing the hardware frame
        // beside the decoded one is how a torn or misaligned dump is caught —
        // the two must agree. Read low-to-high, so the last word printed is the
        // one immediately under rsp.
        serial::fatal_puts("\n  [rsp-64..rsp)");
        for i in (1..=8).rev() {
            let w = stack_word(frame.rsp, -(i as isize));
            if i % 4 == 0 {
                serial::fatal_puts("\n   ");
            }
            serial::fatal_puts(" ");
            serial::fatal_hex(w);
        }
        // Which window the stack is in, because it says which kind of stack it
        // is without a symbol lookup: `physmap` is a heap-allocated thread
        // stack, `image` is the boot/.bss one.
        serial::fatal_puts(if frame.rsp >= crate::phys::KERNEL_VMA {
            "\n  [rsp/image]"
        } else {
            "\n  [rsp/physmap]"
        });
        for i in 0..64 {
            let w = stack_word(frame.rsp, i as isize);
            if i % 4 == 0 {
                serial::fatal_puts("\n   ");
            }
            serial::fatal_puts(" ");
            serial::fatal_hex(w);
        }
    }
    // And the caller chain hunt needs the fault address untorn — print it last
    // so a torn line cannot hide it again.
    serial::fatal_puts("\n  [cr2 final]=0x");
    serial::fatal_hex(read_cr2());
    serial::fatal_puts("\n");
    crate::halt();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    // Diverging by signature: `iretq` from a double fault is not architecturally
    // defined to work, so there is nothing to return to.
    fatal("#DF double fault", &frame, Some(code));
}

extern "x86-interrupt" fn unhandled(frame: InterruptStackFrame) {
    fatal("unhandled vector", &frame, None);
}

/// One named stub per CPU exception vector, because `unhandled` names none.
///
/// An `x86-interrupt` handler is not told which vector called it, so every
/// vector without a dedicated handler printed the same four words — and a dump
/// that says `unhandled vector` is a dump that cannot tell `#DB` (a stray `TF`
/// in a restored `rflags`) from `#SS` (a bad `rsp`) from `#MF` (a bad `fxrstor`
/// area). Those are different bugs with the same four words, and the ssh-wedge
/// crash produced exactly that line.
///
/// **Two macros, because the error-code shape is not cosmetic**: for a vector
/// that pushes an error code, a handler declared without one reads the code
/// where `rip` should be and reports a fabricated frame. The split is the
/// architectural list (`#TS`, `#NP`, `#SS`, `#AC`, `#CP`, `#VC`, `#SX`), not a
/// preference.
///
/// Vectors 0/6/8/13/14 keep their own handlers and are not generated here;
/// everything at 32 and above is a device or IPI vector that `set_handler`
/// claims at wire-up time and stays on plain [`unhandled`], where a wrong one
/// means "nobody installed it" rather than "the CPU objected".
macro_rules! exception_stubs {
    ($($name:ident = $vector:literal, $label:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
                fatal($label, &frame, None);
            }
        )*
        /// Install every generated stub. Called by [`init`] after the blanket
        /// fill and before the five hand-written entries, which override the
        /// ones they share a vector with.
        ///
        /// `function_casts_as_integer` allowed for the reason [`init`] states:
        /// a handler's address in a gate descriptor is what an IDT is.
        #[allow(function_casts_as_integer)]
        fn install_exception_stubs(idt: &Idt) {
            $( idt.set($vector, $name as usize); )*
        }
    };
}

macro_rules! exception_stubs_with_code {
    ($($name:ident = $vector:literal, $label:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
                fatal($label, &frame, Some(code));
            }
        )*
        #[allow(function_casts_as_integer)]
        fn install_exception_stubs_with_code(idt: &Idt) {
            $( idt.set($vector, $name as usize); )*
        }
    };
}

exception_stubs! {
    vec_nmi =  2, "#NMI non-maskable interrupt";
    vec_of  =  4, "#OF overflow";
    vec_br  =  5, "#BR bound range";
    vec_nm  =  7, "#NM device not available";
    vec_cso =  9, "#CSO coprocessor segment overrun";
    vec_mc  = 18, "#MC machine check";
    vec_ve  = 20, "#VE virtualization";
    vec_hv  = 28, "#HV hypervisor injection";
}

exception_stubs_with_code! {
    vec_ts  = 10, "#TS invalid TSS";
    vec_cp  = 21, "#CP control protection";
    vec_vc  = 29, "#VC VMM communication";
    vec_sx  = 30, "#SX security exception";
}

// The frame types, the register block and every entry stub live in
// `akuma-trap-amd64`; what is left here is policy. A handler is a safe
// `fn(&mut Trap)` registered in `register_handlers`, and `init` points the
// vectors at the crate's stubs.

/// `RFLAGS.TF` — single-step.
const RFLAGS_TF: u64 = 1 << 8;

/// How many ring-0 `#DB`s this boot has disarmed. Non-zero means the kernel
/// restored a corrupt `rflags` from somewhere and survived it; the boot log's
/// `[DB]` lines say where.
static RING0_DEBUG_TRAPS: AtomicU64 = AtomicU64::new(0);

/// How many ring-0 `#DB` reports to print before going quiet. A single-step
/// that re-arms would otherwise fill the console with the same line and push
/// the interesting part of the log out of the ring.
const RING0_DEBUG_REPORT_LIMIT: u64 = 8;

/// Ring-0 `#DB`s taken this boot (`0` is the expected value).
#[must_use]
pub fn ring0_debug_traps() -> u64 {
    RING0_DEBUG_TRAPS.load(Ordering::Relaxed)
}

/// Dispatchers that ran with the direction flag set (`0` is the expected
/// value). See the module header: delivery does not clear `DF`, every kernel
/// `memcpy`/`memset` is a `rep` string op, and the hand-written entry stubs
/// each `cld` before calling out. This is the tripwire for the stub that
/// forgets — read once per entry, two instructions and a never-taken branch.
static TRAP_ENTRY_DF_SEEN: AtomicU64 = AtomicU64::new(0);

/// First statement of every hand-written entry's dispatcher.
#[inline]
pub fn note_trap_entry_flags() {
    // `daif::read` is raw `RFLAGS` on this target.
    if akuma_cpu::daif::read() & (1 << 10) != 0 {
        TRAP_ENTRY_DF_SEEN.fetch_add(1, Ordering::Relaxed);
    }
}

/// Entries whose dispatcher saw `DF=1` this boot (`0` is the expected value).
#[must_use]
pub fn trap_entry_df_seen() -> u64 {
    TRAP_ENTRY_DF_SEEN.load(Ordering::Relaxed)
}

/// `#DB` — and the reason this kernel needs one at all.
///
/// Vector 1 fires on a single-step (`RFLAGS.TF`), a hardware breakpoint, or a
/// task-switch trap. This target armed none of those and therefore had no
/// handler: `#DB` went to the blanket `unhandled` stub, which prints and halts
/// the core. **That made a stray `TF` bit a whole-machine kill** — and one
/// arrives: the ssh/apk wedge is `akuma_threading_x86_switch_context`'s closing
/// `popfq` restoring a saved flags word that has `TF` set, so the `ret` retires
/// and the trap lands on the resumed thread's first instruction
/// (`docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`).
///
/// Two origins, two answers:
///
/// * **Ring 3** — a program single-stepping itself, or one that set `TF`
///   through a signal frame (`signal::sanitize_rflags` permits `TF` on
///   purpose, as Linux does). That is a `SIGTRAP` for the program, never a
///   kernel event, and it was killing the machine.
/// * **Ring 0** — the kernel is not a debuggee and never sets `TF`, so this is
///   corruption. **Disarm and continue**: clear `TF` in the frame `iretq` is
///   about to restore, count it, and report the first few. Continuing is not a
///   guess — the crash's own evidence shows the *return address* on the
///   restored frame was correct and only the flags word was wrong, so the
///   resumed thread is otherwise intact. A machine that survives with a `[DB]`
///   line in the log is worth more than one that halts with the evidence
///   overwritten by its own exception frame.
///
/// `DR6` is cleared either way: hardware accumulates its bits and never clears
/// them, so a stale `BS` would misattribute the next trap.
fn debug_trap(t: &mut Trap<'_>) {
    note_trap_entry_flags();
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    let dr6 = akuma_cpu::debugreg::dr6();
    akuma_cpu::debugreg::clear_dr6();
    if t.from_ring3() {
        crate::uaccess::clac_if_enabled();
        // The BKL, for the same reason as `general_protection_trap`.
        let took = !crate::smp::bkl_held();
        if took {
            crate::smp::bkl_enter();
        }
        let delivered = crate::signal::deliver_fault_signal(
            t.frame, t.regs, SIGTRAP, crate::signal::segv::SI_KERNEL, 0,
        );
        if took {
            crate::smp::bkl_leave();
        }
        if delivered {
            return;
        }
        // No handler: Linux's default for `SIGTRAP` is to terminate, which is
        // what `user_fault` does here — the process, not the kernel.
        user_fault("#DB debug (ring 3, no handler)", t.frame, None, None, SIGSEGV);
    }
    let n = RING0_DEBUG_TRAPS.fetch_add(1, Ordering::Relaxed);
    if n < RING0_DEBUG_REPORT_LIMIT {
        serial::puts("\n[DB] ring-0 debug trap disarmed: rip=0x");
        serial::put_hex(t.frame.rip);
        serial::puts(" rflags=0x");
        serial::put_hex(t.frame.rflags);
        serial::puts(" dr6=0x");
        serial::put_hex(dr6);
        serial::puts(" core=");
        serial::put_dec(crate::smp::cpu_index() as u64);
        serial::puts(" task=");
        serial::put_dec(crate::smp::current_task() as u64);
        serial::puts("\n");
    }
    t.frame.rflags &= !RFLAGS_TF;
}

// `#UD` — vector 6, no error code, same shape as `#DB` above and for the same
// reason: a ring-3 `#UD` must be able to redirect into a `SIGILL` handler (or,
// with none installed, kill just that process), which needs the full
// `TrapRegs` and a rewritable return address. The generated `x86-interrupt`
// stub this vector used before could only halt the machine — proven live
// 2026-09-20, where `llama-server`'s OpenBLAS runtime dispatch executed an
// instruction the vCPU does not actually expose (Firecracker, 4 vCPUs) and
// took the whole guest down instead of just that one process, reproducing the
// same wedge seen on bare metal. A ring-3 `#UD` has no "disarm and continue"
// story the way a stray `#DB` does — the instruction bytes really are bad —
// so ring 0 still falls through to `fatal`.
fn invalid_opcode_trap(t: &mut Trap<'_>) {
    note_trap_entry_flags();
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    if t.from_ring3() {
        crate::uaccess::clac_if_enabled();
        if deliver_under_bkl(t, SIGILL, crate::signal::segv::SI_KERNEL, 0) {
            return;
        }
        // No handler: Linux's default for `SIGILL` is to terminate, which is
        // what `user_fault` does here — the process, not the kernel.
        user_fault("#UD invalid opcode (ring 3, no handler)", t.frame, None, Some(&*t.regs), SIGSEGV);
    }
    fatal("#UD invalid opcode", t.frame, None);
}

/// Deliver `sig` to the ring-3 code that took this trap, **under the BKL**, and say
/// whether a handler was entered.
///
/// `deliver_fault_signal` edits the process's signal state and writes a frame to
/// the user stack, which can itself demand-page or break a CoW page; a CoW break
/// broadcasts a shootdown IPI whose acknowledgement wait assumes every sender
/// holds the lock (the argument on `akuma_mmu::set_shootdown_hooks`). A trap from
/// ring 3 arrives without it, so take it for the window and drop it after; one
/// that already holds it (reentrant by owner core) is left alone. Returning
/// `false` (no handler, signal blocked, frame could not be built) is the caller's
/// cue to `user_fault`, **after** the release — `kill_current_from_fault` takes
/// the lock itself and never returns.
fn deliver_under_bkl(t: &mut Trap<'_>, sig: u32, si_code: i32, addr: u64) -> bool {
    let took = !crate::smp::bkl_held();
    if took {
        crate::smp::bkl_enter();
    }
    let delivered = crate::signal::deliver_fault_signal(t.frame, t.regs, sig, si_code, addr);
    if took {
        crate::smp::bkl_leave();
    }
    delivered
}

/// `#PF` — the only handler that can *return*, and the only one that can return
/// *somewhere else*.
///
/// Called by the `#PF` stub in `akuma-trap-amd64` with the hardware frame;
/// whatever this leaves in `frame.rip` is where `iretq` resumes. Three outcomes, in this order:
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
/// A plain function, not `x86-interrupt`: the stub already did the entry work,
/// and this must be free to edit the frame — see the module header.
fn page_fault_trap(t: &mut Trap<'_>) {
    note_trap_entry_flags();
    // BKL-hold attribution: stamp the core's cache for this transient
    // excursion (the interrupted thread keeps its own tag — see
    // `set_core_tag_transient`), so a hold inside fault service names
    // "fault" rather than the syscall that was interrupted.
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    let code = PageFaultCode::new(t.error_code.unwrap_or(0));
    let addr = read_cr2();
    let base = LAZY_BASE.load(Ordering::Relaxed);
    let len = LAZY_LEN.load(Ordering::Relaxed);

    let in_lazy = base != 0 && addr >= base && addr < base + len;

    if in_lazy && code.not_present() {
        let page = addr & !0xfff;
        // Zeroed before mapping (`alloc_zeroed`): a recycled frame otherwise leaks
        // whatever the previous owner left in it to whoever faults next. If the
        // map fails the frame drops, which returns it to the PMM.
        if let Some(frame) = crate::phys::FreshFrame::alloc_zeroed()
            && paging::map_page(page as usize, frame.pa() as u64, PteProt::KERNEL_RW, MemAttr::WriteBack)
        {
            let _ = frame.publish();
            DEMAND_FAULTS.fetch_add(1, Ordering::Relaxed);
            return;
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
    let t_wait = tsc();
    let took_bkl = !crate::smp::bkl_held();
    if took_bkl {
        crate::smp::bkl_enter();
    }
    let t_held = tsc();

    // Count the serviced fault into the current process's `[PSTATS]` record
    // (aarch64 gets this through glue's fault accounting; this target's fault
    // path is its own). One relaxed add per fault; `inc_pagefault(1)` also
    // bumps `pagefault_pages`, whose unit here is one 4 KiB page per fault.
    if let Some(p) = crate::usermode::current_process() {
        p.syscall_stats.inc_pagefault(1);
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
        let t_done = tsc();
        FAULT_STATS[0].fetch_add(1, Ordering::Relaxed);
        FAULT_STATS[1].fetch_add(t_held.wrapping_sub(t_wait), Ordering::Relaxed);
        FAULT_STATS[2].fetch_add(t_done.wrapping_sub(t_held), Ordering::Relaxed);
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

    if let Some(fixup) = akuma_user_access::user_copy_fixup(t.frame.rip) {
        COPY_FIXUPS.fetch_add(1, Ordering::Relaxed);
        t.frame.rip = fixup;
        return;
    }

    if t.from_ring3() {
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
        // **Under the BKL** (`deliver_under_bkl`): writing the signal frame to
        // the user stack can itself demand-page or break a CoW page. The nested
        // `#PF` would take it anyway (this handler is reentrant by owner core),
        // so this is belt and braces — and the belt is cheap next to a fault
        // that is about to build a 440-byte frame.
        if deliver_under_bkl(t, SIGSEGV, si_code, addr) {
            return;
        }
        describe_page_fault(code);
        user_fault("#PF page fault", t.frame, Some(code.raw()), Some(&*t.regs), SIGSEGV);
    }
    describe_page_fault(code);
    fatal("#PF page fault", t.frame, Some(code.raw()));
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
/// never has anything to lose. The *real* ledger update happens against the
/// **owner** process under the hold taken below, which is where a replaced
/// frame has to be recorded.
pub(crate) fn faulting_address_space() -> akuma_mmu::UserAddressSpace {
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

    // **The owner's address-space lock, for the whole servicing window.**
    //
    // The AArch64 handler has always done its CoW break under
    // `owner.address_space.lock()` (`bkl_guard.rs` constraint 3); this target's
    // port skipped the lock, and that was the fork-vs-sibling-thread race:
    // `fork_process`'s share pass runs with the BKL **dropped** (`no-bkl-process`
    // is compiled in on this target), holding the owner's lock across a walk
    // whose every page is `read PTE → cow_ref_inc → demote` as one atom against
    // exactly this handler. Unlocked, the two interleaved: fork read a leaf
    // naming frame X, a sibling's fault broke X (`cow_ref_dec` → freed or
    // re-shared), fork then inc'd the stale X and mapped it into the child —
    // refcounts stranded (66k frames stuck in the CoW ledger in the repro),
    // frames freed while still mapped, and the freed-then-recycled page
    // surfaced as `#PF write to cr2=0x30` / `#PF fetch from rip=0x0` under a
    // `cargo -j4` self-host build.
    //
    // Taking the same lock fork holds makes the handler's
    // read-PTE → decide → rewrite → refcount → ledger swap one critical section
    // against the share pass, which is the same serialization the AArch64 side
    // has had since the `no-bkl-process` carve-out landed. `fork`'s hold never
    // waits for the BKL (that is the carve-out's point), so a fault spinning
    // here — BKL held — cannot invert against it.
    //
    // `None` — a kernel root, ring-0 self-test, or not-yet-registered task —
    // proceeds unlocked, exactly as this function always did: there is no fork
    // walk racing those tables.
    let owner = akuma_exec::process::address_space_owner_pid_for_fault()
        .and_then(akuma_exec::process::lookup_process_shared);
    let mut owner_as = owner.map(|p| p.address_space.lock());

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
            let Some(mut fresh) = crate::phys::FreshFrame::alloc() else {
                return false; // OOM: fall through to the fatal path
            };
            // The destination is not published in any page table until the
            // `map_page_pte` below, and `pa` is the CoW source, read-only in every
            // space that maps it.
            fresh.copy_from_frame(pa);
            let writable = PteProt { write: true, ..prot };
            if !faulted.map_page_pte(page, fresh.pa(), writable, false) {
                return false; // `fresh` drops and frees the private copy
            }
            let fresh = fresh.publish();
            // **Every core, before the old frame can go.** `map_page_pte`'s
            // `invlpg` is this core's only. A sibling thread on another core
            // still holds the read-only translation to `pa` from before the
            // break, and a read through it faults on nothing: it keeps reading
            // the old frame while this thread's writes land in `fresh`, and
            // once the `cow_ref_dec` below frees `pa`, it reads whatever the
            // PMM put there next. That was kot on the trashcan (2026-09-25):
            // every `Bash` tool call is a `posix_spawn`, served here as a CoW
            // fork of a four-core tokio process, and within minutes a worker
            // read a task-state word from before its `NOTIFIED` bit was set
            // (`assertion failed: next.is_notified()`, SIGABRT) or a pointer
            // from a recycled frame (`#PF` write to `cr2=0x8`, SIGSEGV).
            // AArch64's break has always flushed `AllCores` here
            // (`akuma-exceptions`), and `flush_tlb_page`'s x86 arm names this
            // path as its caller; this is the call that was never made. The
            // guard waits for every peer's acknowledgement as it drops, so the
            // free below cannot overtake a peer that has not flushed.
            let _ = akuma_mmu::flush_tlb_page(page, akuma_mmu::TlbTarget::AllCores);
            // The old frame loses this address space. Only the last holder
            // frees it — the whole point of the count.
            if akuma_pmm::cow_ref_dec(pa) {
                akuma_pmm::free_page(pa, 0);
            }
            // The private copy is tracked by the owner's ledger so teardown
            // gives it back; the frame it replaced was removed from the ledger
            // by the same two lines. Under the hold taken at the top of this
            // function — the successor of `cow_swap_frame`, which locked the
            // *faulting thread's own* `Process` (a fresh-lock `new_shared` view
            // for a `CLONE_THREAD` sibling) after the PTE was already rewritten.
            if let Some(space) = owner_as.as_deref_mut() {
                let _ = space.remove_user_frame(akuma_mmap::PhysFrame::new(pa));
                space.track_user_frame(akuma_mmap::PhysFrame::new(fresh));
            }
            COW_COPIES.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

/// Copy-on-write outcome counters, reported by the boot self-tests.
pub static COW_COPIES: AtomicU64 = AtomicU64::new(0);
pub static COW_TAKEN: AtomicU64 = AtomicU64::new(0);
pub static COW_RETRIES: AtomicU64 = AtomicU64::new(0);

/// The faulting thread's register file, plus the bytes around every register
/// that could be a user pointer.
///
/// # Why a fault report needs the memory and not just the registers
///
/// The 2026-09-18 `-j4` failure is a **`hlt` in ring 3** — musl's `mallocng`
/// compiles `assert()` to `a_crash()`, which is `hlt`, so every `rip` this
/// target reported as a mysterious `#GP` at `ld-musl+0x46xxx` is the allocator
/// catching its own corrupt in-band metadata. Which assertion does not narrow
/// it far enough: `get_meta` checks `meta->mem == base`, and the answer that
/// separates the remaining explanations is what the bytes at `base` actually
/// **are**. All zeros means the page was replaced or re-zeroed under the
/// process; plausible-but-wrong pointers mean a write landed in a frame its
/// writer no longer owned, or a second mapping is live at the same address.
/// The registers alone cannot tell those apart, and by the time anything else
/// can look the process is dead.
///
/// Read through the page table rather than through [`crate::uaccess`]: this
/// runs on a dying thread and must not itself fault, so a VA with no
/// translation is reported as such instead of being demand-paged. The same
/// reason `translate` walks rather than trusting a shadow record.
fn dump_user_registers_and_memory(regs: &TrapRegs) {
    use core::fmt::Write as _;

    let mut w = akuma_primitives::console::StackWriter::<512>::new();
    let _ = writeln!(
        w,
        "  [regs] rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}\n\
         \x20        rsi={:#018x} rdi={:#018x} rbp={:#018x} r8 ={:#018x}\n\
         \x20        r9 ={:#018x} r10={:#018x} r11={:#018x} r12={:#018x}\n\
         \x20        r13={:#018x} r14={:#018x} r15={:#018x}",
        regs.rax, regs.rbx, regs.rcx, regs.rdx,
        regs.rsi, regs.rdi, regs.rbp, regs.r8,
        regs.r9, regs.r10, regs.r11, regs.r12,
        regs.r13, regs.r14, regs.r15,
    );
    w.flush();

    // The four `mallocng`'s `get_meta` uses: `rax`/`rdi` hold the chunk
    // pointer on the two code paths observed, `r8` the group base it derived
    // and `rcx` the `struct meta *` it read out of the group header. Dumped in
    // that order so a reader can follow the same chain the allocator did.
    let space = faulting_address_space();
    for (name, val) in [
        ("rax", regs.rax),
        ("rdi", regs.rdi),
        ("r8 ", regs.r8),
        ("rcx", regs.rcx),
    ] {
        dump_user_qwords(&space, name, val);
    }
}

/// 48 bytes around `val`, as six qwords, when `val` looks like a mapped user
/// address. Silent for anything else — a register holding a small integer is
/// the common case and printing it would bury the ones that matter.
///
/// Starts one 16-byte unit **below** the pointer, because that is where
/// `mallocng` keeps what it is about to read: the slot offset at `p-2`, the
/// group's `meta` pointer at `base-16`.
fn dump_user_qwords(space: &akuma_mmu::UserAddressSpace, name: &str, val: u64) {
    use core::fmt::Write as _;

    let start = (val as usize) & !0xf;
    let Some(start) = start.checked_sub(16) else { return };
    if !(0x1000..0x0000_8000_0000_0000).contains(&start) {
        return;
    }
    let mut w = akuma_primitives::console::StackWriter::<256>::new();
    let _ = write!(w, "  [mem] {name}={val:#x}:");
    let mut any = false;
    for i in 0..6usize {
        let va = start + i * 8;
        let page = va & !0xfff;
        match space.translate(page) {
            Some(pa) => {
                // SAFETY: `pa` is a live frame of this address space reached
                // through the physmap, and exactly eight aligned bytes inside
                // the page `translate` resolved are read.
                let q = unsafe {
                    crate::phys::phys_ptr::<u64>(((pa & !0xfff) as u64) + (va & 0xfff) as u64)
                        .read_volatile()
                };
                let _ = write!(w, " {q:016x}");
                any = true;
            }
            None => {
                let _ = write!(w, " <unmapped>");
            }
        }
    }
    let _ = writeln!(w);
    if any {
        w.flush();
    }
}

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
fn user_fault(
    vector: &str,
    frame: &InterruptStackFrame,
    error_code: Option<u64>,
    regs: Option<&TrapRegs>,
    sig: u32,
) -> ! {
    // **One flush, not twenty `puts`.** `serial::LOCK` is per call, so a peer
    // core faulting at the same moment lands between two of them and shreds
    // both lines. That is not hypothetical here: the 2026-09-18 `-j4` run
    // faulted two threads of one process on two cores at once and produced
    //
    //   [Fault] #GP general protection#GP general protection in ring 3 on cpu 1
    //   … pmm_free=1380940 fpcache_len=138094041633 …
    //
    // — two `pmm_free` values interleaved into one unusable number, in the
    // report whose whole purpose is those numbers. Same fix, same reason, as
    // `sched.rs`'s `[SWITCH NO-BKL]`.
    let mut w = akuma_primitives::console::StackWriter::<512>::new();
    let _ = core::fmt::write(&mut w, format_args!(
        "\n[Fault] {} in ring 3 on cpu {}", vector, crate::smp::cpu_index()));
    if let Some(code) = error_code {
        let _ = core::fmt::write(&mut w, format_args!(" err=0x{code:016x}"));
    }
    // The wrong-root diagnostic for the `cowstale` race
    // (`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` § "The open issue"):
    // which root is actually active, which task slot is published on this
    // core, and which process pid the slot maps to. A reader that sees bss as
    // zeros names its root here — compare against the fork child's.
    //
    // `cr2` is the faulting *address* only for a `#PF`. A `#GP` rejects its
    // operand before translation and leaves whatever the last page fault put
    // there, so it is labelled rather than presented as this fault's address.
    let _ = core::fmt::write(&mut w, format_args!(
        " rip=0x{:016x} rsp=0x{:016x} cr2=0x{:016x}{} cr3=0x{:016x} task={} pid={}\n",
        frame.rip, frame.rsp, read_cr2(),
        if error_code.is_some() && vector.starts_with("#GP") { "(stale)" } else { "" },
        crate::paging::active_root(), crate::smp::current_task(),
        crate::usermode::current_pid()));
    // Which mapping, if any, covered the faulting address? A kill that lands
    // *inside* a recorded region is the kernel's lie (the page should have
    // been there); one outside it is the program's (wild pointer, or a store
    // past a mapping's end — the `SIGBUS` shape). Named at the kill, because
    // a process that is about to die cannot be asked later. Best effort: the
    // region lock is a plain spinlock and this is the kill path already.
    {
        let fa = read_cr2() as usize;
        if let Some(p) = crate::usermode::current_process() {
            let regions = p.mmap_regions.lock();
            let mut covered = false;
            for r in akuma_mmap::regions_overlapping(&regions, fa, fa + 1) {
                if r.contains(fa) {
                    covered = true;
                    let _ = core::fmt::write(&mut w, format_args!(
                        "  [Fault-REGION] cr2 inside va={:#x} pages={} file={} sw={} prot={:?}\n",
                        r.start_va, r.pages, r.file.is_some(), r.shared_write.is_some(), r.prot));
                    // What the page table says about the page, beside what the
                    // region says it should be: a write fault the CoW handler
                    // refused is `read-only PTE, no marker`, and whether the
                    // *region* allows the write is the whole question
                    // (2026-10-09: four Chromium zygote children died on this
                    // at one `rip` and the log could not say which side was
                    // wrong). `refs` is the global CoW share count.
                    // The allocator reservations too (>= 1 GiB): a forked child
                    // whose region list lost or clipped one is how an
                    // allocator ends up writing into somebody else's pages.
                    let mut shown = 0u32;
                    for g in regions.iter().filter(|g| g.pages >= 262_144).take(6) {
                        shown += 1;
                        let _ = core::fmt::write(&mut w, format_args!(
                            "  [Fault-BIG] va={:#x} pages={} prot={:?} file={}\n",
                            g.start_va, g.pages, g.prot, g.file.is_some()));
                    }
                    let _ = core::fmt::write(&mut w, format_args!(
                        "  [Fault-BIG] {} region(s) of >= 1 GiB shown, {} regions in all\n", shown, regions.len()));
                    let uas = faulting_address_space();
                    let pg = fa & !0xfff;
                    match (uas.pte_prot(pg), uas.translate(pg)) {
                        (Some((pp, marked)), Some(pa)) => {
                            let _ = core::fmt::write(&mut w, format_args!(
                                "  [Fault-PTE] write={} exec={} user={} marked={} refs={} pa={:#x}\n",
                                pp.write, pp.exec, pp.user, marked,
                                akuma_pmm::cow_ref_get(pa & !0xfff), pa & !0xfff));
                        }
                        _ => {
                            let _ = core::fmt::write(&mut w, format_args!("  [Fault-PTE] no leaf for {pg:#x}\n"));
                        }
                    }
                }
            }
            if !covered {
                let _ = core::fmt::write(&mut w, format_args!(
                    "  [Fault-REGION] cr2 outside every region (n={})\n", regions.len()));
                for (i, r) in regions.iter().take(8).enumerate() {
                    let _ = core::fmt::write(&mut w, format_args!(
                        "  [MR{}] va={:#x} pages={} prot={:?} file={} sw={}\n",
                        i, r.start_va, r.pages, r.prot, r.file.is_some(), r.shared_write.is_some()));
                }
            }
        } else {
            let _ = core::fmt::write(&mut w, format_args!(
                "  [Fault-REGION] no current process for region check\n"));
        }
    }
    // Post-mortem at the OOM floor: a ring-3 kill under memory pressure is
    // only diagnosable from these numbers (found 2026-09-17, when the
    // file-page-cache reference leak drained the guest to `pmm_free=0` and
    // every downstream symptom — the SMP=4 `#UD`, the dead network — was
    // downstream of that). A boot with no NIC never reaches
    // `mem_watch_tick`, so the counters ride the fault line itself.
    let _ = core::fmt::write(&mut w, format_args!(
        "  [memwatch-at-kill] pmm_free={} fpcache_len={} fpcache_cap={} cow_ref_frames={} mmap_checked={} anon_races={}\n",
        akuma_pmm::free_count(), akuma_fpcache::len(), akuma_fpcache::cap(),
        akuma_pmm::cow_ref_count(),
        crate::mm::PLACEMENTS_CHECKED.load(Ordering::Relaxed),
        crate::mm::PAGE_FAULT_RACES.load(Ordering::Relaxed)));
    // Hit/miss/evict/inval: distinguishes "the cache ate the RAM" (len pinned
    // at cap, evictions churning) from "a mapper leaked refs" (len small,
    // `cow_ref_frames` huge) — the two look identical from `pmm_free=0` alone.
    akuma_fpcache::stats_line(&mut w);
    crate::shmpages::stats_line(&mut w);
    w.flush();
    if let Some(regs) = regs {
        dump_user_registers_and_memory(regs);
    }
    crate::usermode::kill_current_from_fault(sig);
}

/// The signal a ring-3 fault raises.
const SIGSEGV: u32 = 11;

/// `SIGTRAP`, the signal a ring-3 `#DB` is.
const SIGTRAP: u32 = 5;

/// `SIGILL`, the signal a ring-3 `#UD` is.
const SIGILL: u32 = 4;

/// `#GP` — fatal, except inside the user-copy loop.
///
/// A **non-canonical** address (bit 47 not sign-extended into 48..63) is not a
/// page fault: the CPU rejects it before translation, as `#GP` with error code
/// 0. `crate::uaccess::range_ok` refuses such pointers before any copy, so this
/// arm is the second line — a direct `copy_from_user_safe` caller, or a bug in
/// the range check, still gets `EFAULT` and not a halt. Same stub, same frame,
/// same fixup query as [`page_fault_dispatch`]; no demand paging, because a
/// `#GP` is never "not mapped yet".
fn general_protection_trap(t: &mut Trap<'_>) {
    note_trap_entry_flags();
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_FAULT,
    );
    if let Some(fixup) = akuma_user_access::user_copy_fixup(t.frame.rip) {
        COPY_FIXUPS.fetch_add(1, Ordering::Relaxed);
        t.frame.rip = fixup;
        return;
    }
    let code = t.error_code.unwrap_or(0);
    if t.from_ring3() {
        // The same `SIGSEGV` route as a `#PF`, with `SI_KERNEL` and no address:
        // a `#GP` has no faulting *address* to report (the CPU rejected the
        // operand before translation), and Linux reports it the same way. Under
        // the BKL for the reason `page_fault_trap` states.
        if deliver_under_bkl(t, SIGSEGV, crate::signal::segv::SI_KERNEL, 0) {
            return;
        }
        user_fault("#GP general protection", t.frame, Some(code), Some(&*t.regs), SIGSEGV);
    }
    fatal("#GP general protection", t.frame, Some(code));
}

// ---------------------------------------------------------------------------
// Ring-3-survivable exceptions (2026-09-25)
// ---------------------------------------------------------------------------
//
// `#DE`, `#NP`, `#SS`, `#AC`, `#MF` and `#XM` were generated `x86-interrupt`
// stubs that went straight to `fatal`, so a *program* taking one halted the
// machine. That was photographed on the HP box: kot's musl `free` read a
// garbage `meta` pointer from a corrupted heap group and dereferenced it as
// `cmp rcx, [rbp+0x10]` (mallocng `get_meta`). A non-canonical address with
// `rbp`/`rsp` as the base register is `#SS`, not `#GP`, so the one process's
// heap corruption took all five cores down. The dump itself was wrong as well:
// those stubs never `swapgs`, so from ring 3 `gs` was the program's and the
// `core=`/`task_slot=` line was missing.
//
// Each now enters through a stub that saves `TrapRegs` and `swapgs`es on a
// ring-3 origin, like `#UD`, and becomes the signal Linux raises for it:
// `SIGFPE` for `#DE`/`#MF`/`#XM`, `SIGBUS` for `#NP`/`#SS`/`#AC`
// (`arch/x86/kernel/traps.c`). No handler means the default action, which
// kills the process, not the kernel. From ring 0 it is still `fatal`.
//
// `#BP` is here since 2026-10-08. Its gate was DPL 0, so an `int3` from ring 3
// arrived as `#GP` (`err=0x1a`: vector 3, IDT) and became a `SIGSEGV`. Every
// Chromium `CHECK` failure is an `int3` (`__builtin_trap`/`IMMEDIATE_CRASH`),
// so each looked like a segfault, and a debugger's breakpoint could not work.
// The gate is DPL 3 now and the program gets `SIGTRAP` with `SI_KERNEL`, as
// Linux's `do_int3_user` gives it. `#OF` stays out: `into` is `#UD` in 64-bit
// mode, so nothing in ring 3 can raise vector 4.

/// `SIGBUS`, what Linux raises for a ring-3 `#NP`, `#SS` or `#AC`.
const SIGBUS: u32 = 7;
/// `SIGFPE`, what Linux raises for a ring-3 `#DE`, `#MF` or `#XM`.
const SIGFPE: u32 = 8;
/// `FPE_INTDIV`: integer divide by zero (or `INT_MIN / -1`, the same `#DE`).
const FPE_INTDIV: i32 = 1;
/// `BUS_ADRALN`: invalid address alignment.
const BUS_ADRALN: i32 = 1;

/// Shared body: ring 3 → the signal (or the process's death), ring 0 → `fatal`.
///
/// Same shape as [`invalid_opcode_trap`]: `clac`, BKL around the delivery because
/// it edits the process's signal state, and `user_fault` when the program has no
/// handler, the signal is blocked, or the frame could not be built.
fn ring3_exception(label: &str, t: &mut Trap<'_>, sig: u32, si_code: i32) {
    note_trap_entry_flags();
    if t.from_ring3() {
        akuma_bkl::sync::set_core_tag_transient(
            crate::smp::cpu_index_u32(),
            akuma_bkl::sync::HOLD_TAG_FAULT,
        );
        crate::uaccess::clac_if_enabled();
        if deliver_under_bkl(t, sig, si_code, 0) {
            return;
        }
        user_fault(label, t.frame, t.error_code, Some(&*t.regs), sig);
    }
    fatal(label, t.frame, t.error_code);
}

/// Vector 3: a ring-3 `int3` becomes `SIGTRAP` through [`ring3_exception`].
fn breakpoint_trap(t: &mut Trap<'_>) {
    ring3_exception("#BP breakpoint", t, SIGTRAP, crate::signal::segv::SI_KERNEL);
}

fn segment_not_present_trap(t: &mut Trap<'_>) {
    ring3_exception("#NP segment not present", t, SIGBUS, crate::signal::segv::SI_KERNEL);
}

fn stack_segment_trap(t: &mut Trap<'_>) {
    ring3_exception("#SS stack fault", t, SIGBUS, crate::signal::segv::SI_KERNEL);
}

fn alignment_check_trap(t: &mut Trap<'_>) {
    ring3_exception("#AC alignment check", t, SIGBUS, BUS_ADRALN);
}

fn divide_error_trap(t: &mut Trap<'_>) {
    ring3_exception("#DE divide error", t, SIGFPE, FPE_INTDIV);
}

fn x87_fp_trap(t: &mut Trap<'_>) {
    ring3_exception("#MF x87 floating point", t, SIGFPE, crate::signal::segv::SI_KERNEL);
}

fn simd_fp_trap(t: &mut Trap<'_>) {
    ring3_exception("#XM SIMD floating point", t, SIGFPE, crate::signal::segv::SI_KERNEL);
}

/// The LAPIC timer vector's body, called by the timer stub with the frame.
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
fn timer_trap(t: &mut Trap<'_>) {
    note_trap_entry_flags();
    // BKL-hold attribution: a tick that lands mid-hold is the IRQ/scheduler,
    // not the interrupted thread (transient stamp; the thread's tag survives).
    akuma_bkl::sync::set_core_tag_transient(
        crate::smp::cpu_index_u32(),
        akuma_bkl::sync::HOLD_TAG_IRQ,
    );
    let from_user = t.from_ring3();
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
        let outcome = crate::signal::deliver_pending_on_tick(t.frame, t.regs);
        if took {
            crate::smp::bkl_leave();
        }
        // **After the release, and that is the whole reason this is not done
        // inside.** `kill_current_from_fault` takes the BKL itself and never
        // returns, so killing while still holding the bracket above would leave
        // the lock one level deep for the rest of the boot — the task unwinds
        // into `run_process`, which expects to hold it exactly once.
        match outcome {
            crate::signal::TickOutcome::Fatal(sig) => crate::signal::kill_current_from_tick(sig),
            crate::signal::TickOutcome::Exit(status) => {
                crate::usermode::leave_current_from_tick(status)
            }
            _ => {}
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

/// Address of the timer stub, for [`set_handler`].
///
/// A function rather than a `pub` symbol because the stub's ABI is an
/// implementation detail of `akuma-trap-amd64` — the caller wants "the timer
/// entry point", not a typed `extern "C" fn` it would then have to spell.
#[must_use]
pub fn timer_interrupt_entry() -> usize {
    trap::entry(Kind::Timer)
}

/// Install a handler for one vector, after [`init`].
///
/// The IDT is read by the CPU on every interrupt, so this edits a live table.
/// Safe only because it is called with interrupts masked and before the vector
/// it installs can be raised — the LAPIC timer is configured *after* its handler
/// is in place.
pub fn set_handler(vector: u8, handler: usize) {
    IDT.set(vector, handler);
}

/// Register every trap handler with `akuma-trap-amd64`. Single-shot per kind.
fn register_handlers() {
    trap::register(Kind::DivideError, divide_error_trap);
    trap::register(Kind::Debug, debug_trap);
    trap::register(Kind::Breakpoint, breakpoint_trap);
    trap::register(Kind::InvalidOpcode, invalid_opcode_trap);
    trap::register(Kind::SegmentNotPresent, segment_not_present_trap);
    trap::register(Kind::StackSegment, stack_segment_trap);
    trap::register(Kind::GeneralProtection, general_protection_trap);
    trap::register(Kind::PageFault, page_fault_trap);
    trap::register(Kind::X87Fp, x87_fp_trap);
    trap::register(Kind::AlignmentCheck, alignment_check_trap);
    trap::register(Kind::SimdFp, simd_fp_trap);
    trap::register(Kind::Timer, timer_trap);
}

/// Build the IDT and load it.
///
/// `function_casts_as_integer` is allowed here deliberately. The lint exists to
/// catch a function *item* being used where its return value was meant, which is
/// almost always a bug — but putting a handler's address into a gate descriptor
/// is the one thing an IDT is. The `x86-interrupt` handlers have three
/// signatures (with and without an error code, one diverging), so spelling each
/// cast through its exact fn-pointer type would add lines of ceremony that say
/// nothing the descriptor does not already say.
///
/// Order matters in one place: every handler is registered **before** the table
/// is loaded, so no stub can run with nothing to call.
#[allow(function_casts_as_integer)]
pub fn init() {
    register_handlers();
    IDT.fill(unhandled as usize);
    install_exception_stubs(&IDT);
    install_exception_stubs_with_code(&IDT);
    IDT.set(8, double_fault as usize);
    // The hand-assembled entries; see the module header. Each kind knows its
    // vector and whether ring 3 may raise it (`#BP`, from `int3`), so adding a
    // trap is one `Kind` arm and one `register` line.
    for kind in Kind::ALL {
        let Some(vector) = kind.vector() else { continue };
        let entry = trap::entry(kind);
        if kind.user_callable() {
            IDT.set_user(vector, entry);
        } else {
            IDT.set(vector, entry);
        }
    }
    load();
}

/// Load the (one, shared) IDT on the calling core.
///
/// `IDTR` is a per-core register, so every AP must do this; the table it points
/// at is the same one, and by the time a secondary runs it is complete.
pub fn load() {
    IDT.load();
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

    // 7. the direction flag does not leak into the kernel. musl's x86_64
    //    `memmove` is `std; rep movsb; cld`, so a fault on any byte of an
    //    overlapping copy — into a CoW page, a lazy page — is delivered with
    //    `DF=1`, and delivery does not clear it. Every kernel memcpy/memset is
    //    a `rep` string op: without the stub's `cld`, the demand-page zero this
    //    fault triggers runs *backwards* out of the fresh frame's base into the
    //    previous physical frame. One asm block, so `DF` is clear again before
    //    any Rust code runs; interrupts masked, so the fault under test is the
    //    only entry taken with the flag set. The page below the fresh frame
    //    cannot be checked from here; what can is that the dispatcher never
    //    saw `DF` (the tripwire) and the fresh page is zero end to end.
    //    `docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md`.
    const DF_VA: u64 = LAZY_VA + 4096; // same tables as case 6: nothing new retained
    arm_lazy(DF_VA, 4096);
    let df_before = trap_entry_df_seen();
    let demand_before = DEMAND_FAULTS.load(Ordering::Relaxed);
    let irq = akuma_cpu::daif::save_and_mask_irq();
    let first: u8;
    // SAFETY: `DF` is set and cleared inside the one block, so the ABI's
    // "clear on exit" holds; the load is from the armed lazy region, which the
    // #PF handler maps (zeroed) before re-executing it.
    unsafe {
        core::arch::asm!(
            "std",
            "mov {b}, byte ptr [{p}]",
            "cld",
            p = in(reg) DF_VA,
            b = out(reg_byte) first,
            options(nostack),
        );
    }
    akuma_cpu::daif::restore(irq);
    disarm_lazy();
    let mut zeroed = first == 0;
    if let Some(pa) = paging::unmap_page(DF_VA as usize) {
        // SAFETY: the frame the fault just mapped, reached through the physmap,
        // not yet freed.
        let page = unsafe { core::slice::from_raw_parts(phys_ptr::<u8>(pa), 4096) };
        zeroed &= page.iter().all(|&b| b == 0);
        akuma_pmm::free_page(pa as usize, 0);
    } else {
        zeroed = false;
    }
    t.check(
        "trap entry: a fault taken with DF set was demand-paged",
        DEMAND_FAULTS.load(Ordering::Relaxed) == demand_before + 1,
    );
    t.check("trap entry: the page it zeroed is zero end to end (the zeroing ran forwards)", zeroed);
    t.check_eq(
        "trap entry: no dispatcher ran with the direction flag set",
        trap_entry_df_seen() - df_before,
        0,
    );

    // Exactly four fixups: cases 2, 3, 4 (#PF) and 5 (#GP). Case 6 must not
    // have counted.
    t.check_eq(
        "user copy: exactly four faults were fixed up",
        (COPY_FIXUPS.load(Ordering::Relaxed) - fixups_before) as u64,
        4,
    );

    // Nothing in this kernel sets `TF` or arms a debug register, so a ring-0
    // `#DB` is corruption that [`debug_dispatch`] disarmed rather than a trap
    // anybody asked for. Zero here is the assertion; the `[DB]` lines in the
    // log say where if it is not. Checked in the suite rather than only
    // counted, because the whole value of surviving the trap is that a boot
    // still reports it.
    t.check_eq("debug: no ring-0 #DB was taken", ring0_debug_traps(), 0);

    // The same assertion one layer down: a ring-0 `#DB` is usually a restored
    // `rflags` that was rewritten in memory, and the canary at the bottom of
    // every kernel and trap stack says whether an overflow is what rewrote it
    // (`sched::check_canaries`, run on both sides of every switch).
    t.check_eq(
        "debug: no kernel stack canary was overwritten",
        crate::sched::stack_canary_failures(),
        0,
    );
    t.check_eq(
        "debug: every context switch held the kernel lock",
        crate::sched::switches_without_bkl(),
        0,
    );
    // The F8 tripwire's tally. Non-zero means the address-space free gate let an
    // L0 go while a task slot still named it and the switch had to demote that
    // slot to the kernel root — the `[SWITCH FREED-CR3]` use-after-free
    // (`proposals/AMD64_SWITCH_FREED_CR3_UAF.md`). Asserted rather than printed
    // because the demotion makes the failure survivable, and a survivable
    // failure is one a boot can otherwise scroll past.
    t.check_eq(
        "debug: no switch installed a freed page-table root",
        crate::sched::freed_cr3_trips(),
        0,
    );
    // Informational, deliberately not an equality: `yield_now` taking the lock
    // for a caller that had none is the guard working, not a fault. It is
    // printed so a boot says how often the kernel's wait loops reach the
    // scheduler unlocked.
    serial::puts("  sched: yields that took the BKL for their switch: ");
    serial::put_dec(crate::sched::yields_that_took_bkl());
    serial::puts("\n");

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
