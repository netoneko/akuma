#![cfg_attr(not(test), no_std)]
//! amd64 trap entry: raw stack in, `&mut Trap` out.
//!
//! # What this replaces
//!
//! `amd64/src/idt.rs` had five hand-written assembly entries (`#PF`/`#GP` and the
//! other error-code vectors through one macro, the no-error-code vectors and the
//! LAPIC timer through hand copies), each calling an `extern "C"` dispatcher that
//! took `*mut InterruptStackFrame, *mut TrapRegs` and opened with
//! `let f = unsafe { &mut *frame };` — eighteen sites, every one vouching for the
//! same fact. The fact is not about the line; it is about the *stub*, and it is
//! true once.
//!
//! # The one contract
//!
//! > A pointer a stub in this crate hands to its thunk addresses the frame the CPU
//! > just pushed (and, for a vector with an error code, the code before it) and
//! > the register block the stub just saved: both live on the current stack, both
//! > disjoint, both exclusively the handler's until the stub's `iretq`.
//!
//! The pointers are produced by `lea` on the stack the stub itself built and are
//! consumed by the thunk beside it; they cannot be named from outside the crate
//! because the thunks are private. That privacy **is** the discharge of the
//! obligation, which is why the dereference is a safe-signature private function
//! rather than a public `unsafe fn` every caller would repeat the same comment on.
//!
//! # Handlers are registered, and safe
//!
//! A handler is a `fn(&mut Trap)` registered per [`Kind`] before the vector can
//! fire (`idt::init` does, then loads the table). It is read through
//! [`Registered`], lock-free: a trap that interrupted code holding a lock must
//! not need one to find its handler. An unregistered kind panics naming itself
//! — a boot-order bug, not a state to degrade through.
//!
//! # Stub shapes
//!
//! Two, and the difference is not cosmetic:
//!
//! | | CPU pushes | on entry `rsp` is | stub adds | frame is at |
//! |---|---|---|---|---|
//! | no error code (`#DE`, `#DB`, `#BP`, `#UD`, `#MF`, `#XM`, timer) | 40 B | `≡ 8 (mod 16)` | 120 B regs | `rsp + 120` |
//! | error code (`#NP`, `#SS`, `#GP`, `#PF`, `#AC`) | 48 B | `≡ 0 (mod 16)` | 8 B pad + 120 B regs | `rsp + 128` |
//!
//! Both leave `rsp` 16-aligned at the `call`, which System V requires. The pad is
//! pushed *first* so the register block is at `rsp + 0` and a thunk's second
//! argument is a plain `lea rsi, [rsp]`.
//!
//! Every stub also clears `DF` (delivery does not, and every kernel
//! `memcpy`/`memset` is a `rep` string op — `docs/archive/
//! AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md`) and `swapgs`es on a ring-3 origin in
//! both directions. The error-code shape additionally `clac`s when SMAP is on.
//! **Add a stub, add the `cld`.**
//!
//! # Naked functions, not `global_asm!` labels
//!
//! The stubs are `#[unsafe(naked)]` functions, so a stub's address is
//! `stub as usize` and the thunk and `SMAP_ACTIVE` are `sym` operands. The
//! previous form needed a `.global` label per stub, an `unsafe extern "C"` block to
//! name each one from Rust, and `#[unsafe(no_mangle)]` on every thunk so the
//! assembly could `call` it by string.

use core::sync::atomic::AtomicU8;

use akuma_not_even_once::Registered;

/// The frame the CPU pushes on exception entry, in push order.
///
/// Defined here rather than pulled from a crate: it is five `u64`s fixed by the
/// architecture, and `x86-interrupt` handlers receive it by value.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct InterruptStackFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// What a stub for a vector **with an error code** hands to its thunk: the error
/// code the CPU pushes, then the ordinary return frame.
///
/// Layout is fixed by the hardware and by the stub's `lea rdi, [rsp + 128]`.
/// Bits of a `#PF` code: 0 present, 1 write, 2 user, 3 reserved-bit, 4
/// instruction fetch.
#[repr(C)]
pub struct CodedFrame {
    pub error_code: u64,
    pub frame: InterruptStackFrame,
}

/// The interrupted **general-purpose register file**, saved by the stub and handed
/// to the handler.
///
/// Field order is the stub's push order reversed — `r15` is pushed last and so
/// lands at offset 0 — and it is the ABI between the assembly and Rust. The
/// `const _` block below asserts it.
///
/// # Why all fifteen
///
/// A *serviced* fault needs only the caller-saved set plus `rbp`; the dispatcher is
/// `extern "C"` and preserves the rest. **Delivering a signal needs to read them**:
/// a `SIGSEGV` handler is handed a `ucontext_t` whose `uc_mcontext` is the
/// interrupted register file, and `rt_sigreturn` puts that file back, so a
/// register the kernel never wrote down is one a handler returns into garbage.
/// By the time Rust decides to deliver, its own frames have used `rbx`/`r12`-`r15`.
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

/// Bytes of [`TrapRegs`]: what the stubs' fifteen pushes add up to.
pub const REGS_BYTES: usize = 120;

/// The stub indexes nothing by hand — it pushes and pops in a fixed order — but
/// **Rust reads this struct off that stack**, so the two orders are one fact in
/// two places. A reordered field compiles, boots, and hands a signal handler a
/// register file with two values swapped.
const _: () = {
    assert!(core::mem::offset_of!(TrapRegs, r15) == 0);
    assert!(core::mem::offset_of!(TrapRegs, rbp) == 64);
    assert!(core::mem::offset_of!(TrapRegs, rax) == 112);
    assert!(core::mem::size_of::<TrapRegs>() == REGS_BYTES);
    assert!(core::mem::size_of::<InterruptStackFrame>() == 40);
    assert!(core::mem::size_of::<CodedFrame>() == 48);
    assert!(core::mem::offset_of!(InterruptStackFrame, cs) == 8);
};

/// Non-zero once `CR4.SMAP` is set. The error-code stubs test it before their
/// `clac`, which would `#UD` on a CPU without SMAP (Haswell). Written by
/// `amd64/src/uaccess.rs`'s `init_smap`.
pub static SMAP_ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Which trap a handler is being run for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// `#DE`, vector 0.
    DivideError,
    /// `#DB`, vector 1.
    Debug,
    /// `#BP`, vector 3 — DPL 3, so ring 3's `int3` reaches it.
    Breakpoint,
    /// `#UD`, vector 6.
    InvalidOpcode,
    /// `#NP`, vector 11.
    SegmentNotPresent,
    /// `#SS`, vector 12.
    StackSegment,
    /// `#GP`, vector 13.
    GeneralProtection,
    /// `#PF`, vector 14.
    PageFault,
    /// `#MF`, vector 16.
    X87Fp,
    /// `#AC`, vector 17.
    AlignmentCheck,
    /// `#XM`, vector 19.
    SimdFp,
    /// The LAPIC timer. Its vector is chosen by `lapic::init`, not fixed here.
    Timer,
}

impl Kind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 12] = [
        Self::DivideError,
        Self::Debug,
        Self::Breakpoint,
        Self::InvalidOpcode,
        Self::SegmentNotPresent,
        Self::StackSegment,
        Self::GeneralProtection,
        Self::PageFault,
        Self::X87Fp,
        Self::AlignmentCheck,
        Self::SimdFp,
        Self::Timer,
    ];

    /// Number of kinds: the size of the handler table.
    pub const COUNT: usize = Self::ALL.len();

    /// The IDT vector this kind is architecturally bound to, or `None` for the
    /// timer, whose vector is a software choice.
    #[must_use]
    pub const fn vector(self) -> Option<u8> {
        Some(match self {
            Self::DivideError => 0,
            Self::Debug => 1,
            Self::Breakpoint => 3,
            Self::InvalidOpcode => 6,
            Self::SegmentNotPresent => 11,
            Self::StackSegment => 12,
            Self::GeneralProtection => 13,
            Self::PageFault => 14,
            Self::X87Fp => 16,
            Self::AlignmentCheck => 17,
            Self::SimdFp => 19,
            Self::Timer => return None,
        })
    }

    /// Whether the CPU pushes an error code for this vector. Decides the stub
    /// shape; it is architectural, not a preference.
    #[must_use]
    pub const fn has_error_code(self) -> bool {
        matches!(
            self,
            Self::SegmentNotPresent
                | Self::StackSegment
                | Self::GeneralProtection
                | Self::PageFault
                | Self::AlignmentCheck
        )
    }

    /// Whether ring 3 may raise this vector with `int n` (a DPL-3 gate). Only
    /// `#BP`, from `int3`.
    #[must_use]
    pub const fn user_callable(self) -> bool {
        matches!(self, Self::Breakpoint)
    }
}

/// A trap in progress.
///
/// The frame the CPU pushed, the register file the stub saved, and the error code
/// if the vector has one. Both references are the handler's alone until it
/// returns; what is left in them is what `iretq` resumes.
pub struct Trap<'a> {
    pub kind: Kind,
    /// `Some` exactly when [`Kind::has_error_code`].
    pub error_code: Option<u64>,
    pub frame: &'a mut InterruptStackFrame,
    pub regs: &'a mut TrapRegs,
}

impl Trap<'_> {
    /// Whether the interrupted code was ring 3 (the saved `CS` RPL).
    #[must_use]
    pub fn from_ring3(&self) -> bool {
        self.frame.cs & 3 == 3
    }
}

/// A trap handler. Runs with `IF` clear (these are interrupt gates), on the
/// interrupted task's kernel stack, and may rewrite `frame.rip`/`frame.rsp`/
/// `regs` to redirect where `iretq` resumes.
pub type Handler = fn(&mut Trap<'_>);

static HANDLERS: [Registered<Handler>; Kind::COUNT] = [const {
    Registered::new("akuma-trap-amd64: trap taken with no handler registered — register before loading the IDT")
}; Kind::COUNT];

/// Register the handler for `kind`. Single-shot per kind (a second call is
/// ignored, `Registered`'s rule); call before the IDT is loaded.
pub fn register(kind: Kind, handler: Handler) {
    HANDLERS[kind as usize].register(handler);
}

/// Whether `kind` has a handler.
#[must_use]
pub fn is_registered(kind: Kind) -> bool {
    HANDLERS[kind as usize].is_registered()
}

/// Build the [`Trap`] for a stub with no error code and run the handler.
///
/// Private on purpose — see the crate contract. Called only by the thunks below.
#[cfg(any(test, all(target_os = "none", target_arch = "x86_64")))]
fn enter_plain(kind: Kind, frame: *mut InterruptStackFrame, regs: *mut TrapRegs) {
    // SAFETY: the crate contract. The two pointers address disjoint ranges of the
    // stack the stub just built, live until its `iretq`.
    let (frame, regs) = unsafe { (&mut *frame, &mut *regs) };
    let mut trap = Trap { kind, error_code: None, frame, regs };
    HANDLERS[kind as usize].require()(&mut trap);
}

/// As [`enter_plain`] for a stub whose CPU frame is preceded by an error code.
#[cfg(any(test, all(target_os = "none", target_arch = "x86_64")))]
fn enter_coded(kind: Kind, frame: *mut CodedFrame, regs: *mut TrapRegs) {
    // SAFETY: as `enter_plain`; `frame` also covers the error code before the
    // return frame.
    let (coded, regs) = unsafe { (&mut *frame, &mut *regs) };
    let mut trap = Trap { kind, error_code: Some(coded.error_code), frame: &mut coded.frame, regs };
    HANDLERS[kind as usize].require()(&mut trap);
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
mod stubs {
    use super::{CodedFrame, InterruptStackFrame, Kind, SMAP_ACTIVE, TrapRegs, enter_coded, enter_plain};

    /// A stub + thunk for a vector **without** an error code.
    macro_rules! plain {
        ($kind:expr, $entry:ident, $thunk:ident) => {
            extern "C" fn $thunk(frame: *mut InterruptStackFrame, regs: *mut TrapRegs) {
                enter_plain($kind, frame, regs);
            }

            #[unsafe(naked)]
            extern "C" fn $entry() {
                core::arch::naked_asm!(
                    // On entry the CPU has pushed rip, cs, rflags, rsp, ss: 40
                    // bytes, so rsp is 8 mod 16. If the trap came from ring 3,
                    // %gs is the program's; kernel code expects its per-CPU block
                    // there. Swap it in — only then, because from ring 0 it is
                    // already in place and a second swapgs would swap it out.
                    "    test qword ptr [rsp + 8], 3",
                    "    jz 2f",
                    "    swapgs",
                    "2:",
                    // The whole register file, as `TrapRegs` read bottom-up: rax
                    // first (highest offset), r15 last (offset 0). Fifteen pushes
                    // are 120 bytes, also 8 mod 16, so the two cancel and rsp is
                    // 16-aligned at the call with no padding push.
                    "    push rax",
                    "    push rbx",
                    "    push rcx",
                    "    push rdx",
                    "    push rsi",
                    "    push rdi",
                    "    push rbp",
                    "    push r8",
                    "    push r9",
                    "    push r10",
                    "    push r11",
                    "    push r12",
                    "    push r13",
                    "    push r14",
                    "    push r15",
                    // Delivery does not clear DF; the interrupted code may have
                    // set it (musl's memmove is `std; rep movsb; cld`).
                    "    cld",
                    "    lea rdi, [rsp + 120]",      // &InterruptStackFrame
                    "    lea rsi, [rsp]",            // &TrapRegs
                    "    call {thunk}",
                    // Restore exactly what is there now: the handler may have
                    // rewritten rip/rsp and the argument registers to enter a
                    // signal handler, or switched tasks (the timer).
                    "    pop r15",
                    "    pop r14",
                    "    pop r13",
                    "    pop r12",
                    "    pop r11",
                    "    pop r10",
                    "    pop r9",
                    "    pop r8",
                    "    pop rbp",
                    "    pop rdi",
                    "    pop rsi",
                    "    pop rdx",
                    "    pop rcx",
                    "    pop rbx",
                    "    pop rax",
                    "    test qword ptr [rsp + 8], 3",
                    "    jz 3f",
                    "    swapgs",
                    "3:",
                    "    iretq",
                    thunk = sym $thunk,
                );
            }
        };
    }

    /// A stub + thunk for a vector **with** an error code.
    macro_rules! coded {
        ($kind:expr, $entry:ident, $thunk:ident) => {
            extern "C" fn $thunk(frame: *mut CodedFrame, regs: *mut TrapRegs) {
                enter_coded($kind, frame, regs);
            }

            #[unsafe(naked)]
            extern "C" fn $entry() {
                core::arch::naked_asm!(
                    // The CPU has pushed, on a 16-aligned rsp (long mode aligns
                    // before pushing, whether or not the privilege level
                    // changed): [rsp] error code, [rsp+8] rip, [rsp+16] cs,
                    // [rsp+24] rflags, [rsp+32] rsp, [rsp+40] ss. The saved CS's
                    // RPL is the ring-3 test, as in `plain`.
                    "    test qword ptr [rsp + 16], 3",
                    "    jz 2f",
                    "    swapgs",
                    "2:",
                    // Alignment padding first: fifteen registers are 120 bytes,
                    // and rsp must be 16-aligned at the call. Pushing the pad
                    // first puts the register block at [rsp + 0].
                    "    sub rsp, 8",
                    "    push rax",
                    "    push rbx",
                    "    push rcx",
                    "    push rdx",
                    "    push rsi",
                    "    push rdi",
                    "    push rbp",
                    "    push r8",
                    "    push r9",
                    "    push r10",
                    "    push r11",
                    "    push r12",
                    "    push r13",
                    "    push r14",
                    "    push r15",
                    // Hardware does NOT clear RFLAGS.AC on delivery, so a fault
                    // taken inside a `stac` window would run the handler with
                    // SMAP suspended. Clear it, but only where SMAP is on:
                    // `clac` is #UD on a CPU without it. `iretq` restores the
                    // saved rflags, so a demand-paged `rep movsb` resumes with
                    // AC set.
                    "    cmp byte ptr [rip + {smap}], 0",
                    "    je 1f",
                    "    clac",
                    "1:",
                    "    cld",
                    "    lea rdi, [rsp + 128]",      // &CodedFrame: the error code slot
                    "    lea rsi, [rsp]",            // &TrapRegs
                    "    call {thunk}",
                    "    pop r15",
                    "    pop r14",
                    "    pop r13",
                    "    pop r12",
                    "    pop r11",
                    "    pop r10",
                    "    pop r9",
                    "    pop r8",
                    "    pop rbp",
                    "    pop rdi",
                    "    pop rsi",
                    "    pop rdx",
                    "    pop rcx",
                    "    pop rbx",
                    "    pop rax",
                    "    add rsp, 8",                // the alignment padding
                    "    add rsp, 8",                // drop the error code; iretq does not
                    "    test qword ptr [rsp + 8], 3",
                    "    jz 3f",
                    "    swapgs",
                    "3:",
                    "    iretq",
                    smap = sym SMAP_ACTIVE,
                    thunk = sym $thunk,
                );
            }
        };
    }

    plain!(Kind::DivideError, divide_error_entry, divide_error_thunk);
    plain!(Kind::Debug, debug_entry, debug_thunk);
    plain!(Kind::Breakpoint, breakpoint_entry, breakpoint_thunk);
    plain!(Kind::InvalidOpcode, invalid_opcode_entry, invalid_opcode_thunk);
    plain!(Kind::X87Fp, x87_fp_entry, x87_fp_thunk);
    plain!(Kind::SimdFp, simd_fp_entry, simd_fp_thunk);
    plain!(Kind::Timer, timer_entry, timer_thunk);

    coded!(Kind::SegmentNotPresent, segment_not_present_entry, segment_not_present_thunk);
    coded!(Kind::StackSegment, stack_segment_entry, stack_segment_thunk);
    coded!(Kind::GeneralProtection, general_protection_entry, general_protection_thunk);
    coded!(Kind::PageFault, page_fault_entry, page_fault_thunk);
    coded!(Kind::AlignmentCheck, alignment_check_entry, alignment_check_thunk);

    /// Address of the stub for `kind`, for an IDT gate.
    #[allow(function_casts_as_integer)]
    pub fn entry(kind: Kind) -> usize {
        match kind {
            Kind::DivideError => divide_error_entry as usize,
            Kind::Debug => debug_entry as usize,
            Kind::Breakpoint => breakpoint_entry as usize,
            Kind::InvalidOpcode => invalid_opcode_entry as usize,
            Kind::SegmentNotPresent => segment_not_present_entry as usize,
            Kind::StackSegment => stack_segment_entry as usize,
            Kind::GeneralProtection => general_protection_entry as usize,
            Kind::PageFault => page_fault_entry as usize,
            Kind::X87Fp => x87_fp_entry as usize,
            Kind::AlignmentCheck => alignment_check_entry as usize,
            Kind::SimdFp => simd_fp_entry as usize,
            Kind::Timer => timer_entry as usize,
        }
    }
}

/// Address of the entry stub for `kind`, for an IDT gate. `0` off x86_64 bare
/// metal, where there are no stubs (host tests).
#[must_use]
pub fn entry(kind: Kind) -> usize {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    {
        stubs::entry(kind)
    }
    #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
    {
        let _ = kind;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    fn blank_regs() -> TrapRegs {
        TrapRegs {
            r15: 0, r14: 0, r13: 0, r12: 0, r11: 0, r10: 0, r9: 0, r8: 0,
            rbp: 0, rdi: 0, rsi: 0, rdx: 0, rcx: 0, rbx: 0, rax: 0,
        }
    }

    fn frame(rip: u64, cs: u64) -> InterruptStackFrame {
        InterruptStackFrame { rip, cs, rflags: 0x202, rsp: 0x7fff_0000, ss: 0x1b }
    }

    #[test]
    fn the_stub_offsets_the_assembly_hardcodes_match_the_types() {
        // `lea rdi, [rsp + 120]` for plain, `[rsp + 128]` for coded (8 pad + 120).
        assert_eq!(REGS_BYTES, 120);
        assert_eq!(REGS_BYTES + 8 + core::mem::offset_of!(CodedFrame, error_code), 128);
        assert_eq!(core::mem::offset_of!(CodedFrame, frame), 8);
        // A coded frame in memory is the plain frame shifted up by one word.
        assert_eq!(core::mem::size_of::<CodedFrame>(), core::mem::size_of::<InterruptStackFrame>() + 8);
        // 16-alignment at the call: plain = 40 + 120, coded = 48 + 8 + 120.
        assert_eq!((40 + REGS_BYTES) % 16, 0);
        assert_eq!((48 + 8 + REGS_BYTES) % 16, 0);
    }

    #[test]
    fn error_code_shape_is_the_architectural_list() {
        let with: Vec<_> = Kind::ALL.iter().filter(|k| k.has_error_code()).map(|k| k.vector().unwrap()).collect();
        assert_eq!(with, [11, 12, 13, 14, 17]);
    }

    #[test]
    fn vectors_are_distinct_and_only_the_timer_is_unbound() {
        let mut seen = [false; 256];
        for k in Kind::ALL {
            match k.vector() {
                Some(v) => {
                    assert!(!seen[usize::from(v)], "{k:?} shares vector {v}");
                    seen[usize::from(v)] = true;
                }
                None => assert_eq!(k, Kind::Timer),
            }
        }
        assert_eq!(Kind::ALL.iter().filter(|k| k.user_callable()).count(), 1);
        assert!(Kind::Breakpoint.user_callable());
    }

    #[test]
    fn all_is_indexed_by_discriminant() {
        for (i, k) in Kind::ALL.iter().enumerate() {
            assert_eq!(*k as usize, i, "HANDLERS is indexed by `kind as usize`");
        }
    }

    // Each test uses its own `Kind` so parallel threads never share a slot
    // (registration is single-shot).

    static PLAIN_SEEN: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn a_plain_trap_reaches_its_handler_and_edits_land_in_the_stack() {
        fn h(t: &mut Trap<'_>) {
            assert_eq!(t.kind, Kind::X87Fp);
            assert_eq!(t.error_code, None);
            assert!(t.from_ring3());
            PLAIN_SEEN.store(t.frame.rip, Ordering::Relaxed);
            t.frame.rip = 0xdead_beef; // redirect where iretq resumes
            t.regs.rdi = 11;
        }
        register(Kind::X87Fp, h);
        let mut f = frame(0x40_1000, 0x2b);
        let mut r = blank_regs();
        enter_plain(Kind::X87Fp, &raw mut f, &raw mut r);
        assert_eq!(PLAIN_SEEN.load(Ordering::Relaxed), 0x40_1000);
        assert_eq!(f.rip, 0xdead_beef);
        assert_eq!(r.rdi, 11);
    }

    #[test]
    fn a_coded_trap_carries_the_error_code_and_the_inner_frame() {
        fn h(t: &mut Trap<'_>) {
            assert_eq!(t.kind, Kind::AlignmentCheck);
            assert_eq!(t.error_code, Some(0x1a));
            assert!(!t.from_ring3());
            t.frame.rflags |= 1 << 8;
        }
        register(Kind::AlignmentCheck, h);
        let mut cf = CodedFrame { error_code: 0x1a, frame: frame(0xffff_8000_0000_1000, 0x08) };
        let mut r = blank_regs();
        enter_coded(Kind::AlignmentCheck, &raw mut cf, &raw mut r);
        assert_eq!(cf.frame.rflags, 0x202 | (1 << 8));
        assert_eq!(cf.error_code, 0x1a, "the code word itself is not disturbed");
    }

    #[test]
    #[should_panic(expected = "no handler registered")]
    fn an_unregistered_kind_panics_naming_the_bug() {
        let mut f = frame(0, 0x08);
        let mut r = blank_regs();
        enter_plain(Kind::SimdFp, &raw mut f, &raw mut r);
    }

    #[test]
    fn register_is_single_shot_per_kind() {
        fn first(t: &mut Trap<'_>) {
            t.regs.rax = 1;
        }
        fn second(t: &mut Trap<'_>) {
            t.regs.rax = 2;
        }
        register(Kind::Breakpoint, first);
        register(Kind::Breakpoint, second);
        assert!(is_registered(Kind::Breakpoint));
        let mut f = frame(0, 0x2b);
        let mut r = blank_regs();
        enter_plain(Kind::Breakpoint, &raw mut f, &raw mut r);
        assert_eq!(r.rax, 1, "the second registration must not win");
    }

    #[test]
    fn entry_is_zero_off_target() {
        assert_eq!(entry(Kind::PageFault), 0);
    }
}
