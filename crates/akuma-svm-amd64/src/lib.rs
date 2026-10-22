#![no_std]
#![forbid(unsafe_code)]
//! The AMD SVM (AMD-V) VMCB, as data.
//!
//! # Why this is a crate
//!
//! A hypervisor's correctness lives in wire-layout constants: one transposed
//! offset and the hardware reads the wrong register *silently*, because the
//! VMCB is untyped bytes to the CPU. Every constant below is transcribed from
//! the AMD64 Architecture Programmer's Manual Vol 2 (24593) rev 3.45 and
//! pinned by a host test that cites its table, so a wrong from-memory value
//! fails a test instead of spending a metal boot. The spec-first pass caught
//! three such values already (EFER at save-relative `0D0h` not `080h`; three
//! intercept dwords, not one; IOPM base at `040h` not `014h`).
//!
//! The layering is the tree's usual one: this crate owns the **vocabulary**
//! (offsets, bits, exit codes, segment attributes, the MSR and I/O permission
//! maps) and the **decisions** that
//! are pure functions of a VMCB's bytes — the VMRUN consistency checks of
//! §15.5.1, which the hardware answers with `VMEXIT_INVALID` and no
//! diagnostics. `amd64/src/kvm.rs` owns every `unsafe` (the two MSR writes
//! and the `VMRUN` itself); it calls [`Vmcb`] over an already zero-filled
//! page and never names a field offset of its own.
//!
//! # The one obligation the crate hands its caller
//!
//! **Unused VMCB bytes must be zero** (Table B-1 note), and the control area
//! is padded to `0400h` where the state save area begins. Construct a VMCB on
//! a zero-filled page (`FreshFrame::alloc_zeroed`) and only ever write typed
//! fields; a stale or recycled page is a guest state nobody validated.
//!
//! # Sources
//!
//! AMD APM Vol 2 (24593) rev 3.45, July 2026: Table B-1 (control area),
//! Table B-2 (state save area), §15.4 (enabling), §15.5.1 (VMRUN
//! consistency), §15.30.4 (VM_HSAVE_PA), §15.10.1 (IOPM), §15.11 (MSRPM),
//! Appendix C (exit codes). Offsets in
//! [`save`] are **relative to the save-area start** (`0400h` in the page);
//! [`ctrl`] offsets are from the page base.

pub mod acpi;
pub mod mmio;
pub mod pic;
pub mod pit;
pub mod virtio;

/// Page size of a VMCB (and of the host save area).
pub const VMCB_SIZE: usize = 4096;
/// The state save area begins here in the VMCB page (Table B-2; SEV-ES off).
pub const SAVE_AREA_BASE: usize = 0x400;

/// SVM MSRs and their bit vocabulary (§15.4, §15.30.4). The *writes* happen
/// in `akuma_cpu::msr` at the kernel call site; this is the data.
pub mod msr {
    /// `VM_CR` — SVM lock/enable controls.
    pub const VM_CR: u32 = 0xC001_0114;
    /// `VM_CR.SVMDIS` (bit 4): set means SVM is disabled; with
    /// `Fn8000_000A.EDX[SVML]` clear it is firmware-locked and cannot be
    /// unlocked by software.
    pub const VM_CR_SVMDIS: u64 = 1 << 4;
    /// `EFER` address (same register the kernel already touches for `SCE`).
    pub const EFER: u32 = 0xC000_0080;
    /// `EFER.SVME` (bit 12) — enables `VMRUN`.
    pub const EFER_SVME: u64 = 1 << 12;
    /// `EFER.LME` (bit 8) — long mode enable (guest state).
    pub const EFER_LME: u64 = 1 << 8;
    /// `EFER.LMA` (bit 10) — long mode active (guest state).
    pub const EFER_LMA: u64 = 1 << 10;
    /// `VM_HSAVE_PA` — host save area base (§15.30.4): 4 KiB-aligned
    /// physical address, low 12 bits clear, never the VMCB page.
    pub const VM_HSAVE_PA: u32 = 0xC001_0117;
}

/// Control-area offsets, from the VMCB page base (Table B-1).
pub mod ctrl {
    /// CR intercepts: 15:0 reads CR0–15, 31:16 writes.
    pub const CR_INTERCEPT: usize = 0x000;
    /// DR intercepts: same split as [`CR_INTERCEPT`].
    pub const DR_INTERCEPT: usize = 0x004;
    /// Exception intercepts: bit = vector, 0–31.
    pub const EXCP_INTERCEPT: usize = 0x008;
    /// Instruction intercept dword 0 (bits in [`intr0`]).
    pub const INSTR_INTERCEPT0: usize = 0x00C;
    /// Instruction intercept dword 1 (bits in [`intr1`]).
    pub const INSTR_INTERCEPT1: usize = 0x010;
    /// Instruction intercept dword 2 (bits in [`intr2`]).
    pub const INSTR_INTERCEPT2: usize = 0x014;
    /// Pause filter threshold (15:0).
    pub const PAUSE_FILTER_THRESHOLD: usize = 0x03C;
    /// Pause filter count (15:0).
    pub const PAUSE_FILTER_COUNT: usize = 0x03E;
    /// IOPM base physical address (bits 11:0 ignored).
    pub const IOPM_BASE: usize = 0x040;
    /// MSR permission map base physical address (bits 11:0 ignored).
    pub const MSRPM_BASE: usize = 0x048;
    /// TSC offset added to the guest's `RDTSC`.
    pub const TSC_OFFSET: usize = 0x050;
    /// Guest ASID (31:0), `TLB_CONTROL` (39:32), and the RAP bits 40–41.
    pub const GUEST_ASID: usize = 0x058;
    /// Virtual interrupt control (V_TPR, V_IRQ, ... bit 31 AVIC).
    pub const V_INTR: usize = 0x060;
    /// Interrupt shadow (bit 0) — set while `STI`/`MOV SS` blocking holds.
    pub const INT_SHADOW: usize = 0x068;
    /// Exit code of the last `#VMEXIT` (Appendix C).
    pub const EXITCODE: usize = 0x070;
    /// Exit info 1 (meaning depends on [`EXITCODE`]).
    pub const EXITINFO1: usize = 0x078;
    /// Exit info 2 (meaning depends on [`EXITCODE`]).
    pub const EXITINFO2: usize = 0x080;
    /// Exit interrupt info (for `INTR`/exception exits).
    pub const EXITINTINFO: usize = 0x088;
    /// Nested paging enable (bit 0), SEV bits, PML (bit 11).
    pub const NP_ENABLE: usize = 0x090;
    /// Virtual APIC page base (`AVIC`, bit 31 of [`V_INTR`]).
    pub const AVIC_APIC_BAR: usize = 0x098;
    /// SEV-ES GHCB page base.
    pub const GHCB: usize = 0x0A0;
    /// Event injection descriptor.
    pub const EVENTINJ: usize = 0x0A8;
    /// Guest nested page-table root.
    pub const N_CR3: usize = 0x0B0;
    /// VMCB clean bits (31:0); 0 = all dirty, which is all a seed needs.
    pub const VMCB_CLEAN: usize = 0x0C0;
    /// Next RIP, saved when the NRIPS feature is present (§15.28.x) for
    /// trap-style intercepts.
    pub const NRIP: usize = 0x0C8;
    /// Guest instruction bytes: bits 7:0 of the qword = byte count,
    /// bits 127:8 = the bytes, **last byte nearest the count**.
    pub const GUEST_INSTR: usize = 0x0D0;
    /// REQUESTED_IRR (255:0), for AVIC debugging.
    pub const REQUESTED_IRR: usize = 0x150;
    /// Exit interrupt data (`EXITINTINFO` payload).
    pub const EXITINTDATA: usize = 0x170;
    /// Event injection data payload.
    pub const EVENTINJDATA: usize = 0x178;
    /// PML page base (PML_ENABLE, [`NP_ENABLE`] bit 11).
    pub const PML_BASE: usize = 0x1C8;
    /// PML next index.
    pub const PML_INDEX: usize = 0x1D0;

    /// `TLB_CONTROL` encodings (39:32 of [`GUEST_ASID`]).
    /// `V_INTR_MASKING` — bit 24 of the `V_INTR` qword (`060h`, Table B-1).
    ///
    /// Set: the host's `EFLAGS.IF` at `VMRUN` controls *physical* interrupts
    /// while the guest runs and the guest's `IF` controls virtual ones only
    /// (§15.21.1) — so the host's timer tick (scheduler, watchdog) reaches it
    /// whatever the guest does with `IF`. Clear: the guest's `IF` gates both.
    pub const V_INTR_MASKING: u64 = 1 << 24;
    /// `V_IRQ` — bit 8 of `060h`: a virtual INTR is pending (Table B-1). The
    /// processor writes it back on `#VMEXIT`, so a cleared bit means the guest
    /// took the interrupt (§15.21.4).
    pub const V_IRQ: u64 = 1 << 8;
    /// `V_IGN_TPR` — bit 20 of `060h`: the pending virtual interrupt ignores the
    /// virtual TPR — the ExtINT shape a legacy PIC delivers (§15.21.4).
    pub const V_IGN_TPR: u64 = 1 << 20;
    /// `V_INTR_VECTOR` — bits 39:32 of `060h`: the vector the virtual interrupt
    /// is dispatched through.
    pub const V_INTR_VECTOR_SHIFT: u32 = 32;
    /// `INTERRUPT_SHADOW` — bit 0 of `068h`: the guest is in the one-instruction
    /// window after `STI` or `MOV SS` (§15.21.5).
    pub const INT_SHADOW_BIT: u64 = 1;

    /// The `V_INTR` qword for a legacy-PIC interrupt on `vector`: masking
    /// virtualised, a virtual INTR pending, TPR ignored. The guest takes it when
    /// its `RFLAGS.IF` is set and it is not in an interrupt shadow.
    #[must_use]
    pub const fn v_intr_extint(vector: u8) -> u64 {
        V_INTR_MASKING | V_IRQ | V_IGN_TPR | ((vector as u64) << V_INTR_VECTOR_SHIFT)
    }
    pub const TLB_DO_NOTHING: u8 = 0;
    /// Flush the entire TLB on `VMRUN`.
    pub const TLB_FLUSH_ALL: u8 = 1;
    /// Flush this guest's TLB entries (by ASID).
    pub const TLB_FLUSH_GUEST: u8 = 3;
    /// Flush this guest's non-global entries.
    pub const TLB_FLUSH_GUEST_NONGLOBAL: u8 = 7;
}

/// Instruction intercept bits (Table B-1, `00Ch`/`010h`/`014h`).
pub mod intr0 {
    /// Physical interrupt.
    pub const INTR: u32 = 1 << 0;
    /// NMI.
    pub const NMI: u32 = 1 << 1;
    /// SMI.
    pub const SMI: u32 = 1 << 2;
    /// INIT.
    pub const INIT: u32 = 1 << 3;
    /// Virtual interrupt.
    pub const VINTR: u32 = 1 << 4;
    /// CR0 writes other than 0→TS/MP.
    pub const CR0_WR_OTHER: u32 = 1 << 5;
    /// IDTR read.
    pub const IDTR_READ: u32 = 1 << 6;
    /// GDTR read.
    pub const GDTR_READ: u32 = 1 << 7;
    /// LDTR read.
    pub const LDTR_READ: u32 = 1 << 8;
    /// TR read.
    pub const TR_READ: u32 = 1 << 9;
    /// IDTR write.
    pub const IDTR_WRITE: u32 = 1 << 10;
    /// GDTR write.
    pub const GDTR_WRITE: u32 = 1 << 11;
    /// LDTR write.
    pub const LDTR_WRITE: u32 = 1 << 12;
    /// TR write.
    pub const TR_WRITE: u32 = 1 << 13;
    /// `RDTSC`.
    pub const RDTSC: u32 = 1 << 14;
    /// `RDPMC`.
    pub const RDPMC: u32 = 1 << 15;
    /// `PUSHF`.
    pub const PUSHF: u32 = 1 << 16;
    /// `POPF`.
    pub const POPF: u32 = 1 << 17;
    /// `CPUID`.
    pub const CPUID: u32 = 1 << 18;
    /// `RSM`.
    pub const RSM: u32 = 1 << 19;
    /// `IRET`.
    pub const IRET: u32 = 1 << 20;
    /// `INTn` (software interrupt).
    pub const INTN: u32 = 1 << 21;
    /// `INVD`.
    pub const INVD: u32 = 1 << 22;
    /// `PAUSE`.
    pub const PAUSE: u32 = 1 << 23;
    /// `HLT`.
    pub const HLT: u32 = 1 << 24;
    /// `INVLPG`.
    pub const INVLPG: u32 = 1 << 25;
    /// `INVLPGA`.
    pub const INVLPGA: u32 = 1 << 26;
    /// I/O instruction protection (via the IOPM).
    pub const IOIO_PROT: u32 = 1 << 27;
    /// MSR protection (via the MSRPM).
    pub const MSR_PROT: u32 = 1 << 28;
    /// Task switch.
    pub const TASK_SWITCH: u32 = 1 << 29;
    /// FERR_FREEZE.
    pub const FERR_FREEZE: u32 = 1 << 30;
    /// Shutdown.
    pub const SHUTDOWN: u32 = 1 << 31;
}

/// Instruction intercept dword 1 bits (`010h`).
pub mod intr1 {
    /// **Must be set** — §15.5.1 makes a clear VMRUN-intercept bit
    /// `VMEXIT_INVALID`.
    pub const VMRUN: u32 = 1 << 0;
    /// `VMMCALL`.
    pub const VMMCALL: u32 = 1 << 1;
    /// `VMLOAD`.
    pub const VMLOAD: u32 = 1 << 2;
    /// `VMSAVE`.
    pub const VMSAVE: u32 = 1 << 3;
    /// `STGI`.
    pub const STGI: u32 = 1 << 4;
    /// `CLGI`.
    pub const CLGI: u32 = 1 << 5;
    /// `SKINIT`.
    pub const SKINIT: u32 = 1 << 6;
    /// `RDTSCP`.
    pub const RDTSCP: u32 = 1 << 7;
    /// `ICEBP`/`INT1`.
    pub const ICEBP: u32 = 1 << 8;
    /// `WBINVD`.
    pub const WBINVD: u32 = 1 << 9;
    /// `MONITOR`.
    pub const MONITOR: u32 = 1 << 10;
    /// `MWAIT` (armed).
    pub const MWAIT_ARMED: u32 = 1 << 11;
    /// `MWAIT` (not armed).
    pub const MWAIT_UNARMED: u32 = 1 << 12;
    /// `XSETBV`.
    pub const XSETBV: u32 = 1 << 13;
    /// `RDPRU`.
    pub const RDPRU: u32 = 1 << 14;
    /// EFER-write trap.
    pub const EFER_WRITE: u32 = 1 << 15;
    /// CR0-write trap.
    pub const CR0_WRITE: u32 = 1 << 16;
    /// CR3-write trap.
    pub const CR3_WRITE: u32 = 1 << 19;
    /// CR4-write trap.
    pub const CR4_WRITE: u32 = 1 << 20;
}

/// Instruction intercept dword 2 bits (`014h`) — newer TLB/busclock knobs,
/// listed for completeness; a seed sets none.
pub mod intr2 {
    /// `INVLPGB` (all).
    pub const INVLPGB_ALL: u32 = 1 << 0;
    /// `INVLPG` illegal form.
    pub const INVLPGB_ILLEGAL: u32 = 1 << 1;
    /// `INVPCID`.
    pub const INVPCID: u32 = 1 << 2;
    /// `MCOMMIT`.
    pub const MCOMMIT: u32 = 1 << 3;
    /// `TLBSYNC`.
    pub const TLBSYNC: u32 = 1 << 4;
    /// Bus lock threshold.
    pub const BUSLOCK_THRESHOLD: u32 = 1 << 5;
    /// HLT when no virtual interrupt is pending.
    pub const HLT_NO_VINTR: u32 = 1 << 6;
}

/// Exception intercept bits (byte-wide vectors in `008h`, as `u32`).
pub mod excp {
    /// #DB.
    pub const DB: u32 = 1 << 1;
    /// #UD.
    pub const UD: u32 = 1 << 6;
    /// #DF.
    pub const DF: u32 = 1 << 8;
    /// #GP.
    pub const GP: u32 = 1 << 13;
    /// #PF.
    pub const PF: u32 = 1 << 14;
}

/// `EVENTINJ` (APM Vol 2 §15.20, Figure 15-5, rev 3.45, p. 553): an event for
/// the next `VMRUN` to deliver to the guest before its first instruction.
pub mod eventinj {
    /// `V` — bit 31: an event is to be injected.
    pub const VALID: u64 = 1 << 31;
    /// `EV` — bit 11: the exception pushes an error code.
    pub const ERROR_CODE_VALID: u64 = 1 << 11;
    /// `TYPE` — bits 10:8, value 3: exception (fault or trap) — Table 15-11.
    pub const TYPE_EXCEPTION: u64 = 3 << 8;

    /// An exception `vector`, with an `error` code if the vector pushes one
    /// (`ERRORCODE` bits 63:32). The vector must be a real exception, not 2
    /// (an NMI): `VMRUN` answers an illegal event with `VMEXIT_INVALID`.
    #[must_use]
    pub const fn exception(vector: u8, error: Option<u32>) -> u64 {
        let base = VALID | TYPE_EXCEPTION | vector as u64;
        match error {
            Some(code) => base | ERROR_CODE_VALID | ((code as u64) << 32),
            None => base,
        }
    }
}

/// State save area offsets, **relative to `0400h`** in the VMCB page
/// (Table B-2).
pub mod save {
    /// ES descriptor, 16 B: selector @+0, attrib @+2, limit @+4, base @+8.
    pub const ES: usize = 0x000;
    /// CS descriptor (same 16 B layout).
    pub const CS: usize = 0x010;
    /// SS descriptor.
    pub const SS: usize = 0x020;
    /// DS descriptor — base is the **lower 32 bits only** in this table.
    pub const DS: usize = 0x030;
    /// FS descriptor.
    pub const FS: usize = 0x040;
    /// GS descriptor — full 64-bit base.
    pub const GS: usize = 0x050;
    /// GDTR (limit `+4` lower 16 bits, base `+8`).
    pub const GDTR: usize = 0x060;
    /// LDTR.
    pub const LDTR: usize = 0x070;
    /// IDTR (limit `+4`, base `+8`).
    pub const IDTR: usize = 0x080;
    /// TR (base `+8`).
    pub const TR: usize = 0x090;
    /// Guest CPL, one byte. **CPL comes from here, not from CS.DPL.**
    pub const CPL: usize = 0x0CB;
    /// Guest EFER.
    pub const EFER: usize = 0x0D0;
    /// Guest CR4.
    pub const CR4: usize = 0x148;
    /// Guest CR3.
    pub const CR3: usize = 0x150;
    /// Guest CR0.
    pub const CR0: usize = 0x158;
    /// Guest DR7.
    pub const DR7: usize = 0x160;
    /// Guest DR6.
    pub const DR6: usize = 0x168;
    /// Guest RFLAGS.
    pub const RFLAGS: usize = 0x170;
    /// Guest RIP.
    pub const RIP: usize = 0x178;
    /// Guest RSP.
    pub const RSP: usize = 0x1D8;
    /// Guest RAX.
    pub const RAX: usize = 0x1F8;
    /// Guest `STAR` (Table B-2, APM Vol 2 rev 3.45, p. 771).
    pub const STAR: usize = 0x200;
    /// Guest `LSTAR`.
    pub const LSTAR: usize = 0x208;
    /// Guest `CSTAR`.
    pub const CSTAR: usize = 0x210;
    /// Guest `SFMASK`.
    pub const SFMASK: usize = 0x218;
    /// Guest `KernelGsBase` (what `SWAPGS` exchanges).
    pub const KERNEL_GS_BASE: usize = 0x220;
    /// Guest `SYSENTER_CS`.
    pub const SYSENTER_CS: usize = 0x228;
    /// Guest `SYSENTER_ESP`.
    pub const SYSENTER_ESP: usize = 0x230;
    /// Guest `SYSENTER_EIP`.
    pub const SYSENTER_EIP: usize = 0x238;
    /// Guest CR2.
    pub const CR2: usize = 0x240;
    /// Guest PAT.
    pub const G_PAT: usize = 0x268;
    /// `LASTEXCPFROM` (Table B-2: `288h`, verified against rev 3.45).
    pub const LASTEXCPFROM: usize = 0x288;
}

/// Segment attribute word, 12 bits = descriptor bits 55:52 | 47:40
/// (Table B-2 note). A NULL segment is an all-zero attribute (`P = 0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SegAttr(pub u16);

impl SegAttr {
    /// Descriptor type field (bits 3:0).
    #[must_use]
    pub const fn ty(self) -> u8 {
        (self.0 & 0xF) as u8
    }
    /// S bit (bit 4): 1 = code/data, 0 = system.
    #[must_use]
    pub const fn is_code_data(self) -> bool {
        self.0 & (1 << 4) != 0
    }
    /// DPL (bits 6:5). The hardware observes it for DS/ES/FS/GS; the **guest
    /// CPL is the separate byte at save-relative `0CBh`**.
    #[must_use]
    pub const fn dpl(self) -> u8 {
        ((self.0 >> 5) & 3) as u8
    }
    /// Present (bit 7).
    #[must_use]
    pub const fn is_present(self) -> bool {
        self.0 & (1 << 7) != 0
    }
    /// L bit (bit 9): 64-bit code segment.
    #[must_use]
    pub const fn is_long(self) -> bool {
        self.0 & (1 << 9) != 0
    }
    /// D/B bit (bit 10): 32-bit default size.
    #[must_use]
    pub const fn is_db(self) -> bool {
        self.0 & (1 << 10) != 0
    }
    /// Granularity (bit 11).
    #[must_use]
    pub const fn is_4k_granular(self) -> bool {
        self.0 & (1 << 11) != 0
    }
    /// Assemble from descriptor fields: `ty` (4 bits), `s`, `dpl`, `p`,
    /// `avl`, `l`, `db`, `g`.
    #[must_use]
    #[allow(clippy::many_single_char_names, clippy::fn_params_excessive_bools)]
    // The descriptor's own layout is eight one-letter fields (APM Vol 3
    // "Segment Descriptor"); the names mirror the spec, not Rust style.
    pub const fn from_fields(ty: u8, s: bool, dpl: u8, p: bool, avl: bool, l: bool, db: bool, g: bool) -> Self {
        let mut v = (ty & 0xF) as u16;
        if s {
            v |= 1 << 4;
        }
        v |= ((dpl & 3) as u16) << 5;
        if p {
            v |= 1 << 7;
        }
        if avl {
            v |= 1 << 8;
        }
        if l {
            v |= 1 << 9;
        }
        if db {
            v |= 1 << 10;
        }
        if g {
            v |= 1 << 11;
        }
        Self(v)
    }
    /// 32-bit flat code, DPL 0, accessed: type `B`, S=1, P=1, DB=1, G=1.
    /// G=1 pairs with limit `0xFFFFF` to make the limit 4 GiB — with G=0
    /// the raw 32-bit limit field tops out at 1 MiB, which is not flat.
    pub const CODE32_DPL0: Self = Self::from_fields(0xB, true, 0, true, false, false, true, true);
    /// 32-bit read/write data, DPL 0, accessed: type `3`, S=1, P=1, G=1
    /// (the phase-0 guest's SS/DS/ES; same G=1/limit `0xFFFFF` pairing).
    pub const DATA32_RW_DPL0: Self = Self::from_fields(0x3, true, 0, true, false, false, false, true);
    /// 64-bit code, DPL 0, accessed: type `B`, S=1, P=1, **L=1, D=0**.
    /// With paging on the two together are the long-mode state §15.5.1
    /// demands (`LongModeLAndD` rejects L and D set at once); limit and G
    /// are ignored in 64-bit mode and left clear.
    pub const CODE64_DPL0: Self = Self::from_fields(0xB, true, 0, true, false, true, false, false);
}

/// One decoded segment register from the save area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seg {
    /// Selector as loaded (unmasked).
    pub selector: u16,
    /// Attribute word (see [`SegAttr`]; NULL segment = all zero).
    pub attr: SegAttr,
    /// Limit.
    pub limit: u32,
    /// Base (full 64 bits; note Table B-2 records DS's base as its lower
    /// 32 bits).
    pub base: u64,
}

/// `#VMEXIT` exit codes, Appendix C. Only the ones the seed can actually see
/// are named; anything else decodes to [`ExitCode::Other`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitCode {
    /// `#DB` (vector 1) — exception exits are `040h + vector`.
    Db,
    /// `#UD` (vector 6).
    Ud,
    /// `#GP` (vector 13).
    Gp,
    /// `#PF` (vector 14).
    Pf,
    /// VINTR.
    Vintr,
    /// `CPUID`.
    Cpuid,
    /// `PAUSE` — hardware **advances RIP** on this exit.
    Pause,
    /// `HLT` — hardware **advances RIP** on this exit.
    Hlt,
    /// I/O instruction.
    Ioio,
    /// MSR.
    Msr,
    /// `VMRUN`.
    Vmrun,
    /// `VMMCALL`.
    Vmmcall,
    /// `RDTSCP`.
    Rdtscp,
    /// Nested page fault.
    Npf,
    /// `VMEXIT_INVALID`: a VMRUN consistency check failed (§15.5.1). The
    /// checks that produced it are in [`validate_guest_state`].
    Invalid,
    /// `VMEXIT_BUSY` (unlaunched VMCB in use).
    Busy,
    /// Triple fault — the guest took an exception with no handler and gave
    /// up (Appendix C `7Fh`). A guest under construction hits this instead of
    /// wedging the machine.
    Shutdown,
    /// Anything else — value as in Appendix C.
    Other(u64),
}

impl ExitCode {
    /// Decode Appendix C.
    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        match v {
            0x40 | 0x41 => Self::Db,
            0x46 => Self::Ud,
            0x4D => Self::Gp,
            0x4E => Self::Pf,
            0x64 => Self::Vintr,
            0x72 => Self::Cpuid,
            0x77 => Self::Pause,
            0x78 => Self::Hlt,
            0x7B => Self::Ioio,
            0x7C => Self::Msr,
            0x80 => Self::Vmrun,
            0x81 => Self::Vmmcall,
            0x87 => Self::Rdtscp,
            0x400 => Self::Npf,
            0x7F => Self::Shutdown,
            0xFFFF_FFFF => Self::Invalid,
            0xFFFF_FFFE => Self::Busy,
            other => Self::Other(other),
        }
    }
}

/// A VMCB page, typed.
///
/// Construct over an **already zero-filled** page; see the crate header for
/// why. All accessors are bounds-checked indexing into the page (which is
/// exactly `[u8; 4096]`, so the const offsets compile to addresses and
/// cannot fail at runtime).
#[derive(Debug)]
pub struct Vmcb<'a> {
    page: &'a mut [u8; VMCB_SIZE],
}

#[must_use]
pub const fn rd16(page: &[u8; VMCB_SIZE], off: usize) -> u16 {
    u16::from_le_bytes([page[off], page[off + 1]])
}

#[must_use]
pub const fn rd32(page: &[u8; VMCB_SIZE], off: usize) -> u32 {
    u32::from_le_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]])
}

#[must_use]
pub const fn rd64(page: &[u8; VMCB_SIZE], off: usize) -> u64 {
    u64::from_le_bytes([
        page[off],
        page[off + 1],
        page[off + 2],
        page[off + 3],
        page[off + 4],
        page[off + 5],
        page[off + 6],
        page[off + 7],
    ])
}

pub fn wr16(page: &mut [u8; VMCB_SIZE], off: usize, v: u16) {
    page[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn wr32(page: &mut [u8; VMCB_SIZE], off: usize, v: u32) {
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn wr64(page: &mut [u8; VMCB_SIZE], off: usize, v: u64) {
    page[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

impl<'a> Vmcb<'a> {
    /// Type a (zero-filled) page as a VMCB.
    pub const fn new(page: &'a mut [u8; VMCB_SIZE]) -> Self {
        Self { page }
    }

    /// The raw page (for logging a hexdump of the exit state).
    #[must_use]
    pub const fn page(&self) -> &[u8; VMCB_SIZE] {
        self.page
    }

    // ---- control area ----

    /// Set instruction intercept dword 0 (`00Ch`) wholesale.
    pub fn set_intr0(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT0, bits);
    }
    /// Set instruction intercept dword 1 (`010h`) wholesale.
    pub fn set_intr1(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT1, bits);
    }
    /// Set instruction intercept dword 2 (`014h`) wholesale.
    pub fn set_intr2(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT2, bits);
    }
    /// Set the exception intercept bitmap (`008h`): bit = vector.
    pub fn set_excp(&mut self, bits: u32) {
        wr32(self.page, ctrl::EXCP_INTERCEPT, bits);
    }
    /// Exception intercept bitmap, for post-mortem.
    #[must_use]
    pub const fn excp(&self) -> u32 {
        rd32(self.page, ctrl::EXCP_INTERCEPT)
    }
    /// Guest ASID. **0 is illegal** (§15.5.1) and is rejected by
    /// [`validate_guest_state`].
    pub fn set_asid(&mut self, asid: u32) {
        wr32(self.page, ctrl::GUEST_ASID, asid);
    }
    /// Guest ASID.
    #[must_use]
    pub const fn asid(&self) -> u32 {
        rd32(self.page, ctrl::GUEST_ASID)
    }
    /// `TLB_CONTROL` (39:32 of `058h`).
    pub fn set_tlb_control(&mut self, v: u8) {
        self.page[ctrl::GUEST_ASID + 4] = v;
    }
    /// Nested paging enable (`090h` bit 0).
    pub fn set_np_enable(&mut self, on: bool) {
        let b = rd32(self.page, ctrl::NP_ENABLE);
        wr32(self.page, ctrl::NP_ENABLE, if on { b | 1 } else { b & !1 });
    }
    /// Nested paging enable.
    #[must_use]
    pub const fn np_enable(&self) -> bool {
        rd32(self.page, ctrl::NP_ENABLE) & 1 != 0
    }
    /// Guest nested page-table root (`0B0h`).
    pub fn set_n_cr3(&mut self, pa: u64) {
        wr64(self.page, ctrl::N_CR3, pa);
    }
    /// Guest nested page-table root.
    #[must_use]
    pub const fn n_cr3(&self) -> u64 {
        rd64(self.page, ctrl::N_CR3)
    }
    /// IOPM base (bits 11:0 are ignored by the hardware; caller aligns).
    pub fn set_iopm_base(&mut self, pa: u64) {
        wr64(self.page, ctrl::IOPM_BASE, pa);
    }
    /// MSRPM base (bits 11:0 ignored).
    pub fn set_msrpm_base(&mut self, pa: u64) {
        wr64(self.page, ctrl::MSRPM_BASE, pa);
    }
    /// Exit code of the last `#VMEXIT`.
    #[must_use]
    pub const fn exitcode(&self) -> u64 {
        rd64(self.page, ctrl::EXITCODE)
    }
    /// Exit info 1.
    #[must_use]
    pub const fn exitinfo1(&self) -> u64 {
        rd64(self.page, ctrl::EXITINFO1)
    }
    /// Exit info 2.
    #[must_use]
    pub const fn exitinfo2(&self) -> u64 {
        rd64(self.page, ctrl::EXITINFO2)
    }
    /// Exit interrupt info.
    #[must_use]
    pub const fn exitintinfo(&self) -> u64 {
        rd64(self.page, ctrl::EXITINTINFO)
    }
    /// The `V_INTR` qword (`060h`) as the last `#VMEXIT` left it.
    #[must_use]
    pub const fn v_intr(&self) -> u64 {
        rd64(self.page, ctrl::V_INTR)
    }
    /// Whether the guest is in an interrupt shadow (`068h` bit 0).
    #[must_use]
    pub const fn interrupt_shadow(&self) -> bool {
        rd64(self.page, ctrl::INT_SHADOW) & ctrl::INT_SHADOW_BIT != 0
    }
    /// The `EVENTINJ` qword (`0A8h`) as it stands.
    #[must_use]
    pub const fn eventinj(&self) -> u64 {
        rd64(self.page, ctrl::EVENTINJ)
    }
    /// Next RIP (`0C8h`), saved with NRIPS; 0 before the first run.
    #[must_use]
    pub const fn nrip(&self) -> u64 {
        rd64(self.page, ctrl::NRIP)
    }
    /// Guest instruction bytes captured at the exit: `(count, bytes)` with
    /// the **last** instruction byte at `bytes[0]` (bits 127:8 run backwards
    /// from the count field).
    #[must_use]
    pub const fn guest_instr(&self) -> (usize, [u8; 15]) {
        let cnt = (rd64(self.page, ctrl::GUEST_INSTR) & 0xFF) as usize;
        let mut bytes = [0u8; 15];
        let mut i = 0;
        while i < 15 {
            bytes[i] = self.page[ctrl::GUEST_INSTR + 1 + i];
            i += 1;
        }
        (cnt, bytes)
    }

    // ---- state save area ----

    /// Read a segment descriptor.
    #[must_use]
    pub const fn seg(&self, off: usize) -> Seg {
        let base = SAVE_AREA_BASE + off;
        Seg {
            selector: rd16(self.page, base),
            attr: SegAttr(rd16(self.page, base + 2)),
            limit: rd32(self.page, base + 4),
            base: rd64(self.page, base + 8),
        }
    }
    /// Write a segment descriptor.
    pub fn set_seg(&mut self, off: usize, seg: Seg) {
        let base = SAVE_AREA_BASE + off;
        wr16(self.page, base, seg.selector);
        wr16(self.page, base + 2, seg.attr.0);
        wr32(self.page, base + 4, seg.limit);
        wr64(self.page, base + 8, seg.base);
    }
    /// Guest CPL (save-relative `0CBh` — not CS.DPL).
    pub const fn set_cpl(&mut self, cpl: u8) {
        self.page[SAVE_AREA_BASE + save::CPL] = cpl;
    }
    /// Guest CPL.
    #[must_use]
    pub const fn cpl(&self) -> u8 {
        self.page[SAVE_AREA_BASE + save::CPL]
    }
    /// Guest EFER.
    pub fn set_efer(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::EFER, v);
    }
    /// Guest EFER.
    #[must_use]
    pub const fn efer(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::EFER)
    }
    /// Guest CR4.
    pub fn set_cr4(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR4, v);
    }
    /// Guest CR4.
    #[must_use]
    pub const fn cr4(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR4)
    }
    /// Guest CR3.
    pub fn set_cr3(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR3, v);
    }
    /// Guest CR3.
    #[must_use]
    pub const fn cr3(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR3)
    }
    /// Guest CR0.
    pub fn set_cr0(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR0, v);
    }
    /// Guest CR0.
    #[must_use]
    pub const fn cr0(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR0)
    }
    /// Guest DR6.
    #[must_use]
    pub const fn dr6(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::DR6)
    }
    /// Guest DR7.
    #[must_use]
    pub const fn dr7(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::DR7)
    }
    /// Guest RFLAGS.
    pub fn set_rflags(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RFLAGS, v);
    }
    /// Guest RFLAGS.
    #[must_use]
    pub const fn rflags(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RFLAGS)
    }
    /// Guest RIP.
    pub fn set_rip(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RIP, v);
    }
    /// Guest RIP.
    #[must_use]
    pub const fn rip(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RIP)
    }
    /// Guest RSP.
    pub fn set_rsp(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RSP, v);
    }
    /// Guest RSP.
    #[must_use]
    pub const fn rsp(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RSP)
    }
    /// Guest RAX.
    pub fn set_rax(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RAX, v);
    }
    /// A qword of the state save area, by its [`save`] offset: the MSR-backed
    /// fields (`STAR`, `LSTAR`, `KernelGsBase`, `SYSENTER_*`, `G_PAT`, …) an
    /// intercepted `RDMSR`/`WRMSR` reads and writes.
    #[must_use]
    pub const fn save64(&self, off: usize) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + off)
    }
    /// Write a qword of the state save area, by its [`save`] offset.
    pub fn set_save64(&mut self, off: usize, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + off, v);
    }
    /// The `V_INTR` qword (control `060h`): virtual-interrupt state and
    /// [`ctrl::V_INTR_MASKING`].
    pub fn set_v_intr(&mut self, v: u64) {
        wr64(self.page, ctrl::V_INTR, v);
    }
    /// Event injection (`EVENTINJ`, control `0A8h`) for the next `VMRUN`: the
    /// guest takes the event as if it had arisen on its own.
    pub fn set_eventinj(&mut self, v: u64) {
        wr64(self.page, ctrl::EVENTINJ, v);
    }
    /// Guest RAX.
    #[must_use]
    pub const fn rax(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RAX)
    }
}

/// CR0 bits the consistency checks name (APM Vol 2 §15.5.1; encodings Vol 3).
pub mod cr0 {
    /// PE — protection enable.
    pub const PE: u64 = 1 << 0;
    /// NE — numeric error.
    pub const NE: u64 = 1 << 5;
    /// NW — write through.
    pub const NW: u64 = 1 << 29;
    /// CD — cache disable.
    pub const CD: u64 = 1 << 30;
    /// PG — paging enable.
    pub const PG: u64 = 1 << 31;
}

/// CR4 bits the consistency checks name.
pub mod cr4 {
    /// PAE — physical address extension (bit 5). Bit 6 is `MCE`; the two
    /// were transposed once and the consistency ladder caught the L0
    /// rejecting the resulting `LME & PG & !PAE` state.
    pub const PAE: u64 = 1 << 5;
}

/// A VMRUN consistency check that the guest state violates (§15.5.1).
///
/// The hardware reports all of these the same way — `VMEXIT_INVALID`, no
/// diagnostics — so the checks are reimplemented here, where a failing boot
/// names its own cause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Illicit {
    /// Guest `EFER.SVME` must be 1.
    SvmeClear,
    /// `CR0.CD = 0` with `CR0.NW = 1`.
    Cr0CdWithoutNw,
    /// `CR0[63:32] != 0`.
    Cr0HighBits,
    /// `DR6[63:32] != 0`.
    Dr6HighBits,
    /// `DR7[63:32] != 0`.
    Dr7HighBits,
    /// `EFER.LMA` set without `EFER.LME`.
    LmaWithoutLme,
    /// `LME & PG` without `CR4.PAE`.
    LmePgWithoutPae,
    /// `LME & PG` without `CR0.PE`.
    LmePgWithoutPe,
    /// `LME & PG & PAE` with both `CS.L` and `CS.D` set.
    LongModeLAndD,
    /// Guest ASID is 0.
    AsidZero,
    /// The VMRUN intercept bit is not set — §15.5.1 requires the VMCB to
    /// intercept VMRUN itself.
    VmrunNotIntercepted,
}

/// Check a VMCB's guest state against the §15.5.1 list. `Ok` means VMRUN
/// should not fail with `VMEXIT_INVALID` *for the checks implemented here*.
///
/// Deliberately **not** implemented, pending exact bit tables from the spec
/// transcription: the MBZ-bit sets of CR3/CR4 and the EFER reserved bits
/// (Vol 3 owns the encodings; transcribe before the Linux phase), MSRPM/IOPM
/// beyond max physical address (needs the box's max PA), event-injection
/// legality (the seed never injects: EVENTINJ stays 0), and the S_CET checks
/// (CET unused).
pub fn validate_guest_state(vmcb: &Vmcb<'_>) -> Result<(), Illicit> {
    use cr0::{CD, NW, PE, PG};
    use msr::{EFER_LMA, EFER_LME, EFER_SVME};

    let efer = vmcb.efer();
    let cr0v = vmcb.cr0();
    let cr4v = vmcb.cr4();

    if efer & EFER_SVME == 0 {
        return Err(Illicit::SvmeClear);
    }
    if cr0v & CD == 0 && cr0v & NW != 0 {
        return Err(Illicit::Cr0CdWithoutNw);
    }
    if cr0v >> 32 != 0 {
        return Err(Illicit::Cr0HighBits);
    }
    if vmcb.dr6() >> 32 != 0 {
        return Err(Illicit::Dr6HighBits);
    }
    if vmcb.dr7() >> 32 != 0 {
        return Err(Illicit::Dr7HighBits);
    }
    if efer & EFER_LMA != 0 && efer & EFER_LME == 0 {
        return Err(Illicit::LmaWithoutLme);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr4v & cr4::PAE == 0 {
        return Err(Illicit::LmePgWithoutPae);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr0v & PE == 0 {
        return Err(Illicit::LmePgWithoutPe);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr4v & cr4::PAE != 0 {
        let cs = vmcb.seg(save::CS);
        if cs.attr.is_long() && cs.attr.is_db() {
            return Err(Illicit::LongModeLAndD);
        }
    }
    if vmcb.asid() == 0 {
        return Err(Illicit::AsidZero);
    }
    if !vmcb.page().is_empty() && rd32(vmcb.page(), ctrl::INSTR_INTERCEPT1) & intr1::VMRUN == 0 {
        return Err(Illicit::VmrunNotIntercepted);
    }
    Ok(())
}

impl Illicit {
    /// The static name, for a serial line that cannot allocate.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SvmeClear => "SvmeClear",
            Self::Cr0CdWithoutNw => "Cr0CdWithoutNw",
            Self::Cr0HighBits => "Cr0HighBits",
            Self::Dr6HighBits => "Dr6HighBits",
            Self::Dr7HighBits => "Dr7HighBits",
            Self::LmaWithoutLme => "LmaWithoutLme",
            Self::LmePgWithoutPae => "LmePgWithoutPae",
            Self::LmePgWithoutPe => "LmePgWithoutPe",
            Self::LongModeLAndD => "LongModeLAndD",
            Self::AsidZero => "AsidZero",
            Self::VmrunNotIntercepted => "VmrunNotIntercepted",
        }
    }
}

/// Nested page-table entry encodings.
///
/// The NPT walk uses the long-mode page-table format against `N_CR3`
/// (§15.25.3: the extra translation "uses the same paging mode as the VMM";
/// §15.25.5). Transcribed from Vol 2 §5.3, Figures 519–529:
///
/// - 4-KiB PTE leaf: `P`(0) `RW`(1) `US`(2) `PWT`(3) `PCD`(4) `A`(5) `D`(6)
///   `PAT`(7) `G`(8) AVL(11:9) base[51:12] `NX`(63);
/// - 2-MiB PDE leaf: the same, `PS`(7)=1 and PAT(12), base[51:21];
/// - interior entries: `PS`=0, `D` and `G` MBZ; `NX` is honored at every
///   level when the host's own `EFER.NXE` is set (§5.6.3, §15.25.7).
///
/// The NPT-specific rules the builders bake in (§15.25.5, §15.25.7): a
/// guest-page-table walk is a **user write** at this level, so a page the
/// guest's walker must touch needs U|W or it NPFs; A/D are hardware-set in
/// the entries the walks touch, so builders never set them; guest
/// writability and executability are the AND of the two levels, and `G`
/// (like `CR0.WP`) is guest-only — the nested `G` is ignored.
pub mod npt {
    /// Present.
    pub const P: u64 = 1 << 0;
    /// Read/write.
    pub const RW: u64 = 1 << 1;
    /// User.
    pub const US: u64 = 1 << 2;
    /// Page-level write-through.
    pub const PWT: u64 = 1 << 3;
    /// Page-level cache disable.
    pub const PCD: u64 = 1 << 4;
    /// Accessed — hardware-set by the walk, never set by a builder.
    pub const A: u64 = 1 << 5;
    /// Dirty — hardware-set; MBZ in an interior entry.
    pub const D: u64 = 1 << 6;
    /// Page size — a 2 MiB leaf at the PD level; MBZ in an interior entry.
    pub const PS: u64 = 1 << 7;
    /// Global — guest-only under nested paging (§15.25.7); ignored here.
    pub const G: u64 = 1 << 8;
    /// PAT — the memory-type index bit at a 4-KiB leaf (Figure 523).
    pub const PAT: u64 = 1 << 12;
    /// No-execute — honored when the host's `EFER.NXE` is set (§15.25.7).
    pub const NX: u64 = 1 << 63;
    /// Physical address field of an NPT entry: bits 51:12.
    pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    /// An interior (pointer) entry pointing at `child_pa`. Guest accesses walk
    /// the NPT as user-mode accesses (APM Vol 2 §15.25.5), so every entry needs
    /// U=1 or a present page still faults (NPF, P=1).
    #[must_use]
    pub const fn pointer(child_pa: u64) -> u64 {
        (child_pa & ADDR_MASK) | P | RW | US
    }
    /// A 2 MiB leaf mapping `pa` (which must be 2 MiB aligned).
    #[must_use]
    pub const fn leaf_2m(pa: u64) -> u64 {
        (pa & ADDR_MASK) | P | RW | US | PS
    }
    /// A 4 KiB leaf mapping `pa` (which must be 4 KiB aligned).
    #[must_use]
    pub const fn leaf_4k(pa: u64) -> u64 {
        (pa & ADDR_MASK) | P | RW | US
    }
}

/// The `#VMEXIT(NPF)` `EXITINFO1` error code (§15.25.6, the page-573 field
/// list): a #PF-shaped code, plus the walk-order and shadow-stack bits the
/// NPF form adds above bit 31.
pub mod npf {
    /// The nested page was not present.
    pub const P: u64 = 1 << 0;
    /// The nested-level access was a write (guest-table walks always are).
    pub const RW: u64 = 1 << 1;
    /// The nested-level access was a user access (MMU walks always are).
    pub const US: u64 = 1 << 2;
    /// Reserved bits were set in the nested entry.
    pub const RSV: u64 = 1 << 3;
    /// The access was a code read (a guest-table walk never reports this).
    pub const ID: u64 = 1 << 4;
    /// The fault was a shadow-stack access.
    pub const SS: u64 = 1 << 6;
    /// The fault happened on the final guest-physical → system-physical walk.
    pub const FINAL: u64 = 1 << 32;
    /// The fault happened on a walk of the guest's page-table pages.
    pub const GPT_WALK: u64 = 1 << 33;
    /// The leaf named a supervisor shadow-stack page with the check enabled.
    pub const SHADOW_STACK: u64 = 1 << 37;
}

/// The MSR permissions map (§15.11).
///
/// Four 16-Kbit vectors packed into **two contiguous physical pages**
/// (8 KiB; `MSRPM_BASE_PA` is 4-KiB aligned, bits 11:0 ignored, and a map
/// reaching past maximum physical address is `VMEXIT_INVALID`). Two bits
/// per MSR, **lsb read, msb write, 1 = intercept**; a MSR the map does not
/// cover intercepts unconditionally when `intr0::MSR_PROT` is set. The
/// vectors (Table 158):
///
/// | byte offsets     | MSRs covered            |
/// |------------------|-------------------------|
/// | `0000h`–`07FFh`  | `0000_0000h–0000_1FFFh` |
/// | `0800h`–`0FFFh`  | `C000_0000h–C000_1FFFh` |
/// | `1000h`–`17FFh`  | `C001_0000h–C001_1FFFh` |
/// | `1800h`–`1FFFh`  | reserved                |
pub mod msrpm {
    /// The whole map: two contiguous pages.
    pub const SIZE: usize = 8192;
    /// One 16-Kbit vector: 8192 MSRs × 2 bits.
    pub const VECTOR: usize = 2048;

    /// Byte index and bit position of the two bits covering `msr`, or
    /// `None` for an MSR in a reserved vector (auto-intercept).
    #[must_use]
    pub const fn bits_of(msr: u32) -> Option<(usize, u8)> {
        let area = match msr {
            0x0000_0000..=0x0000_1FFF => 0,
            0xC000_0000..=0xC000_1FFF => 1,
            0xC001_0000..=0xC001_1FFF => 2,
            _ => return None,
        };
        let off = (msr & 0x1FFF) as usize;
        Some((area * VECTOR + off / 4, ((off % 4) * 2) as u8))
    }

    /// `page` with the two bits covering `msr` set to intercept reads
    /// and/or writes. Const so a fixed policy map can arm a `static`.
    #[must_use]
    pub const fn with_access(page: [u8; SIZE], msr: u32, read: bool, write: bool) -> [u8; SIZE] {
        let mut page = page;
        if let Some((byte, bit)) = bits_of(msr) {
            page[byte] = if read {
                page[byte] | (1 << bit)
            } else {
                page[byte] & !(1 << bit)
            };
            page[byte] = if write {
                page[byte] | (1 << (bit + 1))
            } else {
                page[byte] & !(1 << (bit + 1))
            };
        }
        page
    }

    /// The two bits covering `msr` as `(read_intercepted,
    /// write_intercepted)`. A reserved-vector MSR reads as `(true, true)` —
    /// the auto-intercept the hardware applies.
    #[must_use]
    pub const fn bits(page: &[u8; SIZE], msr: u32) -> (bool, bool) {
        if let Some((byte, bit)) = bits_of(msr) {
            let v = page[byte] >> bit;
            (v & 1 != 0, v & 2 != 0)
        } else {
            (true, true)
        }
    }
}

/// The I/O permissions map (§15.10.1).
///
/// "A linear array of 64K+3 bits (two 4-Kbyte pages, and the first three
/// bits of a third 4-Kbyte page)" — **12 contiguous KiB**, 4-KiB aligned
/// (`IOPM_BASE_PA` bits 11:0 ignored; past maximum physical address is
/// `VMEXIT_INVALID`). One bit per 8-bit port, **1 = intercept**, bit 0 =
/// port 0.
pub mod iopm {
    /// The whole map: three contiguous pages (§15.10.1: "12 Kbytes").
    pub const SIZE: usize = 12288;
    /// The ports the map names; the "+3 bits" name ports that do not exist.
    pub const PORTS: usize = 65536;

    /// `page` with the bit for `port` set to intercept or allow. Const so a
    /// fixed policy map can arm a `static`.
    #[must_use]
    pub const fn with_port(page: [u8; SIZE], port: u16, on: bool) -> [u8; SIZE] {
        let bit = port as usize;
        let mask = 1u8 << (bit % 8);
        let mut page = page;
        page[bit / 8] = if on { page[bit / 8] | mask } else { page[bit / 8] & !mask };
        page
    }

    /// The map that intercepts **every** port.
    ///
    /// `0xFF` repeated — bit 1 = intercept — for each of the 64K+3 bits. A
    /// guest whose port traffic the VMM must see (a real OS probes hundreds
    /// of ports) arms this rather than naming ports one `with_port` at a
    /// time.
    #[must_use]
    pub const fn all() -> [u8; SIZE] {
        [0xFF; SIZE]
    }

    /// Is access to `port` intercepted?
    #[must_use]
    pub const fn intercepted(page: &[u8; SIZE], port: u16) -> bool {
        let bit = port as usize;
        page[bit / 8] & (1 << (bit % 8)) != 0
    }
}

/// Page tables as **indices and walks**.
///
/// The one computation a hand-built table gets silently wrong is which slot
/// a VA names at each level — phase 4 wrote its PD→PT entry at slot 0 when
/// `0x5000_0000` names slot 128, and the rehearsal spent a guest `#PF`
/// discovering it. [`idx`] derives every level's index from the VA, and
/// [`walk`]/[`translate`] read a built table back the way the MMU would, so
/// a builder can check its own work before any guest runs.
///
/// Entries use the same encoding as [`npt`] — which is the x86-64 PTE, and
/// so also the guest's own tables (the long-mode phase already builds guest
/// entries with `npt`'s builders).
pub mod tables {
    /// Shift of the PML4 index field.
    pub const PML4_SHIFT: u32 = 39;
    /// Shift of the PDPT index field.
    pub const PDPT_SHIFT: u32 = 30;
    /// Shift of the PD index field.
    pub const PD_SHIFT: u32 = 21;
    /// Shift of the PT index field.
    pub const PT_SHIFT: u32 = 12;

    /// The 9-bit index `va` names at the level whose field starts at
    /// `shift`. The whole lesson of phase 4 in one line: the index is a
    /// property of the VA, derived here rather than computed by hand at a
    /// call site.
    #[must_use]
    pub const fn idx(va: u64, shift: u32) -> usize {
        ((va >> shift) & 0x1FF) as usize
    }

    /// Read access to 4-KiB table pages by physical address.
    ///
    /// The kernel reads through the physmap; a host test backs the pages in
    /// a table of its own. Nothing here can write — a walk is evidence, not
    /// a mutation.
    pub trait PageSource {
        /// The little-endian u64 at `off` in the table page at `table_pa`.
        fn read(&self, table_pa: u64, off: usize) -> u64;
    }

    /// Why a walk stopped.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum WalkError {
        /// A table page the walk reached could not be read (unmapped, or a
        /// zero pointer entry).
        Missing,
        /// The entry at `shift` lacked P — the walk is over and the VA is
        /// unmapped at that level.
        Absent(u32),
    }

    /// A resolved leaf: its entry and the shift of the level it sits at
    /// (12 = 4 KiB, 21 = 2 MiB, 30 = 1 GiB — the PS-bit levels).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Leaf {
        pub entry: u64,
        pub shift: u32,
    }

    impl Leaf {
        /// The physical address `va` translates to through this leaf: the
        /// entry's address field plus the VA's offset inside the page.
        #[must_use]
        pub fn pa(self, va: u64) -> u64 {
            (self.entry & super::npt::ADDR_MASK) | (va & ((1 << self.shift) - 1))
        }
    }

    /// Walk a 4-level table the way the MMU does: index by `va` at each
    /// level, follow interior pointers, stop at the first leaf (P with PS)
    /// or at the PT.
    pub fn walk<S: PageSource>(src: &S, root_pa: u64, va: u64) -> Result<Leaf, WalkError> {
        let mut table = root_pa;
        for shift in [PML4_SHIFT, PDPT_SHIFT, PD_SHIFT] {
            let entry = src.read(table, idx(va, shift) * 8);
            if entry & super::npt::P == 0 {
                return Err(WalkError::Absent(shift));
            }
            if entry & super::npt::PS != 0 {
                return Ok(Leaf { entry, shift });
            }
            table = entry & super::npt::ADDR_MASK;
            if table == 0 {
                return Err(WalkError::Missing);
            }
        }
        let entry = src.read(table, idx(va, PT_SHIFT) * 8);
        if entry & super::npt::P == 0 {
            return Err(WalkError::Absent(PT_SHIFT));
        }
        Ok(Leaf { entry, shift: PT_SHIFT })
    }

    /// The physical address `va` resolves to, or why it does not.
    pub fn translate<S: PageSource>(src: &S, root_pa: u64, va: u64) -> Result<u64, WalkError> {
        walk(src, root_pa, va).map(|leaf| leaf.pa(va))
    }
}

/// The ELF64 image, as far as a loader needs to see it: the `PT_LOAD`
/// segments and the note that names a PVH 32-bit entry point.
///
/// Field offsets are the ELF64 layout from the System V generic ABI (the
/// `elf.h` everyone shares); the tests build an image byte-by-byte and read
/// it back, so a transposed offset is a test failure here rather than a
/// guest that never starts. vmlinux ELF images built with `CONFIG_PVH`
/// carry the entry note; a bzImage does not (its own header is a different
/// format, deliberately not decoded yet).
/// Decoded `EXITINFO1` for the exits whose layout is a bitfield.
pub mod exitinfo {
    /// An intercepted `IN`/`OUT`/`INS`/`OUTS`: `EXITINFO1` as APM Vol 2 §15.10.2,
    /// Figure 15-2 (rev 3.45, p. 538). `EXITINFO2` is the RIP of the
    /// instruction *after* it, so no length decode is needed to resume.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Io {
        /// Bits 31:16 — the port.
        pub port: u16,
        /// Bit 0 — `TYPE`: 1 = `IN`, 0 = `OUT`.
        pub is_in: bool,
        /// Bit 2 — `STR`: `INS`/`OUTS`.
        pub string: bool,
        /// Bit 3 — `REP`.
        pub rep: bool,
        /// Bits 6:4 — `SZ32`/`SZ16`/`SZ8`, as a byte count. 0 if none is set,
        /// which the architecture does not produce.
        pub size: u8,
        /// Bits 9:7 — `A64`/`A32`/`A16`, as a width in bits. 0 if none is set.
        pub addr_bits: u8,
        /// Bits 12:10 — the effective segment (`INS` is always ES = 0).
        pub seg: u8,
    }

    impl Io {
        /// Decode `EXITINFO1`.
        #[must_use]
        pub const fn decode(info1: u64) -> Self {
            let size = if info1 & (1 << 4) != 0 {
                1
            } else if info1 & (1 << 5) != 0 {
                2
            } else if info1 & (1 << 6) != 0 {
                4
            } else {
                0
            };
            let addr_bits = if info1 & (1 << 9) != 0 {
                64
            } else if info1 & (1 << 8) != 0 {
                32
            } else if info1 & (1 << 7) != 0 {
                16
            } else {
                0
            };
            Self {
                port: (info1 >> 16) as u16,
                is_in: info1 & 1 != 0,
                string: info1 & (1 << 2) != 0,
                rep: info1 & (1 << 3) != 0,
                size,
                addr_bits,
                seg: ((info1 >> 10) & 0x7) as u8,
            }
        }

        /// The mask of the operand's bytes in RAX (`0xFF`, `0xFFFF`,
        /// `0xFFFF_FFFF`). A write to AL/AX leaves the rest of RAX alone; a
        /// write to EAX is the caller's to zero-extend.
        #[must_use]
        pub const fn mask(&self) -> u64 {
            match self.size {
                1 => 0xFF,
                2 => 0xFFFF,
                _ => 0xFFFF_FFFF,
            }
        }
    }

    /// `VMEXIT_MSR`'s `EXITINFO1` (§15.11, rev 3.45, p. 540): 0 = `RDMSR`,
    /// 1 = `WRMSR`.
    #[must_use]
    pub const fn msr_is_write(info1: u64) -> bool {
        info1 & 1 != 0
    }
}

/// What CPUID tells a guest.
///
/// The host's answer, with every capability the hypervisor does not provide
/// taken out — a feature the guest sees is a feature it will use. Bit
/// positions are APM Vol 3 rev 3.38 Appendix E (Fn0000_0001 ECX/EDX,
/// Fn8000_0001 ECX), pinned by tests.
pub mod cpuid {
    /// `Fn0000_0001` ECX bits removed.
    ///
    /// Hypervisor-present (31), F16C (29), AVX (28), OSXSAVE (27), XSAVE (26),
    /// x2APIC (21), FMA (12), MONITOR (3). XSAVE and everything riding on it
    /// go together: `XSETBV` and `XCR0` are
    /// the host's, `VMRUN` does not swap them, and a guest that wrote its own
    /// would be changing the host's.
    pub const LEAF1_ECX_CLEAR: u32 =
        (1 << 31) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 26) | (1 << 21) | (1 << 12) | (1 << 3);
    /// `Fn0000_0001` EDX bits removed: APIC (9) — there is no local APIC model
    /// yet — plus MCE (7), MCA (14), MTRR (12) and HTT (28): machine-check and
    /// MTRR MSRs are the host's, and the guest is one thread.
    pub const LEAF1_EDX_CLEAR: u32 = (1 << 28) | (1 << 14) | (1 << 12) | (1 << 9) | (1 << 7);
    /// `Fn8000_0001` ECX bits removed.
    ///
    /// SVM (2) — no nested virtualization — ExtApicSpace (3), OSVW (9), IBS (10), WDT (13), LWP (15), TopologyExt
    /// (22), the performance-counter extensions (23, 24, 27, 28), DataBkptExt
    /// (26), MONITORX (29) and AddrMaskExt (30).
    pub const EXT1_ECX_CLEAR: u32 = (1 << 30)
        | (1 << 29)
        | (1 << 28)
        | (1 << 27)
        | (1 << 26)
        | (1 << 24)
        | (1 << 23)
        | (1 << 22)
        | (1 << 15)
        | (1 << 13)
        | (1 << 10)
        | (1 << 9)
        | (1 << 3)
        | (1 << 2);

    /// The guest's CPUID for `leaf`, given the host's `[eax, ebx, ecx, edx]`.
    ///
    /// Leaves that describe features the hypervisor does not provide — 4
    /// (cache topology), 5 (MONITOR/MWAIT), 6 (thermal), 7 (structured
    /// extended features: AVX2, SMEP, …), 0xA, 0xB, 0xD (XSAVE state), 0x1F,
    /// the hypervisor range 0x4000_0000.., and the extended leaves
    /// 0x8000_000A (SVM) and 0x8000_001D.. (topology, SEV, …) — read as zero.
    #[must_use]
    pub const fn filter(leaf: u32, host: [u32; 4]) -> [u32; 4] {
        let [eax, ebx, ecx, edx] = host;
        match leaf {
            0x0000_0001 => [eax, ebx, ecx & !LEAF1_ECX_CLEAR, edx & !LEAF1_EDX_CLEAR],
            0x0000_0004..=0x0000_0007 | 0x0000_000A..=0x0000_001F => [0; 4],
            0x4000_0000..=0x4FFF_FFFF => [0; 4],
            0x8000_0001 => [eax, ebx, ecx & !EXT1_ECX_CLEAR, edx],
            0x8000_000A | 0x8000_001D..=0x8000_FFFF => [0; 4],
            _ => [eax, ebx, ecx, edx],
        }
    }
}

/// Device models a guest talks to over port I/O and MSRs. Pure state
/// machines — no I/O of their own — so the host tests drive them the way an
/// exit handler would.
pub mod devices {
    /// A 16550 UART's register file (COM1 = ports `0x3F8..=0x3FF`).
    ///
    /// As much of it as a Linux guest's early console and 8250 probe exercise: the
    /// transmitter is always ready, nothing is ever received, and writes to the
    /// transmit register come back to the caller as the character sent.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Uart {
        ier: u8,
        lcr: u8,
        mcr: u8,
        scr: u8,
        dll: u8,
        dlm: u8,
        /// The transmitter-holding-register-empty interrupt is pending.
        thre: bool,
    }

    /// `LSR` read as always: transmit-holding-register empty (bit 5) and
    /// transmitter empty (bit 6); no data ready, no errors.
    pub const LSR_READY: u8 = 0x60;
    /// `IIR`: bit 0 set = no interrupt pending (no FIFO enabled).
    pub const IIR_NONE: u8 = 0x01;

    impl Uart {
        /// A UART in its reset state.
        #[must_use]
        pub const fn new() -> Self {
            Self { ier: 0, lcr: 0, mcr: 0, scr: 0, dll: 0, dlm: 0, thre: false }
        }

        const fn dlab(&self) -> bool {
            self.lcr & 0x80 != 0
        }

        /// Read register `reg` (the port minus the base, `0..8`).
        #[must_use]
        pub fn read(&self, reg: u8) -> u8 {
            match reg & 7 {
                0 => if self.dlab() { self.dll } else { 0 },
                1 => if self.dlab() { self.dlm } else { self.ier },
                2 => self.iir(),
                3 => self.lcr,
                4 => self.mcr,
                5 => LSR_READY,
                // Modem status. In loopback (MCR bit 4) the outputs feed the
                // inputs — DTR to DSR (bit 5), RTS to CTS (bit 4), OUT1 to RI
                // (bit 6), OUT2 to DCD (bit 7) — which is how the 8250 driver
                // proves a UART is there. Out of loopback: carrier, DSR, CTS up.
                6 => {
                    if self.mcr & 0x10 != 0 {
                        ((self.mcr & 0x01) << 5)
                            | ((self.mcr & 0x02) << 3)
                            | ((self.mcr & 0x04) << 4)
                            | ((self.mcr & 0x08) << 4)
                    } else {
                        0xB0
                    }
                }
                _ => self.scr,
            }
        }

        /// `IIR` as the guest would read it: `0x02` (THRE) while that interrupt is
        /// pending and enabled, else [`IIR_NONE`].
        const fn iir(&self) -> u8 {
            if self.thre && self.ier & 0x02 != 0 { 0x02 } else { IIR_NONE }
        }

        /// Read register `reg` with a real read's side effect: reading `IIR`
        /// while THRE is the source clears it (16550 datasheet).
        pub fn read_clearing(&mut self, reg: u8) -> u8 {
            let v = self.read(reg);
            if reg & 7 == 2 && v == 0x02 {
                self.thre = false;
            }
            v
        }

        /// Whether the interrupt line is high: THRE pending and enabled, gated by
        /// `OUT2` in `MCR` as on a PC.
        #[must_use]
        pub const fn irq_level(&self) -> bool {
            self.thre && self.ier & 0x02 != 0 && self.mcr & 0x08 != 0
        }

        /// Write register `reg`. `Some(byte)` when this write transmitted a
        /// character (the transmit register, divisor latch closed).
        pub fn write(&mut self, reg: u8, v: u8) -> Option<u8> {
            match reg & 7 {
                0 => {
                    if self.dlab() {
                        self.dll = v;
                    } else {
                        // The holding register empties at once: THRE re-arms.
                        self.thre = true;
                        // In loopback the character never leaves the chip.
                        return if self.mcr & 0x10 != 0 { None } else { Some(v) };
                    }
                }
                1 => {
                    if self.dlab() {
                        self.dlm = v;
                    } else {
                        let was = self.ier;
                        self.ier = v & 0x0F;
                        // Enabling THRE while the holding register is empty
                        // raises it at once.
                        if self.ier & 0x02 != 0 && was & 0x02 == 0 {
                            self.thre = true;
                        }
                    }
                }
                3 => self.lcr = v,
                4 => self.mcr = v & 0x1F,
                7 => self.scr = v,
                _ => {} // FCR and the read-only registers: accepted, ignored.
            }
            None
        }
    }

    /// A small table of MSR values the hypervisor merely remembers: what a guest
    /// wrote reads back. Fixed capacity, no allocation; a full table refuses
    /// the write (the caller logs it) rather than grow.
    #[derive(Clone, Copy, Debug)]
    pub struct MsrStore<const N: usize> {
        slots: [(u32, u64); N],
        used: usize,
    }

    impl<const N: usize> Default for MsrStore<N> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<const N: usize> MsrStore<N> {
        /// An empty store.
        #[must_use]
        pub const fn new() -> Self {
            Self { slots: [(0, 0); N], used: 0 }
        }

        /// The remembered value of `msr`.
        #[must_use]
        pub fn get(&self, msr: u32) -> Option<u64> {
            self.slots[..self.used].iter().find(|s| s.0 == msr).map(|s| s.1)
        }

        /// Remember `v` for `msr`. False if the table is full.
        pub fn set(&mut self, msr: u32, v: u64) -> bool {
            if let Some(s) = self.slots[..self.used].iter_mut().find(|s| s.0 == msr) {
                s.1 = v;
                return true;
            }
            if self.used == N {
                return false;
            }
            self.slots[self.used] = (msr, v);
            self.used += 1;
            true
        }
    }
}

pub mod elf {
    /// `\x7fELF`.
    pub const MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
    /// `EI_CLASS` = 64-bit.
    pub const CLASS64: u8 = 2;
    /// `EI_DATA` = little-endian.
    pub const DATA_LE: u8 = 1;
    /// `p_type` = loadable segment.
    pub const PT_LOAD: u32 = 1;
    /// `p_type` = note segment.
    pub const PT_NOTE: u32 = 4;

    const E_PHOFF: usize = 0x20;
    const E_PHENTSIZE: usize = 0x36;
    const E_PHNUM: usize = 0x38;
    const EHDR_SIZE: usize = 64;
    const P_TYPE: usize = 0x00;
    const P_OFFSET: usize = 0x08;
    const P_PADDR: usize = 0x18;
    const P_FILESZ: usize = 0x20;
    const P_MEMSZ: usize = 0x28;
    const PHDR_SIZE: usize = 56;

    /// The note naming the PVH 32-bit entry: name "Xen", type 18.
    ///
    /// The type is `XEN_ELFNOTE_PHYS32_ENTRY`, whose descriptor is the entry's
    /// guest-physical address. Pinned against `amd64/src/boot.s` (which
    /// emits this very note and is what QEMU's and Firecracker's PVH loaders
    /// boot) and against the Firecracker CI `vmlinux-6.1.128`'s own note
    /// (`readobj --notes`: owner Xen, type 0x12). It was 9 until 2026-10-22 —
    /// `XEN_ELFNOTE_LOADER`'s number — and no synthetic-image test could
    /// see it, because they all used this constant.
    pub const PVH_NOTE_NAME: &[u8] = b"Xen";
    pub const PVH_NOTE_TYPE: u32 = 18;

    /// One `PT_LOAD` segment.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Segment {
        /// Where the segment's bytes sit in the image.
        pub offset: u64,
        /// Guest-physical address the bytes load at.
        pub pa: u64,
        /// Bytes present in the image.
        pub filesz: u64,
        /// Bytes present in memory (`memsz - filesz` zeroes).
        pub memsz: u64,
    }

    /// A parsed ELF64 header over borrowed bytes.
    #[derive(Clone, Copy)]
    pub struct Image<'a> {
        bytes: &'a [u8],
        phoff: u64,
        phentsize: u64,
        phnum: u64,
    }

    impl<'a> Image<'a> {
        /// Parse the header. Rejects short, wrong-magic, wrong-class and
        /// wrong-endianness images, and a program-header table whose extent
        /// leaves the image — anything else is a loader bug waiting.
        #[must_use]
        pub fn parse(bytes: &'a [u8]) -> Option<Self> {
            if bytes.len() < EHDR_SIZE || bytes[..4] != MAGIC {
                return None;
            }
            if bytes[4] != CLASS64 || bytes[5] != DATA_LE {
                return None;
            }
            let phoff = u64::from_le_bytes(bytes[E_PHOFF..E_PHOFF + 8].try_into().ok()?);
            let phentsize =
                u64::from(u16::from_le_bytes(bytes[E_PHENTSIZE..E_PHENTSIZE + 2].try_into().ok()?));
            let phnum = u64::from(u16::from_le_bytes(bytes[E_PHNUM..E_PHNUM + 2].try_into().ok()?));
            if phentsize < PHDR_SIZE as u64 {
                return None;
            }
            let end = phoff.checked_add(phentsize.checked_mul(phnum)?)?;
            if end as usize > bytes.len() {
                return None;
            }
            Some(Self { bytes, phoff, phentsize, phnum })
        }

        /// The `PT_LOAD` segments, in header order.
        pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
            (0..self.phnum).filter_map(|i| {
                self.phdr(i).and_then(|(ptype, seg)| (ptype == PT_LOAD).then_some(seg))
            })
        }

        /// The PVH 32-bit entry point, from the note segment: name `Xen`,
        /// type 18, descriptor = the entry address. `None` for an image
        /// without the note — including every bzImage. Needs the note segment's
        /// bytes in `self.bytes`; a loader that holds only the file's head
        /// reads [`Image::note_segments`] and calls [`find_pvh_entry`] itself.
        #[must_use]
        pub fn entry_phys32(&self) -> Option<u32> {
            for seg in self.note_segments() {
                let start = usize::try_from(seg.offset).ok()?;
                let end = start.checked_add(usize::try_from(seg.filesz).ok()?)?;
                if let Some(entry) = self.bytes.get(start..end).and_then(find_pvh_entry) {
                    return Some(entry);
                }
            }
            None
        }

        /// The `PT_NOTE` segments, in header order (`pa`/`memsz` are as in the
        /// header; `offset`/`filesz` say where the notes sit in the image).
        pub fn note_segments(&self) -> impl Iterator<Item = Segment> + '_ {
            (0..self.phnum).filter_map(|i| {
                self.phdr(i).and_then(|(ptype, seg)| (ptype == PT_NOTE).then_some(seg))
            })
        }

        fn phdr(&self, i: u64) -> Option<(u32, Segment)> {
            let base = self.phoff as usize + i as usize * self.phentsize as usize;
            let b = self.bytes.get(base..base + PHDR_SIZE)?;
            let rd32 =
                |o: usize| -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?)) };
            let rd64 =
                |o: usize| -> Option<u64> { Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?)) };
            Some((
                rd32(P_TYPE)?,
                Segment {
                    offset: rd64(P_OFFSET)?,
                    pa: rd64(P_PADDR)?,
                    filesz: rd64(P_FILESZ)?,
                    memsz: rd64(P_MEMSZ)?,
                },
            ))
        }
    }

    /// The PVH entry from an ELF note stream (the bytes of one `PT_NOTE`
    /// segment): name `Xen`, type 18, descriptor = the 32-bit entry address.
    ///
    /// Bounds-safe over hostile input: every read is checked and a malformed
    /// stream ends the walk with `None`.
    #[must_use]
    pub fn find_pvh_entry(notes: &[u8]) -> Option<u32> {
        let rd32 = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(notes.get(at..at.checked_add(4)?)?.try_into().ok()?))
        };
        let mut pos = 0usize;
        while pos.checked_add(12)? <= notes.len() {
            let namesz = rd32(pos)? as usize;
            let descsz = rd32(pos + 4)? as usize;
            let ty = rd32(pos + 8)?;
            let name_at = pos + 12;
            // ELF pads the name and the descriptor each to a 4-byte boundary;
            // `namesz`/`descsz` are the unpadded sizes. A real kernel carries
            // notes before the Xen one with odd-length names ("Linux\0" is 6),
            // so stepping by the raw sizes loses the stream and misses the
            // entry.
            let desc_at = name_at.checked_add(namesz.checked_add(3)? & !3)?;
            let next = desc_at.checked_add(descsz.checked_add(3)? & !3)?;
            if desc_at.checked_add(descsz)? > notes.len() {
                return None;
            }
            // The stored name is "Xen\0" (namesz counts the NUL): compare the
            // prefix, bounded by namesz (a 3-byte namesz names "Xen" bare).
            let name = notes.get(name_at..name_at.checked_add(namesz.min(PVH_NOTE_NAME.len() + 1))?)?;
            let name_ok = name.starts_with(PVH_NOTE_NAME);
            if ty == PVH_NOTE_TYPE && name_ok && descsz >= 4 {
                return rd32(desc_at);
            }
            pos = next;
        }
        None
    }
}

/// The PVH handoff: `hvm_start_info` and its memory-map table, built into
/// guest RAM by [`plan`] + the three writers.
///
/// This is the Xen project's public PVH entry ABI (the `hvm_start_info` /
/// `hvm_memmap_table_entry` wire format): magic `336EC578h`, version 0, a
/// flags qword, the guest-physical addresses of the command line, the RSDP
/// (0 = none, what a no-ACPI phase hands over) and the memory map, then the
/// entry count. The guest is *entered* in 32-bit protected mode, paging
/// off, with EBX holding `start_info`'s guest-physical address — the entry
/// EIP comes from the image's ELF note (`elf::entry_phys32`). No Linux code
/// is consulted anywhere in this crate; the format is the platform's boot
/// ABI, the same bytes every PVH loader writes.
pub mod pvh {
    /// `hvm_start_info.magic`.
    pub const START_MAGIC: u32 = 0x336E_C578;
    /// `hvm_start_info.version`: 1, the version that carries the memory map
    /// (`memmap_paddr`/`memmap_entries`); version 0 stops after `rsdp_paddr`
    /// and a guest then reads no memory map at all.
    pub const START_VERSION: u32 = 1;
    /// `hvm_memmap_table_entry.type` = usable RAM.
    pub const MEMMAP_RAM: u32 = 1;
    /// `sizeof(hvm_start_info)` at version 1 (Xen's public `start_info.h`).
    ///
    /// Fields: u32 magic, u32 version, u32 flags, u32 nr_modules, u64 modlist_paddr,
    /// u64 cmdline_paddr, u64 rsdp_paddr, u64 memmap_paddr, u32 memmap_entries,
    /// u32 reserved.
    ///
    /// The offsets are pinned by a test that reads the blobs back with the
    /// kernel's own parser (`akuma-ryzen-amd64`'s `StartInfo`, the one that
    /// boots on QEMU and Firecracker). Until 2026-10-22 this module had
    /// `cmdline` at `+0x10`, `memmap` at `+0x20` and a 48-byte struct, and its
    /// synthetic guest read the same wrong offsets — so the PVH phase passed
    /// while a real guest saw no command line and no memory map.
    pub const START_SIZE: usize = 56;
    /// `sizeof(hvm_memmap_table_entry)`: u64 addr, u64 size, u32 type, u32
    /// reserved.
    pub const MEMMAP_ENTRY_SIZE: usize = 24;

    const F_MAGIC: usize = 0x00;
    const F_VERSION: usize = 0x04;
    const F_FLAGS: usize = 0x08;
    const F_NR_MODULES: usize = 0x0C;
    const F_MODLIST: usize = 0x10;
    const F_CMDLINE: usize = 0x18;
    const F_RSDP: usize = 0x20;
    const F_MEMMAP: usize = 0x28;
    const F_ENTRIES: usize = 0x30;
    const F_RESERVED: usize = 0x34;
    const E_ADDR: usize = 0x00;
    const E_SIZE: usize = 0x08;
    const E_TYPE: usize = 0x10;
    const E_RESERVED: usize = 0x14;

    /// Where the handoff blobs live in guest RAM: one 4-KiB page each,
    /// packed directly under `top_gpa` (the top of guest RAM), above
    /// anything a loader places.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Layout {
        pub cmdline_gpa: u64,
        pub memmap_gpa: u64,
        pub start_gpa: u64,
        /// The RAM extent the memory map reports.
        pub ram_base: u64,
        pub ram_len: u64,
    }

    /// The layout: three pages under `top_gpa`, one RAM entry covering
    /// `[ram_base, ram_base + ram_len)`.
    #[must_use]
    pub const fn plan(top_gpa: u64, ram_base: u64, ram_len: u64) -> Layout {
        Layout {
            cmdline_gpa: top_gpa - 0x3000,
            memmap_gpa: top_gpa - 0x2000,
            start_gpa: top_gpa - 0x1000,
            ram_base,
            ram_len,
        }
    }

    /// Write `hvm_start_info` into the `start_gpa` page.
    pub fn write_start(buf: &mut [u8], l: &Layout) {
        wr32(buf, F_MAGIC, START_MAGIC);
        wr32(buf, F_VERSION, START_VERSION);
        wr32(buf, F_FLAGS, 0);
        wr32(buf, F_NR_MODULES, 0);
        wr64(buf, F_MODLIST, 0);
        wr64(buf, F_CMDLINE, l.cmdline_gpa);
        wr64(buf, F_RSDP, 0);
        wr64(buf, F_MEMMAP, l.memmap_gpa);
        wr32(buf, F_ENTRIES, 1);
        wr32(buf, F_RESERVED, 0);
    }

    /// Write the one-entry memory map into the `memmap_gpa` page.
    pub fn write_memmap(buf: &mut [u8], l: &Layout) {
        wr64(buf, E_ADDR, l.ram_base);
        wr64(buf, E_SIZE, l.ram_len);
        wr32(buf, E_TYPE, MEMMAP_RAM);
        wr32(buf, E_RESERVED, 0);
    }

    /// `hvm_memmap_table_entry.type` = ACPI tables (reclaimable).
    pub const MEMMAP_ACPI: u32 = 3;

    /// Add an ACPI-tables entry `[gpa, gpa + len)` as entry 1 of the memory map.
    ///
    /// Also points `start_buf`'s `rsdp_paddr` at `rsdp_gpa`; the start_info then
    /// reports two entries. Entry 0 ([`write_memmap`]'s RAM) must not overlap it.
    pub fn add_acpi(start_buf: &mut [u8], memmap_buf: &mut [u8], rsdp_gpa: u64, gpa: u64, len: u64) {
        wr64(memmap_buf, MEMMAP_ENTRY_SIZE + E_ADDR, gpa);
        wr64(memmap_buf, MEMMAP_ENTRY_SIZE + E_SIZE, len);
        wr32(memmap_buf, MEMMAP_ENTRY_SIZE + E_TYPE, MEMMAP_ACPI);
        wr32(memmap_buf, MEMMAP_ENTRY_SIZE + E_RESERVED, 0);
        wr64(start_buf, F_RSDP, rsdp_gpa);
        wr32(start_buf, F_ENTRIES, 2);
    }

    /// Write the NUL-terminated command line into the `cmdline_gpa` page;
    /// false if it does not fit a page (it never will — 4 KiB).
    pub fn write_cmdline(buf: &mut [u8], cmdline: &str) -> bool {
        if cmdline.len() + 1 > buf.len() {
            return false;
        }
        buf[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
        buf[cmdline.len()] = 0;
        true
    }

    fn wr32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn wr64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    // The crate is `no_std`; the harness links std, but the path needs naming
    // for the one test that reads a real kernel off disk.
    extern crate std;
    use super::*;

    /// A VMCB page with one recognizable byte pattern everywhere, so an
    /// accessor that reads the wrong offset fails loudly rather than
    /// reading another field's zero.
    fn pattern_page() -> [u8; VMCB_SIZE] {
        let mut page = [0u8; VMCB_SIZE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        page
    }

    // ---- absolute offset pins: every Table B-1/B-2 fact the seed relies on ----

    #[test]
    fn control_offsets_are_table_b1() {
        let page = pattern_page();
        assert_eq!(ctrl::EXITCODE, 0x070);
        assert_eq!(rd64(&page, ctrl::EXITCODE), u64::from_le_bytes(page[0x070..0x078].try_into().unwrap()));
        assert_eq!(ctrl::EXITINFO1, 0x078);
        assert_eq!(ctrl::EXITINFO2, 0x080);
        assert_eq!(ctrl::EXITINTINFO, 0x088);
        assert_eq!(ctrl::N_CR3, 0x0B0);
        assert_eq!(ctrl::NRIP, 0x0C8);
        assert_eq!(ctrl::GUEST_INSTR, 0x0D0);
        assert_eq!(ctrl::GUEST_ASID, 0x058);
        assert_eq!(ctrl::NP_ENABLE, 0x090);
        assert_eq!(ctrl::IOPM_BASE, 0x040);
        assert_eq!(ctrl::MSRPM_BASE, 0x048);
    }

    #[test]
    fn save_offsets_are_table_b2() {
        // Save-area-relative offsets from the table; absolute in the page.
        for (rel, abs) in [
            (save::ES, 0x400),
            (save::CS, 0x410),
            (save::SS, 0x420),
            (save::DS, 0x430),
            (save::FS, 0x440),
            (save::GS, 0x450),
            (save::GDTR, 0x460),
            (save::IDTR, 0x480),
            (save::TR, 0x490),
            (save::CPL, 0x4CB),
            (save::EFER, 0x4D0),
            (save::CR4, 0x548),
            (save::CR3, 0x550),
            (save::CR0, 0x558),
            (save::DR7, 0x560),
            (save::DR6, 0x568),
            (save::RFLAGS, 0x570),
            (save::RIP, 0x578),
            (save::RSP, 0x5D8),
            (save::RAX, 0x5F8),
            (save::CR2, 0x640),
            (save::G_PAT, 0x668),
        ] {
            assert_eq!(SAVE_AREA_BASE + rel, abs, "offset 0x{rel:03X} must land at 0x{abs:03X}");
        }
    }

    /// Numeric pins for the CR0/CR4/EFER bit vocabulary. The validator
    /// reasons *through* these constants, so a symbolic test can never
    /// catch a transposed bit — `cr4::PAE` rode at `1 << 6` (`MCE`) while
    /// every test stayed green, and only the L0's `VMEXIT_INVALID` named
    /// it. Pin the encodings against Vol 3 so the same slip fails `cargo
    /// test` instead of a boot.
    #[test]
    fn bit_constants_are_vol3_encodings() {
        // CR0 (Table 3-14-ish; the encodings are ancient and stable).
        assert_eq!(cr0::PE, 1 << 0);
        assert_eq!(cr0::NE, 1 << 5);
        assert_eq!(cr0::NW, 1 << 29);
        assert_eq!(cr0::CD, 1 << 30);
        assert_eq!(cr0::PG, 1 << 31);
        // CR4: PAE is bit 5; bit 6 is MCE — the transposition this pins.
        assert_eq!(cr4::PAE, 0x20);
        // EFER (MSR 0xC000_0080) bits, and the MSR addresses themselves.
        assert_eq!(msr::EFER_LME, 1 << 8);
        assert_eq!(msr::EFER_LMA, 1 << 10);
        assert_eq!(msr::EFER_SVME, 1 << 12);
        assert_eq!(msr::EFER, 0xC000_0080);
        assert_eq!(msr::VM_CR, 0xC001_0114);
        assert_eq!(msr::VM_CR_SVMDIS, 1 << 4);
        assert_eq!(msr::VM_HSAVE_PA, 0xC001_0117);
    }

    #[test]
    fn typed_accessors_read_the_pinned_offsets() {
        // All raw writes happen before the VMCB is typed: one borrow for the
        // setup, one for the reads.
        let mut page = [0u8; VMCB_SIZE];
        wr64(&mut page, 0x4D0, 0x1234);
        wr64(&mut page, 0x578, 0xCAFE);
        page[0x4CB] = 3;
        wr32(&mut page, 0x058, 7);
        wr32(&mut page, 0x090, 1);
        wr64(&mut page, 0x0B0, 0xABCD_000);
        wr64(&mut page, 0x0C8, 0x5555);
        wr64(&mut page, 0x070, 0x81);
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(vmcb.efer(), 0x1234);
        assert_eq!(vmcb.rip(), 0xCAFE);
        assert_eq!(vmcb.cpl(), 3);
        assert_eq!(vmcb.asid(), 7);
        assert!(vmcb.np_enable());
        assert_eq!(vmcb.n_cr3(), 0xABCD_000);
        assert_eq!(vmcb.nrip(), 0x5555);
        assert_eq!(vmcb.exitcode(), 0x81);
        assert_eq!(rd64(vmcb.page(), ctrl::EXITCODE), vmcb.exitcode());
    }

    #[test]
    fn setters_write_the_pinned_offsets() {
        let mut page = [0u8; VMCB_SIZE];
        {
            let mut vmcb = Vmcb::new(&mut page);
            vmcb.set_efer(0x1111);
            vmcb.set_rip(0x2222);
            vmcb.set_rax(0x3333);
            vmcb.set_rsp(0x4444);
            vmcb.set_cr0(0x5555);
            vmcb.set_cr3(0x6666);
            vmcb.set_cr4(0x7777);
            vmcb.set_rflags(2);
            vmcb.set_cpl(0);
            vmcb.set_asid(1);
            vmcb.set_tlb_control(ctrl::TLB_DO_NOTHING);
            vmcb.set_n_cr3(0x88_000);
            vmcb.set_np_enable(true);
            vmcb.set_intr0(intr0::CPUID | intr0::HLT | intr0::IOIO_PROT | intr0::MSR_PROT | intr0::INTR);
            vmcb.set_intr1(intr1::VMRUN | intr1::VMMCALL);
            vmcb.set_excp(excp::DB | excp::UD | excp::GP | excp::PF);
        }
        assert_eq!(rd64(&page, 0x4D0), 0x1111);
        assert_eq!(rd64(&page, 0x578), 0x2222);
        assert_eq!(rd64(&page, 0x5F8), 0x3333);
        assert_eq!(rd64(&page, 0x5D8), 0x4444);
        assert_eq!(rd64(&page, 0x558), 0x5555);
        assert_eq!(rd64(&page, 0x550), 0x6666);
        assert_eq!(rd64(&page, 0x548), 0x7777);
        assert_eq!(rd64(&page, 0x570), 2);
        assert_eq!(page[0x4CB], 0);
        assert_eq!(rd32(&page, 0x058), 1);
        // TLB_CONTROL is 39:32 of 058h = byte 3 of the dword.
        assert_eq!(page[0x05B], 0);
        assert_eq!(rd32(&page, 0x090) & 1, 1);
        assert_eq!(rd32(&page, 0x00C), 1 | 1 << 18 | 1 << 24 | 1 << 27 | 1 << 28);
        assert_eq!(rd32(&page, 0x010), 1 | 1 << 1);
        assert_eq!(rd32(&page, 0x008), 1 << 1 | 1 << 6 | 1 << 13 | 1 << 14);
    }

    #[test]
    fn segment_roundtrip_matches_the_16_byte_layout() {
        let mut page = [0u8; VMCB_SIZE];
        let cs = Seg {
            selector: 0x08,
            attr: SegAttr::CODE32_DPL0,
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        {
            let mut vmcb = Vmcb::new(&mut page);
            vmcb.set_seg(save::CS, cs);
        }
        // selector @+0, attrib @+2, limit @+4, base @+8 — Table B-2 note.
        assert_eq!(rd16(&page, 0x410), 0x08);
        assert_eq!(rd16(&page, 0x412), SegAttr::CODE32_DPL0.0);
        assert_eq!(rd32(&page, 0x414), 0xFFFF_FFFF);
        assert_eq!(rd64(&page, 0x418), 0);
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(vmcb.seg(save::CS), cs);
        assert_eq!(vmcb.seg(save::ES), Seg { selector: 0, attr: SegAttr(0), limit: 0, base: 0 });
    }

    #[test]
    fn segattr_decodes_the_documented_words() {
        // 0x49B = type B (code, exec/read, accessed), S=1, DPL=0, P=1,
        // L=0, DB=1, G=0 — the APM's worked example, before granularity.
        let cs = SegAttr(0x49B);
        assert_eq!(cs.ty(), 0xB);
        assert!(cs.is_code_data());
        assert_eq!(cs.dpl(), 0);
        assert!(cs.is_present());
        assert!(!cs.is_long());
        assert!(cs.is_db());
        assert!(!cs.is_4k_granular());
        // The preset turns granularity on (0x800) — flat 4 GiB needs it.
        assert_eq!(SegAttr::CODE32_DPL0.0, 0x49B | 0x800);
        // 0x93 = type 3 (data, read/write, accessed), S=1, DPL=0, P=1.
        let ds = SegAttr(0x93);
        assert_eq!(ds.ty(), 0x3);
        assert!(ds.is_code_data());
        assert_eq!(ds.dpl(), 0);
        assert!(ds.is_present());
        assert!(!ds.is_db());
        // The preset turns granularity on (0x800) — flat 4 GiB needs it.
        assert_eq!(SegAttr::DATA32_RW_DPL0.0, 0x93 | 0x800);
        // NULL segment: all zero, P clear.
        assert!(!SegAttr(0).is_present());
        // DPL occupies bits 6:5.
        assert_eq!(SegAttr::from_fields(0xB, true, 3, true, false, false, true, false).0, 0x4FB);
    }

    #[test]
    fn exitcodes_decode_appendix_c() {
        assert_eq!(ExitCode::from_u64(0x81), ExitCode::Vmmcall);
        assert_eq!(ExitCode::from_u64(0x78), ExitCode::Hlt);
        assert_eq!(ExitCode::from_u64(0x77), ExitCode::Pause);
        assert_eq!(ExitCode::from_u64(0x72), ExitCode::Cpuid);
        assert_eq!(ExitCode::from_u64(0x80), ExitCode::Vmrun);
        assert_eq!(ExitCode::from_u64(0x87), ExitCode::Rdtscp);
        assert_eq!(ExitCode::from_u64(0x7B), ExitCode::Ioio);
        assert_eq!(ExitCode::from_u64(0x7C), ExitCode::Msr);
        assert_eq!(ExitCode::from_u64(0x64), ExitCode::Vintr);
        assert_eq!(ExitCode::from_u64(0x400), ExitCode::Npf);
        assert_eq!(ExitCode::from_u64(0xFFFF_FFFF), ExitCode::Invalid);
        assert_eq!(ExitCode::from_u64(0xFFFF_FFFE), ExitCode::Busy);
        // Exception exits are 040h + vector (Appendix C).
        assert_eq!(ExitCode::from_u64(0x41), ExitCode::Db);
        assert_eq!(ExitCode::from_u64(0x46), ExitCode::Ud);
        assert_eq!(ExitCode::from_u64(0x4D), ExitCode::Gp);
        assert_eq!(ExitCode::from_u64(0x4E), ExitCode::Pf);
        assert_eq!(ExitCode::from_u64(0x123), ExitCode::Other(0x123));
    }

    #[test]
    fn intercept_bits_are_table_b1() {
        assert_eq!(intr0::CPUID, 1 << 18);
        assert_eq!(intr0::HLT, 1 << 24);
        assert_eq!(intr0::IOIO_PROT, 1 << 27);
        assert_eq!(intr0::MSR_PROT, 1 << 28);
        assert_eq!(intr0::INTR, 1 << 0);
        assert_eq!(intr1::VMRUN, 1 << 0);
        assert_eq!(intr1::VMMCALL, 1 << 1);
        assert_eq!(intr1::EFER_WRITE, 1 << 15);
        assert_eq!(excp::DB, 1 << 1);
        assert_eq!(excp::UD, 1 << 6);
        assert_eq!(excp::GP, 1 << 13);
        assert_eq!(excp::PF, 1 << 14);
    }

    #[test]
    fn guest_instr_bytes_run_backwards_from_the_count() {
        let mut page = [0u8; VMCB_SIZE];
        // An instruction's last byte lands nearest the count field:
        // bytes stored at +1.. are reversed relative to execution order.
        page[ctrl::GUEST_INSTR] = 3;
        page[ctrl::GUEST_INSTR + 1] = 0x0F; // last byte of the instruction
        page[ctrl::GUEST_INSTR + 2] = 0x01;
        page[ctrl::GUEST_INSTR + 3] = 0xD9; // first byte executed
        let vmcb = Vmcb::new(&mut page);
        let (cnt, bytes) = vmcb.guest_instr();
        assert_eq!(cnt, 3);
        assert_eq!(bytes[0], 0x0F);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 0xD9);
    }

    // ---- the §15.5.1 validator ----

    /// A guest state that must pass: the phase-0 shape — 32-bit flat
    /// protected mode, no guest paging, SVME on, ASID 1, VMRUN intercepted.
    fn phase0_page() -> [u8; VMCB_SIZE] {
        let mut page = [0u8; VMCB_SIZE];
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME);
        vmcb.set_cr0(cr0::PE | cr0::NE);
        vmcb.set_cr4(0);
        vmcb.set_rflags(2);
        vmcb.set_cpl(0);
        vmcb.set_asid(1);
        vmcb.set_intr1(intr1::VMRUN);
        let cs = Seg { selector: 0x08, attr: SegAttr::CODE32_DPL0, limit: 0xFFFF_FFFF, base: 0 };
        vmcb.set_seg(save::CS, cs);
        page
    }

    #[test]
    fn flat_protected_no_paging_guest_is_legal() {
        let mut page = phase0_page();
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(validate_guest_state(&vmcb), Ok(()));
    }

    #[test]
    fn each_consistency_check_fires_on_its_own_violation() {
        // SVME clear -> SvmeClear.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_efer(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::SvmeClear));

        // CD=0 with NW=1 -> Cr0CdWithoutNw.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | cr0::NW);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Cr0CdWithoutNw));
        // ...but CD=1 with NW=1 is the historical reset shape and is legal.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | cr0::NW | cr0::CD);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Ok(()));

        // CR0[63:32] set -> Cr0HighBits.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Cr0HighBits));

        // DR6[63:32] set -> Dr6HighBits.
        let mut page = phase0_page();
        wr64(&mut page, 0x568, 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Dr6HighBits));

        // DR7[63:32] set -> Dr7HighBits.
        let mut page = phase0_page();
        wr64(&mut page, 0x560, 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Dr7HighBits));

        // LMA without LME -> LmaWithoutLme.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_efer(msr::EFER_SVME | msr::EFER_LMA);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmaWithoutLme));

        // LME & PG without PAE -> LmePgWithoutPae.
        let mut page = phase0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPae));

        // LME & PG without PE -> LmePgWithoutPe.
        let mut page = phase0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPe));

        // LME & PG & PAE with CS.L and CS.D both set -> LongModeLAndD.
        let mut page = phase0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        let bad_cs = Seg {
            selector: 0x08,
            attr: SegAttr::from_fields(0xB, true, 0, true, false, true, true, false),
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        vmcb.set_seg(save::CS, bad_cs);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LongModeLAndD));

        // ASID 0 -> AsidZero.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_asid(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::AsidZero));

        // VMRUN intercept clear -> VmrunNotIntercepted.
        let mut page = phase0_page();
        Vmcb::new(&mut page).set_intr1(intr1::VMMCALL);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::VmrunNotIntercepted));
    }

    /// The long-mode shape: phase 0's flat state with long mode switched
    /// on — guest paging up (`PG` + `PAE`), `LME` and `LMA` set, a 64-bit
    /// CS. This is the state the paged guest phase loads and what
    /// `LongModeLAndD` guards.
    fn long_mode_page() -> [u8; VMCB_SIZE] {
        let mut page = phase0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME | msr::EFER_LMA);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        vmcb.set_seg(
            save::CS,
            Seg { selector: 0x08, attr: SegAttr::CODE64_DPL0, limit: 0xFFFF_FFFF, base: 0 },
        );
        page
    }

    /// L=1 with D=0 is the whole long-mode segment attribute: type `B`,
    /// S=1, P=1, raw word 0x29B (0xB + S + P + L).
    #[test]
    fn code64_attr_is_long_not_db() {
        let a = SegAttr::CODE64_DPL0;
        assert_eq!(a.ty(), 0xB);
        assert!(a.is_code_data());
        assert_eq!(a.dpl(), 0);
        assert!(a.is_present());
        assert!(a.is_long());
        assert!(!a.is_db());
        assert!(!a.is_4k_granular());
        assert_eq!(a.0, 0x29B);
    }

    #[test]
    fn long_mode_paged_guest_is_legal() {
        let mut page = long_mode_page();
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(validate_guest_state(&vmcb), Ok(()));
    }

    /// The long-mode state minus one ingredient must fail the one check the
    /// missing ingredient belongs to — the exact failure a transposed bit
    /// in a VMCB builder would take.
    #[test]
    fn long_mode_guest_loses_legality_one_ingredient_at_a_time() {
        // No PAE -> LmePgWithoutPae.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_cr4(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPae));

        // No PE -> LmePgWithoutPe.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_cr0(cr0::NE | cr0::PG);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPe));

        // CS.L and CS.D together -> LongModeLAndD.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_seg(
            save::CS,
            Seg {
                selector: 0x08,
                attr: SegAttr::from_fields(0xB, true, 0, true, false, true, true, false),
                limit: 0xFFFF_FFFF,
                base: 0,
            },
        );
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LongModeLAndD));
    }

    #[test]
    fn npt_encodings_are_the_documented_subset() {
        assert_eq!(npt::pointer(0x1234_5000), 0x1234_5000 | 0b111);
        assert_eq!(npt::leaf_2m(0x200_0000), 0x200_0000 | npt::P | npt::RW | npt::US | npt::PS);
        assert_eq!(npt::leaf_2m(0x200_0000) & npt::PS != 0, true);
        // The 4-KiB leaf carries no PS: bit 7 belongs to the 2-MiB PDE form.
        assert_eq!(npt::leaf_4k(0x1234_5000), 0x1234_5000 | 0b111);
        assert_eq!(npt::leaf_4k(0x1234_5000) & npt::PS, 0);
        assert_eq!(0x1234_5000 & npt::ADDR_MASK, 0x1234_5000);
        assert_eq!(npt::ADDR_MASK & 0xFFF, 0);
    }

    /// Numeric pins for the NPT bit vocabulary. The builders *are* these
    /// constants, so a symbolic test stays green through a transposition —
    /// the class of error `cr4::PAE` rode on. Figures 519–529 (Vol 2 §5.3)
    /// pin the positions; §15.25.5/§15.25.7 pin the semantics recorded in
    /// the module doc.
    #[test]
    fn npt_bits_are_vol2_fig519_529() {
        assert_eq!(npt::P, 1 << 0);
        assert_eq!(npt::RW, 1 << 1);
        assert_eq!(npt::US, 1 << 2);
        assert_eq!(npt::PWT, 1 << 3);
        assert_eq!(npt::PCD, 1 << 4);
        assert_eq!(npt::A, 1 << 5);
        assert_eq!(npt::D, 1 << 6);
        assert_eq!(npt::PS, 1 << 7);
        assert_eq!(npt::G, 1 << 8);
        assert_eq!(npt::PAT, 1 << 12);
        assert_eq!(npt::NX, 1 << 63);
        assert_eq!(npt::ADDR_MASK, 0x000F_FFFF_FFFF_F000);
    }

    /// The NPF error code (§15.25.6, p. 573): #PF-shaped low bits, the
    /// walk-order bits at 32/33, the shadow-stack bit at 37. Bit 5 stays
    /// reserved — a constant pinned at 1 << 5 would be the PAE slip again.
    #[test]
    fn npf_error_bits_are_vol2_p573() {
        assert_eq!(npf::P, 1 << 0);
        assert_eq!(npf::RW, 1 << 1);
        assert_eq!(npf::US, 1 << 2);
        assert_eq!(npf::RSV, 1 << 3);
        assert_eq!(npf::ID, 1 << 4);
        assert_eq!(npf::SS, 1 << 6);
        assert_eq!(npf::FINAL, 1 << 32);
        assert_eq!(npf::GPT_WALK, 1 << 33);
        assert_eq!(npf::SHADOW_STACK, 1 << 37);
    }

    #[test]
    fn illicit_names_are_distinct_serial_strings() {
        let all = [
            Illicit::SvmeClear,
            Illicit::Cr0CdWithoutNw,
            Illicit::Cr0HighBits,
            Illicit::Dr6HighBits,
            Illicit::Dr7HighBits,
            Illicit::LmaWithoutLme,
            Illicit::LmePgWithoutPae,
            Illicit::LmePgWithoutPe,
            Illicit::LongModeLAndD,
            Illicit::AsidZero,
            Illicit::VmrunNotIntercepted,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.name().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.name(), b.name());
            }
        }
    }

    /// §15.11 / Table 158: four 16-Kbit vectors, the three MSR ranges and
    /// the reserved fourth; two bits per MSR, lsb read, msb write.
    #[test]
    fn msrpm_offsets_match_table_158() {
        assert_eq!(msrpm::SIZE, 8192);
        assert_eq!(msrpm::VECTOR, 2048);
        // EFER (0xC000_0080) sits in the C000_0000h vector: byte
        // 0x800 + 0x80/4, bits (0x80 % 4)*2..+1.
        assert_eq!(msrpm::bits_of(0xC000_0080), Some((0x800 + 0x20, 0)));
        // First and last MSR of two areas — the byte/bit arithmetic.
        assert_eq!(msrpm::bits_of(0x0000_0000), Some((0, 0)));
        assert_eq!(msrpm::bits_of(0x0000_1FFF), Some((2047, 6)));
        assert_eq!(msrpm::bits_of(0xC001_0000), Some((2 * 2048, 0)));
        assert_eq!(msrpm::bits_of(0xC001_1FFF), Some((3 * 2048 - 1, 6)));
        // The reserved fourth vector (and every uncovered MSR) has no bits:
        // the auto-intercept, which `bits` reports as intercepted.
        assert_eq!(msrpm::bits_of(0x0000_2000), None);
        assert_eq!(msrpm::bits_of(0xC000_2000), None);
        assert_eq!(msrpm::bits_of(0xC001_2000), None);
        assert_eq!(msrpm::bits(&[0; msrpm::SIZE], 0xC000_2000), (true, true));
    }

    #[test]
    fn msrpm_intercept_bits_roundtrip() {
        let page = msrpm::with_access([0; msrpm::SIZE], msr::EFER, true, false);
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        // The neighbouring MSR shares a byte — it must stay clear.
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (false, false));
        let page = msrpm::with_access(page, 0xC000_0081, true, true);
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (true, true));
        let page = msrpm::with_access(page, 0xC000_0081, false, false);
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (false, false));
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        // Arming a reserved-vector MSR is a no-op, not a panic.
        let _ = msrpm::with_access([0; msrpm::SIZE], 0xC000_4000, true, true);
    }

    /// §15.10.1: 12 contiguous KiB = 64K+3 bits, one bit per port,
    /// bit 0 = port 0, 1 = intercept.
    #[test]
    fn iopm_is_one_bit_per_port() {
        assert_eq!(iopm::SIZE, 12288);
        assert_eq!(iopm::PORTS, 65536);
        let page = iopm::with_port([0; iopm::SIZE], 0, true);
        assert!(iopm::intercepted(&page, 0));
        assert!(!iopm::intercepted(&page, 1));
        // Port 0x80 (POST): byte 16, bit 0.
        let page = iopm::with_port(page, 0x80, true);
        assert!(iopm::intercepted(&page, 0x80));
        assert_eq!(page[0x10], 0x01);
        // The last port: last byte, last bit.
        let page = iopm::with_port(page, u16::MAX, true);
        assert!(iopm::intercepted(&page, u16::MAX));
        assert_eq!(page[8191], 0x80);
        // Clearing one port of an all-intercept map clears only its bit.
        let page = iopm::with_port([0xFF; iopm::SIZE], 0x1234, false);
        assert!(!iopm::intercepted(&page, 0x1234));
        assert!(iopm::intercepted(&page, 0x1235));
    }

    /// The phase-4 bug as a standing test: `0x5000_0000` names PD slot 128,
    /// not slot 0. Every level's index derived from the VA, pinned here for
    /// a VA that caught the hand-computed kind.
    #[test]
    fn table_indices_are_derived_from_the_va() {
        let va = 0x5000_0000; // 1 GiB + 256 MiB
        assert_eq!(tables::idx(va, tables::PML4_SHIFT), 0);
        assert_eq!(tables::idx(va, tables::PDPT_SHIFT), 1);
        assert_eq!(tables::idx(va, tables::PD_SHIFT), 128);
        assert_eq!(tables::idx(va, tables::PT_SHIFT), 0);
        // One bit below the PD field walks back to slot 127.
        assert_eq!(tables::idx(va - 0x1000, tables::PD_SHIFT), 127);
        // A VA high in the canonical half hits the top slot.
        assert_eq!(tables::idx(0xFFFF_FFFF_FFFF_F000, tables::PML4_SHIFT), 511);
    }

    /// A walk over a table built exactly the way the two-level guest builds
    /// its own — PML4 → PDPT → PD → PT, 4-KiB leaves — must reach the code
    /// frame. This is the host-side rehearsal the phase-4 note asked for: a
    /// builder that writes a slot by hand fails here before any guest runs.
    #[test]
    fn walk_reads_a_built_table_the_way_the_mmu_does() {
        struct FourPages {
            pml4: [u64; 512],
            pdpt: [u64; 512],
            pd: [u64; 512],
            pt: [u64; 512],
        }
        impl tables::PageSource for FourPages {
            fn read(&self, table_pa: u64, off: usize) -> u64 {
                match table_pa {
                    0x1000 => self.pml4[off / 8],
                    0x2000 => self.pdpt[off / 8],
                    0x3000 => self.pd[off / 8],
                    _ => self.pt[off / 8],
                }
            }
        }
        let mut b = FourPages {
            pml4: [0; 512],
            pdpt: [0; 512],
            pd: [0; 512],
            pt: [0; 512],
        };
        let (pml4, pdpt, pd, pt, code) = (0x1000u64, 0x2000u64, 0x3000u64, 0x4000u64, 0x9000u64);
        let va = 0x5000_0000u64;
        b.pml4[tables::idx(va, tables::PML4_SHIFT)] = npt::pointer(pdpt);
        b.pdpt[tables::idx(va, tables::PDPT_SHIFT)] = npt::pointer(pd);
        b.pd[tables::idx(va, tables::PD_SHIFT)] = npt::pointer(pt);
        b.pt[tables::idx(va, tables::PT_SHIFT)] = npt::leaf_4k(code);

        let leaf = tables::walk(&b, pml4, va).unwrap();
        assert_eq!(leaf.shift, tables::PT_SHIFT);
        assert_eq!(tables::translate(&b, pml4, va), Ok(code));
        // An offset inside the page lands inside the frame.
        assert_eq!(tables::translate(&b, pml4, va + 0xABC), Ok(code + 0xABC));
        // An unmapped VA in the same PDPT reports the level it stopped at.
        assert_eq!(tables::walk(&b, pml4, 0x4000_0000), Err(tables::WalkError::Absent(tables::PD_SHIFT)));
        // A zero pointer entry is a missing page, not a leaf.
        b.pml4[tables::idx(va, tables::PML4_SHIFT)] = 0;
        assert_eq!(tables::walk(&b, pml4, va), Err(tables::WalkError::Absent(tables::PML4_SHIFT)));
    }

    /// A 2-MiB leaf short-circuits at the PD: the PS-bit level, and the
    /// offset inside the page is the 21 low bits, not the 12.
    #[test]
    fn walk_stops_at_a_large_page_leaf() {
        struct TwoPages {
            pml4: [u64; 512],
            pd: [u64; 512],
        }
        impl tables::PageSource for TwoPages {
            fn read(&self, table_pa: u64, off: usize) -> u64 {
                if table_pa == 0x1000 {
                    self.pml4[off / 8]
                } else {
                    self.pd[off / 8]
                }
            }
        }
        let mut src = TwoPages { pml4: [0; 512], pd: [0; 512] };
        // PML4 1, PDPT 2, PD 2: three distinct slots, 2-MiB region 0x40_0000.
        let va = 0x8040_0000u64;
        let pd_frame = 0x2000_0000u64;
        src.pml4[tables::idx(va, tables::PML4_SHIFT)] = npt::pointer(0x1000);
        src.pml4[tables::idx(va, tables::PDPT_SHIFT)] = npt::pointer(0x2000);
        src.pd[tables::idx(va, tables::PD_SHIFT)] = npt::leaf_2m(pd_frame);
        let leaf = tables::walk(&src, 0x1000, va).unwrap();
        assert_eq!(leaf.shift, tables::PD_SHIFT);
        // The page base itself has zero offset inside the 2-MiB leaf.
        assert_eq!(tables::translate(&src, 0x1000, va), Ok(pd_frame));
        // 0x800 inside the page rides along, proving the mask is 21 bits.
        assert_eq!(tables::translate(&src, 0x1000, va + 0x800), Ok(pd_frame + 0x800));
    }

    /// An ELF64 built byte-by-byte in the test, read back through the
    /// parser: the segment list is the PT_LOAD set in header order, and the
    /// Xen type-18 note names the 32-bit entry.
    #[test]
    fn elf_image_roundtrips_segments_and_pvh_note() {
        let mut img = [0u8; 0x400];
        img[..4].copy_from_slice(&elf::MAGIC);
        img[4] = elf::CLASS64;
        img[5] = elf::DATA_LE;
        img[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        img[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        img[0x38..0x3A].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
        // PHDR 0: PT_LOAD text at pa 0x100_0000, bytes at file offset 0x200.
        let ph0 = 64;
        img[ph0..ph0 + 4].copy_from_slice(&elf::PT_LOAD.to_le_bytes());
        img[ph0 + 0x08..ph0 + 0x10].copy_from_slice(&0x200u64.to_le_bytes());
        img[ph0 + 0x18..ph0 + 0x20].copy_from_slice(&0x100_0000u64.to_le_bytes());
        img[ph0 + 0x20..ph0 + 0x28].copy_from_slice(&0x178u64.to_le_bytes());
        img[ph0 + 0x28..ph0 + 0x30].copy_from_slice(&0x178u64.to_le_bytes());
        // PHDR 1: PT_NOTE holding the PVH entry note (name "Xen", type 18,
        // descriptor = entry 0x120_0000).
        let ph1 = 64 + 56;
        img[ph1..ph1 + 4].copy_from_slice(&elf::PT_NOTE.to_le_bytes());
        img[ph1 + 0x08..ph1 + 0x10].copy_from_slice(&0x300u64.to_le_bytes());
        img[ph1 + 0x20..ph1 + 0x28].copy_from_slice(&20u64.to_le_bytes()); // 12+4+4
        img[ph1 + 0x28..ph1 + 0x30].copy_from_slice(&20u64.to_le_bytes());
        img[0x300..0x304].copy_from_slice(&4u32.to_le_bytes()); // namesz
        img[0x304..0x308].copy_from_slice(&4u32.to_le_bytes()); // descsz
        img[0x308..0x30C].copy_from_slice(&18u32.to_le_bytes()); // type
        img[0x30C..0x310].copy_from_slice(b"Xen\0");
        img[0x310..0x314].copy_from_slice(&0x120_0000u32.to_le_bytes());

        let image = elf::Image::parse(&img).unwrap();
        let mut segs = [None; 4];
        let mut n = 0;
        for s in image.segments() {
            segs[n] = Some(s);
            n += 1;
        }
        assert_eq!(n, 1);
        let s = segs[0].unwrap();
        assert_eq!(s.offset, 0x200);
        assert_eq!(s.pa, 0x100_0000);
        assert_eq!(s.filesz, 0x178);
        assert_eq!(image.entry_phys32(), Some(0x120_0000));

        // The rejects: short, bad magic, 32-bit class, big-endian.
        assert!(elf::Image::parse(&img[..32]).is_none());
        let mut bad = img;
        bad[0] = b'X';
        assert!(elf::Image::parse(&bad).is_none());
        let mut bad = img;
        bad[4] = 1;
        assert!(elf::Image::parse(&bad).is_none());
        let mut bad = img;
        bad[5] = 2;
        assert!(elf::Image::parse(&bad).is_none());
        // A program-header table running past the image is a reject:
        // 64 + 18*56 = 1072 > 0x400.
        let mut bad = img;
        bad[0x38..0x3A].copy_from_slice(&18u16.to_le_bytes());
        assert!(elf::Image::parse(&bad).is_none());
        // No note segment -> no entry (every bzImage).
        let mut bare = [0u8; 0x400];
        bare[..4].copy_from_slice(&elf::MAGIC);
        bare[4] = elf::CLASS64;
        bare[5] = elf::DATA_LE;
        bare[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        bare[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        bare[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        bare[64..68].copy_from_slice(&elf::PT_LOAD.to_le_bytes());
        assert_eq!(elf::Image::parse(&bare).unwrap().entry_phys32(), None);
    }

    /// The note number, pinned numerically and against the loader contract:
    /// `XEN_ELFNOTE_PHYS32_ENTRY` is 18. The synthetic-image test above builds
    /// its note from the same constant, so it can never catch a wrong value —
    /// this one can (the constant was 9, `XEN_ELFNOTE_LOADER`).
    #[test]
    fn pvh_note_type_is_xen_elfnote_phys32_entry() {
        assert_eq!(elf::PVH_NOTE_TYPE, 18);
        assert_eq!(elf::PVH_NOTE_NAME, b"Xen");
    }

    /// A note segment as a real kernel lays it out: a note with an odd-length
    /// name and descriptor *before* the Xen one, each padded to 4 bytes. The
    /// entry is found only if the parser steps by the padded sizes.
    #[test]
    fn pvh_entry_is_found_behind_padded_notes() {
        let mut img = [0u8; 0x200];
        img[..4].copy_from_slice(&elf::MAGIC);
        img[4] = elf::CLASS64;
        img[5] = elf::DATA_LE;
        img[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        img[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        img[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        img[ph..ph + 4].copy_from_slice(&elf::PT_NOTE.to_le_bytes());
        img[ph + 0x08..ph + 0x10].copy_from_slice(&0x100u64.to_le_bytes());
        let total = (12 + 8 + 4) + (12 + 4 + 4); // "Linux\0"+3 bytes, padded; then Xen
        img[ph + 0x20..ph + 0x28].copy_from_slice(&(total as u64).to_le_bytes());
        // note 1: namesz 6 ("Linux\0", padded to 8), descsz 3 (padded to 4), type 0x100
        img[0x100..0x104].copy_from_slice(&6u32.to_le_bytes());
        img[0x104..0x108].copy_from_slice(&3u32.to_le_bytes());
        img[0x108..0x10C].copy_from_slice(&0x100u32.to_le_bytes());
        img[0x10C..0x112].copy_from_slice(b"Linux\0");
        img[0x114..0x117].copy_from_slice(&[1, 2, 3]);
        // note 2: the Xen entry note, right after the padding.
        let n2 = 0x100 + 12 + 8 + 4;
        img[n2..n2 + 4].copy_from_slice(&4u32.to_le_bytes());
        img[n2 + 4..n2 + 8].copy_from_slice(&4u32.to_le_bytes());
        img[n2 + 8..n2 + 12].copy_from_slice(&18u32.to_le_bytes());
        img[n2 + 12..n2 + 16].copy_from_slice(b"Xen\0");
        img[n2 + 16..n2 + 20].copy_from_slice(&0x0100_0140u32.to_le_bytes());
        assert_eq!(elf::Image::parse(&img).unwrap().entry_phys32(), Some(0x0100_0140));
    }

    /// The real thing, when one is at hand: `AKUMA_TEST_VMLINUX=<path>` (the
    /// Firecracker CI `vmlinux-6.1.128`). Skipped silently otherwise — a 40 MB
    /// kernel is not a repo file. Pins what the Linux phase's loader will see:
    /// four `PT_LOAD`s from physical 16 MiB, and a PVH entry in that range.
    #[test]
    fn real_vmlinux_parses_when_provided() {
        let Ok(path) = std::env::var("AKUMA_TEST_VMLINUX") else { return };
        let bytes = std::fs::read(path).expect("AKUMA_TEST_VMLINUX is unreadable");
        let image = elf::Image::parse(&bytes).expect("not an ELF64 LE image");
        let segs: std::vec::Vec<_> = image.segments().collect();
        assert_eq!(segs.len(), 4, "vmlinux-6.1.128 has four PT_LOADs");
        assert_eq!(segs[0].pa, 0x100_0000);
        let entry = image.entry_phys32().expect("no PVH note found in a PVH-capable kernel");
        assert!(
            (0x100_0000..0x200_0000).contains(&u64::from(entry)),
            "PVH entry {entry:#x} outside the text segment"
        );
    }

    /// `EXITINFO1` for an intercepted IN/OUT, decoded field by field against
    /// Figure 15-2 (§15.10.2): PORT 31:16, SEG 12:10, A64/A32/A16 9/8/7,
    /// SZ32/SZ16/SZ8 6/5/4, REP 3, STR 2, TYPE 0 (1 = IN).
    #[test]
    fn ioio_exitinfo1_is_vol2_figure_15_2() {
        // `in al, dx` with dx = 0x3FD (the 16550 line-status register).
        let io = exitinfo::Io::decode(0x03FD_0011);
        assert_eq!(io.port, 0x3FD);
        assert!(io.is_in);
        assert!(!io.string && !io.rep);
        assert_eq!(io.size, 1);
        assert_eq!(io.mask(), 0xFF);
        // `out dx, eax` to 0xCF8: SZ32, TYPE 0.
        let io = exitinfo::Io::decode(0x0CF8_0040);
        assert_eq!((io.port, io.is_in, io.size), (0xCF8, false, 4));
        assert_eq!(io.mask(), 0xFFFF_FFFF);
        // `rep outsw` with 32-bit addressing through ES (seg 0) vs DS (seg 3).
        let io = exitinfo::Io::decode((0x60 << 16) | (3 << 10) | (1 << 8) | (1 << 5) | (1 << 3) | (1 << 2));
        assert_eq!((io.port, io.is_in, io.string, io.rep), (0x60, false, true, true));
        assert_eq!((io.size, io.addr_bits, io.seg), (2, 32, 3));
        // The three address-size bits, one at a time.
        assert_eq!(exitinfo::Io::decode(1 << 7).addr_bits, 16);
        assert_eq!(exitinfo::Io::decode(1 << 8).addr_bits, 32);
        assert_eq!(exitinfo::Io::decode(1 << 9).addr_bits, 64);
        // The architecture never sets none; the decoder says so rather than guess.
        assert_eq!(exitinfo::Io::decode(0).size, 0);
        // MSR exits: EXITINFO1 = 0 is RDMSR, 1 is WRMSR (§15.11).
        assert!(!exitinfo::msr_is_write(0));
        assert!(exitinfo::msr_is_write(1));
    }

    /// The save-area MSR fields of Table B-2 (rev 3.45, pp. 770-771), by offset.
    #[test]
    fn save_area_msr_offsets_are_table_b2() {
        assert_eq!(save::STAR, 0x200);
        assert_eq!(save::LSTAR, 0x208);
        assert_eq!(save::CSTAR, 0x210);
        assert_eq!(save::SFMASK, 0x218);
        assert_eq!(save::KERNEL_GS_BASE, 0x220);
        assert_eq!(save::SYSENTER_CS, 0x228);
        assert_eq!(save::SYSENTER_ESP, 0x230);
        assert_eq!(save::SYSENTER_EIP, 0x238);
        assert_eq!(save::CR2, 0x240);
        assert_eq!(save::G_PAT, 0x268);
        assert_eq!(save::LASTEXCPFROM, 0x288);
    }

    /// What CPUID removes, pinned to the Vol 3 feature tables, and the leaves
    /// that read as zero.
    #[test]
    fn guest_cpuid_hides_what_the_hypervisor_does_not_provide() {
        let all = [0xFFFF_FFFF; 4];
        // Fn0000_0001 ECX (Vol 3 Appendix E): 31 hypervisor, 29 F16C, 28 AVX,
        // 27 OSXSAVE, 26 XSAVE, 21 x2APIC, 12 FMA, 3 MONITOR.
        let l1 = cpuid::filter(1, all);
        for bit in [31, 29, 28, 27, 26, 21, 12, 3] {
            assert_eq!(l1[2] & (1 << bit), 0, "ECX bit {bit} must be hidden");
        }
        // What stays: SSE3 0, PCLMULQDQ 1, SSSE3 9, CMPXCHG16B 13, SSE4.1/4.2,
        // MOVBE 22, POPCNT 23, AES 25, RDRAND 30.
        for bit in [0, 1, 9, 13, 19, 20, 22, 23, 25, 30] {
            assert_ne!(l1[2] & (1 << bit), 0, "ECX bit {bit} must survive");
        }
        // EDX: APIC 9, MCE 7, MTRR 12, MCA 14, HTT 28 hidden; TSC 4, MSR 5,
        // PAE 6, PAT 16, FXSR 24, SSE 25, SSE2 26 stay.
        for bit in [9, 7, 12, 14, 28] {
            assert_eq!(l1[3] & (1 << bit), 0, "EDX bit {bit} must be hidden");
        }
        for bit in [4, 5, 6, 16, 24, 25, 26] {
            assert_ne!(l1[3] & (1 << bit), 0, "EDX bit {bit} must survive");
        }
        // EAX (family/model/stepping) and EBX (APIC id, brand) pass through.
        assert_eq!((l1[0], l1[1]), (0xFFFF_FFFF, 0xFFFF_FFFF));
        // Fn8000_0001 ECX: SVM 2 and TopologyExtensions 22 hidden; LahfSahf 0
        // and ABM 5 stay. EDX (NX 20, SYSCALL 11, LM 29) is untouched.
        let e1 = cpuid::filter(0x8000_0001, all);
        for bit in [2, 3, 9, 22, 23] {
            assert_eq!(e1[2] & (1 << bit), 0, "ext ECX bit {bit} must be hidden");
        }
        for bit in [0, 5, 8] {
            assert_ne!(e1[2] & (1 << bit), 0, "ext ECX bit {bit} must survive");
        }
        assert_eq!(e1[3], 0xFFFF_FFFF);
        // Zeroed leaves.
        for leaf in [4, 5, 6, 7, 0xA, 0xB, 0xD, 0x1F, 0x4000_0000, 0x4000_0001, 0x8000_000A, 0x8000_001D, 0x8000_001E] {
            assert_eq!(cpuid::filter(leaf, all), [0; 4], "leaf {leaf:#x} must read zero");
        }
        // Passed through untouched.
        for leaf in [0, 2, 3, 0x8000_0000, 0x8000_0002, 0x8000_0007, 0x8000_0008] {
            assert_eq!(cpuid::filter(leaf, all), all, "leaf {leaf:#x} must pass through");
        }
    }

    /// `V_INTR_MASKING` is bit 24 of the qword at `060h` (Table B-1), and the
    /// setter writes it there.
    #[test]
    fn v_intr_masking_is_bit_24_of_060h() {
        assert_eq!(ctrl::V_INTR_MASKING, 1 << 24);
        assert_eq!(ctrl::V_INTR, 0x060);
        let mut page = [0u8; VMCB_SIZE];
        Vmcb::new(&mut page).set_v_intr(ctrl::V_INTR_MASKING);
        assert_eq!(&page[0x60..0x68], &0x0100_0000u64.to_le_bytes());
    }

    /// The virtual-interrupt fields of `060h` / `068h`, bit by bit against
    /// Table B-1: V_IRQ 8, V_IGN_TPR 20, V_INTR_MASKING 24, V_INTR_VECTOR 39:32,
    /// INTERRUPT_SHADOW bit 0 of the next qword.
    #[test]
    fn virtual_interrupt_fields_are_table_b_1() {
        assert_eq!(ctrl::V_IRQ, 0x100);
        assert_eq!(ctrl::V_IGN_TPR, 0x10_0000);
        assert_eq!(ctrl::v_intr_extint(0x30), 0x0000_0030_0110_0100);
        assert_eq!(ctrl::v_intr_extint(0xEF) >> 32, 0xEF);
        let mut page = [0u8; VMCB_SIZE];
        page[0x68] = 1;
        assert!(Vmcb::new(&mut page).interrupt_shadow());
        page[0x68] = 2; // GUEST_INTERRUPT_MASK is SEV-ES's, not the shadow.
        assert!(!Vmcb::new(&mut page).interrupt_shadow());
        Vmcb::new(&mut page).set_v_intr(ctrl::v_intr_extint(0x20));
        assert_eq!(Vmcb::new(&mut page).v_intr() & ctrl::V_IRQ, ctrl::V_IRQ);
    }

    /// `EVENTINJ`, bit by bit against Figure 15-5 / Table 15-11.
    #[test]
    fn eventinj_is_vol2_figure_15_5() {
        // #UD: vector 6, no error code. V (31) | TYPE=3 (10:8) | VECTOR.
        assert_eq!(eventinj::exception(6, None), 0x8000_0306);
        // #GP(0): vector 13 pushes an error code, EV (11) set.
        assert_eq!(eventinj::exception(13, Some(0)), 0x8000_0B0D);
        // #PF with error code 0x7: ERRORCODE rides bits 63:32.
        assert_eq!(eventinj::exception(14, Some(7)), 0x0000_0007_8000_0B0E);
        assert_eq!(eventinj::VALID, 1 << 31);
        assert_eq!(eventinj::ERROR_CODE_VALID, 1 << 11);
        assert_eq!(eventinj::TYPE_EXCEPTION >> 8, 3);
        assert_eq!(excp::DF, 1 << 8);
    }

    /// The 16550 as the early console and the 8250 probe see it.
    #[test]
    fn uart_transmits_and_answers_the_probe() {
        let mut u = devices::Uart::new();
        // Always ready to send, nothing received, no interrupt pending.
        assert_eq!(u.read(5), 0x60);
        assert_eq!(u.read(2), 0x01);
        assert_eq!(u.read(0), 0);
        // A character written to the transmit register comes back out.
        assert_eq!(u.write(0, b'L'), Some(b'L'));
        // With DLAB set, registers 0/1 are the divisor latch and send nothing.
        assert_eq!(u.write(3, 0x83), None);
        assert_eq!(u.write(0, 0x01), None);
        assert_eq!(u.write(1, 0x00), None);
        assert_eq!((u.read(0), u.read(1)), (0x01, 0x00));
        // DLAB closed again: register 1 is IER, register 0 transmits.
        assert_eq!(u.write(3, 0x03), None);
        assert_eq!(u.read(3), 0x03);
        assert_eq!(u.write(1, 0xFF), None);
        assert_eq!(u.read(1), 0x0F);
        assert_eq!(u.write(0, b'x'), Some(b'x'));
        // The scratch register reads back what was written.
        u.write(7, 0xA5);
        assert_eq!(u.read(7), 0xA5);
        // Out of loopback the modem lines read DCD|DSR|CTS.
        assert_eq!(u.read(6), 0xB0);
        // Loopback: outputs feed inputs and nothing is transmitted. The 8250
        // driver writes MCR = LOOP|0x0A (RTS, OUT2) and expects MSR high bits
        // 0x90 (CTS, DCD).
        u.write(4, 0x10 | 0x0A);
        assert_eq!(u.read(6) & 0xF0, 0x90);
        assert_eq!(u.write(0, b'!'), None);
    }

    /// The transmit-empty interrupt an interrupt-driven 8250 driver lives on:
    /// raised by enabling it, re-armed by each character, cleared by reading
    /// `IIR`, gated by `OUT2`.
    #[test]
    fn uart_thre_interrupt_follows_the_datasheet() {
        let mut u = devices::Uart::new();
        u.write(4, 0x08); // OUT2
        assert!(!u.irq_level());
        u.write(1, 0x02); // ETBEI
        assert!(u.irq_level());
        assert_eq!(u.read(2), 0x02, "a peek does not clear");
        assert_eq!(u.read_clearing(2), 0x02);
        assert!(!u.irq_level());
        assert_eq!(u.read_clearing(2), 0x01, "nothing pending now");
        // Each character sent empties the holding register again.
        u.write(0, b'a');
        assert!(u.irq_level());
        assert_eq!(u.read_clearing(2), 0x02);
        // Without OUT2 the line stays low though the source is pending.
        u.write(4, 0);
        u.write(0, b'b');
        assert!(!u.irq_level());
        assert_eq!(u.read(2), 0x02);
        // Disabling the source hides it.
        u.write(1, 0x00);
        assert_eq!(u.read(2), 0x01);
        // DLAB writes to register 0 are the divisor, not a character.
        u.write(4, 0x08);
        u.write(1, 0x02);
        u.read_clearing(2);
        u.write(3, 0x80);
        u.write(0, 0x01);
        assert!(!u.irq_level());
    }

    #[test]
    fn msr_store_remembers_until_full() {
        let mut m = devices::MsrStore::<2>::new();
        assert_eq!(m.get(0x277), None);
        assert!(m.set(0x277, 7));
        assert!(m.set(0x1A0, 1));
        assert!(m.set(0x277, 9)); // overwrite, not a new slot
        assert_eq!((m.get(0x277), m.get(0x1A0)), (Some(9), Some(1)));
        assert!(!m.set(0xC0000103, 5), "a full store refuses a third MSR");
        assert_eq!(m.get(0xC0000103), None);
    }

    /// The loader reads only the note segment out of a 40 MB file, so the
    /// stream walker must work on a bare slice — and not panic on garbage.
    #[test]
    fn find_pvh_entry_is_bounds_safe() {
        // note 1 (6-byte name, 3-byte desc, padded) then the Xen entry note.
        let mut n = [0u8; 44];
        n[0..4].copy_from_slice(&6u32.to_le_bytes());
        n[4..8].copy_from_slice(&3u32.to_le_bytes());
        n[8..12].copy_from_slice(&0x100u32.to_le_bytes());
        n[12..18].copy_from_slice(b"Linux\0");
        n[20..23].copy_from_slice(&[1, 2, 3]);
        n[24..28].copy_from_slice(&4u32.to_le_bytes());
        n[28..32].copy_from_slice(&4u32.to_le_bytes());
        n[32..36].copy_from_slice(&18u32.to_le_bytes());
        n[36..40].copy_from_slice(b"Xen\0");
        n[40..44].copy_from_slice(&0x0100_0140u32.to_le_bytes());
        assert_eq!(elf::find_pvh_entry(&n), Some(0x0100_0140));
        // Truncated anywhere: None, never a panic.
        for cut in 0..n.len() {
            let _ = elf::find_pvh_entry(&n[..cut]);
        }
        // A descriptor size that runs off the end, and an absurd one.
        let mut bad = n;
        bad[28..32].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(elf::find_pvh_entry(&bad), None);
        assert_eq!(elf::find_pvh_entry(&[]), None);
        // Right name, wrong type: not the entry.
        let mut other = n;
        other[32..36].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(elf::find_pvh_entry(&other), None);
    }

    /// The PVH wire format: `hvm_start_info` version 1 and its memory-map
    /// entry, byte for byte (Xen's public `start_info.h`).
    #[test]
    fn pvh_handoff_blobs_are_the_hvm_start_info_layout() {
        assert_eq!(pvh::START_MAGIC, 0x336E_C578);
        assert_eq!(pvh::START_VERSION, 1);
        assert_eq!(pvh::MEMMAP_RAM, 1);
        assert_eq!(pvh::START_SIZE, 56);
        assert_eq!(pvh::MEMMAP_ENTRY_SIZE, 24);

        let l = pvh::plan(0x1000_0000, 0, 0xFF00_0000);
        assert_eq!(l.cmdline_gpa, 0x1000_0000 - 0x3000);
        assert_eq!(l.memmap_gpa, 0x1000_0000 - 0x2000);
        assert_eq!(l.start_gpa, 0x1000_0000 - 0x1000);

        let mut page = [0u8; 4096];
        pvh::write_start(&mut page, &l);
        assert_eq!(&page[0x00..0x04], &0x336E_C578u32.to_le_bytes());
        assert_eq!(&page[0x04..0x08], &1u32.to_le_bytes()); // version
        assert_eq!(&page[0x08..0x0C], &0u32.to_le_bytes()); // flags
        assert_eq!(&page[0x0C..0x10], &0u32.to_le_bytes()); // nr_modules
        assert_eq!(&page[0x10..0x18], &0u64.to_le_bytes()); // modlist_paddr
        assert_eq!(&page[0x18..0x20], &l.cmdline_gpa.to_le_bytes());
        assert_eq!(&page[0x20..0x28], &0u64.to_le_bytes()); // rsdp = none
        assert_eq!(&page[0x28..0x30], &l.memmap_gpa.to_le_bytes());
        assert_eq!(&page[0x30..0x34], &1u32.to_le_bytes()); // memmap_entries
        assert_eq!(&page[0x34..0x38], &0u32.to_le_bytes()); // reserved

        let mut page = [0u8; 4096];
        pvh::write_memmap(&mut page, &l);
        assert_eq!(&page[0x00..0x08], &0u64.to_le_bytes()); // addr
        assert_eq!(&page[0x08..0x10], &0xFF00_0000u64.to_le_bytes()); // size
        assert_eq!(&page[0x10..0x14], &1u32.to_le_bytes()); // type = RAM
        assert_eq!(&page[0x14..0x18], &0u32.to_le_bytes()); // reserved

        let mut page = [0u8; 4096];
        assert!(pvh::write_cmdline(&mut page, "console=hvc0"));
        assert_eq!(&page[..13], b"console=hvc0\0");
        assert!(!pvh::write_cmdline(&mut page, &"x".repeat(4096)));
    }

    /// The independent oracle: the blobs the writer builds, read back by the
    /// parser the kernel itself boots with on QEMU and Firecracker
    /// (`akuma-ryzen-amd64`). A writer and a test that share one misreading of
    /// the layout agree with each other and with nothing else — the PVH phase
    /// passed for exactly that reason while Linux saw no command line.
    #[test]
    fn pvh_blobs_parse_with_the_kernels_own_start_info_reader() {
        use akuma_ryzen_amd64::{PhysMem, StartInfo};

        struct Guest {
            pages: [(u64, [u8; 4096]); 3],
        }
        impl PhysMem for Guest {
            fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
                for (base, bytes) in &self.pages {
                    if pa >= *base && pa + buf.len() as u64 <= base + 4096 {
                        let at = (pa - base) as usize;
                        buf.copy_from_slice(&bytes[at..at + buf.len()]);
                        return true;
                    }
                }
                false
            }
        }

        let l = pvh::plan(0x0800_0000, 0, 0x0800_0000 - 0x3000);
        let mut g = Guest {
            pages: [(l.start_gpa, [0; 4096]), (l.memmap_gpa, [0; 4096]), (l.cmdline_gpa, [0; 4096])],
        };
        pvh::write_start(&mut g.pages[0].1, &l);
        pvh::write_memmap(&mut g.pages[1].1, &l);
        assert!(pvh::write_cmdline(&mut g.pages[2].1, "console=ttyS0 earlyprintk=serial,ttyS0"));

        let si = StartInfo::parse(&g, l.start_gpa).expect("the kernel's parser rejects our start_info");
        assert_eq!(si.version, 1);
        assert_eq!(si.nr_modules, 0);
        assert_eq!(si.cmdline_paddr, l.cmdline_gpa);
        assert_eq!(si.rsdp_paddr, 0);
        assert_eq!(si.memmap_paddr, l.memmap_gpa);
        assert_eq!(si.memmap_entries, 1);
        let e = si.memmap_entry(&g, 0).expect("memmap entry 0");
        assert_eq!((e.addr, e.size, e.kind), (0, 0x0800_0000 - 0x3000, 1));
        assert!(e.is_ram());
        let mut buf = [0u8; 128];
        assert_eq!(si.cmdline(&g, &mut buf), Some("console=ttyS0 earlyprintk=serial,ttyS0"));
    }

    /// The ACPI page, read back with the walker the host kernel uses on real
    /// firmware tables (`akuma-ryzen-amd64`): RSDP bytes -> XSDT -> FADT -> DSDT,
    /// and the start_info + memory map that announce it.
    #[test]
    fn acpi_tables_are_found_by_the_kernels_own_table_walker() {
        use akuma_ryzen_amd64::acpi::{dsdt, find_table, rsdp_from_bytes};
        use akuma_ryzen_amd64::{PhysMem, StartInfo};

        const TOP: u64 = 0x0800_0000;
        struct Guest {
            pages: [(u64, [u8; 4096]); 4],
        }
        impl PhysMem for Guest {
            fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
                self.pages.iter().any(|(base, bytes)| {
                    let ok = pa >= *base && pa + buf.len() as u64 <= base + 4096;
                    if ok {
                        let at = (pa - base) as usize;
                        buf.copy_from_slice(&bytes[at..at + buf.len()]);
                    }
                    ok
                })
            }
        }
        let l = pvh::plan(TOP, 0, TOP - 0x4000);
        let acpi_gpa = TOP - 0x4000;
        let mut g = Guest {
            pages: [(l.start_gpa, [0; 4096]), (l.memmap_gpa, [0; 4096]), (l.cmdline_gpa, [0; 4096]), (acpi_gpa, [0; 4096])],
        };
        let dev = acpi::VirtioMmio { base: 0xD000_0000, len: 0x1000, irq: 5 };
        let rsdp_gpa = acpi::build(&mut g.pages[3].1, acpi_gpa, &dev).unwrap();
        pvh::write_start(&mut g.pages[0].1, &l);
        pvh::write_memmap(&mut g.pages[1].1, &l);
        let (a, b) = g.pages.split_at_mut(1);
        pvh::add_acpi(&mut a[0].1, &mut b[0].1, rsdp_gpa, acpi_gpa, 0x1000);

        let si = StartInfo::parse(&g, l.start_gpa).expect("start_info");
        assert_eq!(si.rsdp_paddr, rsdp_gpa);
        assert_eq!(si.memmap_entries, 2);
        let ram = si.memmap_entry(&g, 0).unwrap();
        let tab = si.memmap_entry(&g, 1).unwrap();
        assert!(ram.is_ram() && !tab.is_ram());
        assert_eq!((tab.addr, tab.size, tab.kind), (acpi_gpa, 0x1000, 3));
        assert!(ram.addr + ram.size <= tab.addr, "RAM stops where the tables start");

        // The table walker, starting from the RSDP the start_info names.
        let mut rsdp_bytes = [0u8; 36];
        assert!(g.read(si.rsdp_paddr, &mut rsdp_bytes));
        let rsdp = rsdp_from_bytes(&rsdp_bytes).expect("the walker's RSDP checks reject it");
        assert_eq!(rsdp.revision, 2);
        assert_ne!(rsdp.xsdt, 0, "extended checksum accepted");
        let fadt = find_table(&g, &rsdp, b"FACP").expect("FADT through the XSDT");
        assert_eq!(fadt.length, 276);
        let d = dsdt(&g, &rsdp).expect("DSDT through the FADT");
        assert_eq!(&d.signature, b"DSDT");
        assert_eq!(d.addr, acpi_gpa + acpi::DSDT_OFF as u64);
    }
}
