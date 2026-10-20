//! SVM hypervisor seed — M1 rung 0 (`docs/archive/AKUMA_KVM.md`).
//!
//! A CPUID probe that prints one `[KVM]` line on every boot, plus — behind the
//! `kvmtest` cmdline flag, the `hdatest` precedent — a trivial guest: two
//! `vmmcall`s and a `hlt` under nested paging. The data side (VMCB vocabulary,
//! §15.5.1 guest-state validation, NPT encodings) lives in
//! `akuma-svm-amd64` and is host-tested; this module is only the wiring and
//! the three `unsafe` sites the CPU actually requires (two MSR writes and the
//! `vmrun` itself).
//!
//! Spec-first: no Linux/KVM code was consulted (GPL); every constant traces to
//! the APM citation recorded in the crate or the milestone doc.
//!
//! Rung-0 shape, each choice deliberate (see the doc's §3/§4): the enable
//! is per-core MSR state, so the BSP enables at probe time and every AP
//! follows in `ap_entry64` ([`ap_hook`], behind `kvmtest` — rung 2 owns
//! the default), each against a host save area reserved up front and
//! leaked, so no AP allocates during bring-up; the MSRPM/IOPM are `.bss`
//! statics because the two things §15.11/§15.10.1 demand of them — 4-KiB
//! alignment and **physical contiguity** — are what the linker guarantees
//! of an image object (`PA = VA − KERNEL_VMA`, the `linker.ld` `AT()`
//! derivation) and what two separately allocated frames are not; NMI
//! **is** intercepted, unlike the plan's first cut — a guest run with a
//! null guest IDTR would turn one stray NMI into a triple fault, and
//! losing an NMI out of a microsecond test window costs nothing.
//!
//! **The shared-GPR contract** (`vmrun`, Table B-2): VMRUN saves only
//! RSP, RAX, RIP and RFLAGS of the host — every other GP register is
//! *shared with the guest*, and #VMEXIT leaves the guest's last value in
//! the host's register file. The asm therefore clobbers all of them,
//! round-trips RCX and RDX (the two the seed guest exercises) through the
//! exit loop, and treats the rest as host-garbage at the next entry — a
//! real VMM keeps its own register file and feeds them all back (rung 2).
//! The first guest that writes RCX before an intercepted instruction
//! would otherwise corrupt the host in flight.

use crate::phys::FreshFrame;
use crate::serial;
use crate::smp;
use akuma_cpu::msr::{self, Msr};
use akuma_svm_amd64 as svm;
use akuma_svm_amd64::{ctrl, intr0, intr1, save, ExitCode, Seg, SegAttr, Vmcb};

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Console helpers (hda.rs precedent: `puts` straight to the port, no alloc)
// ---------------------------------------------------------------------------

fn p(s: &str) {
    serial::puts(s);
}

/// Breadcrumb for a run that resets the machine before any log can be read: the
/// last step reached, in CMOS byte 0x7F (port 0x70/0x71 from Linux's /dev/port).
/// CMOS survives the reset; serial and the console ring do not.
fn crumb(step: u8) {
    akuma_port::CMOS.write(0x7F, step);
}

fn hx(v: u64, nibbles: u32) {
    serial::put_hexn(v, nibbles);
}

fn dec(v: u64) {
    serial::put_dec(v);
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Probe for SVM and, with `kvmtest` on the command line, run the trivial
/// guest. Safe to call on any x86_64 part: the only MSR touched (`VM_CR`) is
/// read strictly after CPUID advertises SVM — an rdmsr of an unimplemented
/// MSR `#GP`s.
pub fn init(cmdline: &str) {
    let want_test = cmdline.split_ascii_whitespace().any(|t| t == "kvmtest");

    // Extended CPUID leaf range, then the SVM feature bit: Fn8000_0001.ECX[2]
    // (APM Vol 3 §3.1/§15.4).
    if __cpuid(0x8000_0000).eax < 0x8000_000A {
        p("[KVM] no extended CPUID: SVM absent\n");
        return;
    }
    if __cpuid(0x8000_0001).ecx & (1 << 2) == 0 {
        p("[KVM] CPUID Fn8000_0001.ECX[2] clear: SVM absent\n");
        return;
    }

    // SVM present: `VM_CR.SVMDIS` (bit 4) set means disabled, and without
    // EDX[SVML] the lock is firmware-owned (APM Vol 2 §15.11, §15.30.4).
    let vm_cr = msr::read(Msr::VmCr);
    if vm_cr & svm::msr::VM_CR_SVMDIS != 0 {
        p("[KVM] VM_CR.SVMDIS set: SVM firmware-locked\n");
        return;
    }

    // Fn8000_000A (Vol 3 §E.4.9): EAX revision, EBX ASID count, EDX feature
    // bits — NP=0 and NRIPS=3 are the two rung 0 branches on.
    let fa = __cpuid(0x8000_000A);
    p("[KVM] Fn8000_000A: rev=");
    hx(u64::from(fa.eax), 8);
    p(" asids=");
    dec(u64::from(fa.ebx));
    p(" edx=");
    hx(u64::from(fa.edx), 8);
    p("\n");
    if fa.edx & 1 == 0 {
        p("[KVM] EDX[NP] clear: no nested paging, rung 0 needs it\n");
        return;
    }
    let nrips = fa.edx & (1 << 3) != 0;

    p("[KVM] SVM available; EFER.SVME not yet enabled (per-core, rung 2)\n");
    if want_test {
        if !reserve_hsaves() {
            p("[KVM] kvmtest: no frame for the BSP host save area\n");
            return;
        }
        SVM_WANTED.store(true, Ordering::Release);
        trivial_guest(nrips);
    }
}

// ---------------------------------------------------------------------------
// Per-core enable + the permission maps
// ---------------------------------------------------------------------------

/// Set once the probe passed and `kvmtest` asked for the rehearsal: every
/// core that later reaches [`ap_hook`] enables SVM too.
static SVM_WANTED: AtomicBool = AtomicBool::new(false);

/// Per-core host save area physical addresses (§15.30.4: one live,
/// 4-KiB-aligned page per core, distinct from every VMCB). Reserved by the
/// BSP at probe time and deliberately leaked — `VM_HSAVE_PA` holds each
/// for the life of the kernel.
static HSAVE_PA: [AtomicU64; smp::MAX_CPUS] = [const { AtomicU64::new(0) }; smp::MAX_CPUS];

/// Which cores have `EFER.SVME` set.
static SVM_ENABLED: [AtomicBool; smp::MAX_CPUS] =
    [const { AtomicBool::new(false) }; smp::MAX_CPUS];

/// Reserve every core's host save area up front, so no AP allocates during
/// bring-up. Returns false if the BSP's own frame could not be had.
fn reserve_hsaves() -> bool {
    let mut bsp = false;
    for (idx, slot) in HSAVE_PA.iter().enumerate() {
        if slot.load(Ordering::Relaxed) != 0 {
            bsp |= idx == 0;
            continue;
        }
        let Some(f) = FreshFrame::alloc_zeroed() else {
            if idx == 0 {
                return false;
            }
            continue;
        };
        slot.store(f.publish() as u64, Ordering::Relaxed);
        bsp |= idx == 0;
    }
    bsp
}

/// Point this core at its host save area and set `EFER.SVME` (§15.4) —
/// both are per-core MSR state. Idempotent; false if the core has no
/// reserved save area.
fn enable_on_core(idx: usize) -> bool {
    let Some(enabled) = SVM_ENABLED.get(idx) else {
        return false;
    };
    if enabled.load(Ordering::Relaxed) {
        return true;
    }
    let hsave = HSAVE_PA[idx].load(Ordering::Relaxed);
    if hsave == 0 {
        return false;
    }
    // SAFETY: `VM_HSAVE_PA` must hold a live 4-KiB-aligned page distinct
    // from every VMCB (§15.30.4): this is the frame `reserve_hsaves`
    // zeroed and leaked, which nothing else maps or frees.
    unsafe { msr::write(Msr::VmHsavePa, hsave) };
    // SAFETY: `EFER.SVME` enables `VMRUN` on this core (§15.4). The
    // read-modify-write preserves what paging and syscall init already set
    // (LME, LMA, NXE, SCE).
    let efer = msr::read(Msr::Efer);
    unsafe { msr::write(Msr::Efer, efer | svm::msr::EFER_SVME) };
    enabled.store(true, Ordering::Release);
    true
}

/// Called by every AP from `ap_entry64`, after its per-cpu setup: when the
/// BSP's probe wanted SVM, enable it here too. One line per core, so the
/// boot log names exactly who is hypervisor-capable.
pub fn ap_hook() {
    if !SVM_WANTED.load(Ordering::Acquire) {
        return;
    }
    let idx = smp::cpu_index();
    p("  [KVM] cpu ");
    dec(idx as u64);
    if enable_on_core(idx) {
        p(" svm enabled\n");
    } else {
        p(" svm NOT enabled (no host save area)\n");
    }
}

/// The MSR permissions map (§15.11): 8 KiB in **two contiguous physical
/// pages** — the property two separately allocated frames do not have, so
/// the map lives in `.bss`, whose image bytes `linker.ld` places at
/// `VA − KERNEL_VMA`. All zero = nothing intercepted; the one armed bit
/// (EFER read) is set at compile time, which is also what keeps a `static`
/// free of interior mutability.
#[repr(C, align(4096))]
struct Msrpm([u8; svm::msrpm::SIZE]);
static MSRPM: Msrpm = Msrpm(svm::msrpm::with_access(
    [0; svm::msrpm::SIZE],
    svm::msr::EFER,
    true,
    false,
));

/// The I/O permissions map (§15.10.1): 12 contiguous KiB, same `.bss`
/// argument, one bit armed: port `0x80` (POST).
#[repr(C, align(4096))]
struct Iopm([u8; svm::iopm::SIZE]);
static IOPM: Iopm = Iopm(svm::iopm::with_port([0; svm::iopm::SIZE], 0x80, true));

/// The physical address of an object in the kernel image — the derivation
/// `linker.ld` bakes into every section (`AT(ADDR(...) - KERNEL_VMA)`) and
/// `multiboot2.rs`'s PAT init already makes.
fn image_pa<T>(r: &T) -> u64 {
    core::ptr::from_ref::<T>(r) as u64 - crate::phys::KERNEL_VMA
}

// ---------------------------------------------------------------------------
// The trivial guest
// ---------------------------------------------------------------------------

/// `rdmsr(EFER)`, `vmmcall`, `out 0x80, al`, `vmmcall`, `hlt`, spin. The
/// `rdmsr` and the `out` are the first two things the permission maps
/// name — each exit is a map being consulted — and the `vmmcall`s are the
/// checkpoints between them. The trailing `jmp $` would wedge the guest if
/// it were ever reached, which is exactly what makes it a canary.
const GUEST_CODE: [u8; 18] = [
    0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, 0xC000_0080 (EFER)
    0x0F, 0x32, // rdmsr — intercepted by the MSRPM read bit (exit 7Ch)
    0x0F, 0x01, 0xD9, // vmmcall — checkpoint one: RAX = EFER[31:0]
    0xE6, 0x80, // out 0x80, al — intercepted by the IOPM bit (exit 7Bh)
    0x0F, 0x01, 0xD9, // vmmcall — checkpoint two
    0xF4, // hlt — the stop marker
    0xEB, 0xFE, // jmp $
];

/// The seed: build a VMCB and a one-page NPT identity map, run the guest,
/// expect two `VMMCALL` exits and one `HLT`. Every frame stays a held
/// `FreshFrame` for the whole run (dropped only at return, freeing to the
/// PMM) — nothing is published to the physmap, so no mapping code runs.
fn trivial_guest(nrips: bool) {
    crumb(1);
    let Some(mut vmcb_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for VMCB\n");
        return;
    };
    let Some(mut pml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for NPT PML4\n");
        return;
    };
    let Some(mut pdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for NPT PDPT\n");
        return;
    };
    let Some(mut pd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for NPT PD\n");
        return;
    };
    let Some(mut code_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for guest code\n");
        return;
    };

    crumb(2);
    let code_pa = code_f.pa() as u64;
    code_f.bytes_mut()[..GUEST_CODE.len()].copy_from_slice(&GUEST_CODE);

    // The permission maps: 4-KiB aligned and physically contiguous by
    // construction (`.bss` statics — see their docs); the alignment is the
    // one part of that the hardware enforces, so it is checked, not trusted.
    let (msrpm_pa, iopm_pa) = (image_pa(&MSRPM.0), image_pa(&IOPM.0));
    if msrpm_pa % 4096 != 0 || iopm_pa % 4096 != 0 {
        p("[KVM] permission map base not 4-KiB aligned\n");
        return;
    }

    // Nested paging, minimal: identity-map exactly the 2 MiB page holding the
    // guest code. Guest CR0.PG=0 makes guest-linear == guest-physical, so the
    // NPT walk translates `code_pa` directly. The VMCB/hsave/NPT pages are
    // host-physical and never NPT-walked — no mapping for them.
    let pml4_pa = pml4_f.pa() as u64;
    let pdpt_pa = pdpt_f.pa() as u64;
    let pd_pa = pd_f.pa() as u64;
    svm::wr64(pml4_f.bytes_mut(), 0, svm::npt::pointer(pdpt_pa));
    svm::wr64(pdpt_f.bytes_mut(), 0, svm::npt::pointer(pd_pa));
    let pd_index = ((code_pa >> 21) & 0x1FF) as usize;
    svm::wr64(pd_f.bytes_mut(), pd_index * 8, svm::npt::leaf_2m(code_pa & !0x1F_FFFF));

    // VMCB. Unused bytes must stay zero (page note, Table B-1): alloc_zeroed
    // guarantees it, and the wrapper only writes the fields below.
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR6, 0xFFFF_0FF0);
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR7, 0x400);
    {
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        // Intercept what the seed understands: the four exceptions a bad guest
        // state raises, the external events that would hit a null guest IDTR,
        // the three instructions the protocol uses, and the two permission
        // maps (MSR_PROT consults the MSRPM, IOIO_PROT the IOPM).
        v.set_intr0(
            intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT | intr0::MSR_PROT | intr0::IOIO_PROT,
        );
        v.set_intr1(intr1::VMRUN | intr1::VMMCALL);
        v.set_excp(svm::excp::DB | svm::excp::UD | svm::excp::GP | svm::excp::PF);
        v.set_iopm_base(iopm_pa);
        v.set_msrpm_base(msrpm_pa);
        v.set_asid(1);
        v.set_tlb_control(ctrl::TLB_DO_NOTHING);
        v.set_np_enable(true);
        v.set_n_cr3(pml4_pa);
        // Flat segments: base 0, limit 0xFFFF_FFFF. The VMCB limit field is the
        // byte-scaled 32-bit limit, not the 20-bit pre-granularity form: 0xFFFFF
        // here is a 1 MiB limit, and the guest code sits above it (#GP on fetch).
        let flat = |attr: SegAttr| Seg { selector: 0, attr, limit: 0xFFFF_FFFF, base: 0 };
        v.set_seg(save::CS, flat(SegAttr::CODE32_DPL0));
        let data = flat(SegAttr::DATA32_RW_DPL0);
        v.set_seg(save::ES, data);
        v.set_seg(save::SS, data);
        v.set_seg(save::DS, data);
        // FS/GS carry the host bases so the post-exit kernel needs no
        // VMLOAD/VMSAVE — VMRUN loads the guest's, #VMEXIT does not restore
        // the host's (that is what VMLOAD/VMSAVE are for, §15.5.3/§15.5.4).
        v.set_seg(
            save::FS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::FsBase) },
        );
        v.set_seg(
            save::GS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::GsBase) },
        );
        // 32-bit protected, unpaged: PG=0 keeps guest-linear == guest-physical
        // for the NPT; LME/LMA stay clear so no PAE/long-mode checks apply.
        v.set_efer(svm::msr::EFER_SVME);
        v.set_cr0(svm::cr0::PE | svm::cr0::NE);
        v.set_cr3(0);
        v.set_cr4(0);
        v.set_rflags(0x2);
        v.set_cpl(0);
        v.set_rip(code_pa);
        v.set_rsp(0);
    }

    crumb(3);
    // The §15.5.1 consistency checks, host-implemented: VMEXIT_INVALID gives
    // no diagnostics, so a failing boot names its own cause here.
    {
        let v = Vmcb::new(vmcb_f.bytes_mut());
        if let Err(e) = svm::validate_guest_state(&v) {
            p("[KVM] guest state rejected: ");
            p(e.name());
            p("\n");
            return;
        }
    }

    crumb(4);
    if !enable_on_core(0) {
        p("[KVM] no host save area for the BSP\n");
        return;
    }

    crumb(5);
    p("[KVM] running trivial guest (nrips=");
    p(if nrips { "yes" } else { "no" });
    p(", code_pa=");
    hx(code_pa, 16);
    p(")\n");

    // The run. IRQs stay masked for the whole excursion — the guest is a
    // dozen known bytes, and the host save area restores everything VMRUN
    // took. RCX/RDX round-trip through the loop: VMRUN does not save them
    // (Table B-2), so the guest's last value is the only copy there is.
    akuma_cpu::daif::mask_irq();
    let mut vmmcalls = 0u64;
    let mut msr_exits = 0u64;
    let mut ioio_exits = 0u64;
    let (mut guest_rcx, mut guest_rdx) = (0u64, 0u64);
    let (mut halted, mut why) = (false, "loop exhausted");
    for _ in 0..8 {
        crumb(7);
        let (rcx_out, rdx_out) = unsafe { vmrun(vmcb_f.pa() as u64, guest_rcx, guest_rdx) };
        guest_rcx = rcx_out;
        guest_rdx = rdx_out;
        crumb(8);
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Msr => {
                msr_exits += 1;
                if guest_rcx != u64::from(svm::msr::EFER) {
                    why = "unexpected MSR access";
                    p("[KVM] unexpected MSR access: ");
                    dump_exit(&v, guest_rcx, guest_rdx);
                    break;
                }
                // RDMSR of the guest's EFER (save area 0D0h — Table B-2):
                // the value is EDX:EAX. RAX is in the save area, so the
                // next VMRUN loads it; RDX is not in the save area at all,
                // so the high half goes to the next `vmrun` as the RDX the
                // guest starts with.
                let efer = v.efer();
                v.set_rax(efer & 0xFFFF_FFFF);
                guest_rdx = efer >> 32;
                v.set_rip(next_rip(&v, nrips, 2));
            }
            ExitCode::Ioio => {
                ioio_exits += 1;
                // OUT 0x80, al: the POST port. Nothing to emulate — the
                // exit is the IOPM bit being consulted.
                v.set_rip(next_rip(&v, nrips, 2));
            }
            ExitCode::Vmmcall => {
                vmmcalls += 1;
                if vmmcalls == 1 && v.rax() != v.efer() & 0xFFFF_FFFF {
                    why = "checkpoint one: RAX != EFER[31:0] after RDMSR";
                    p("[KVM] ");
                    p(why);
                    p(": ");
                    dump_exit(&v, guest_rcx, guest_rdx);
                    break;
                }
                v.set_rip(next_rip(&v, nrips, 3));
            }
            ExitCode::Hlt => {
                halted = true;
                break;
            }
            _ => {
                why = "unexpected exit";
                p("[KVM] unexpected exit: ");
                dump_exit(&v, guest_rcx, guest_rdx);
                break;
            }
        }
    }
    akuma_cpu::daif::unmask_irq();
    crumb(9);

    let final_v = Vmcb::new(vmcb_f.bytes_mut());
    let pass = halted
        && vmmcalls == 2
        && msr_exits == 1
        && ioio_exits == 1
        && final_v.rax() == svm::msr::EFER_SVME;
    p("[KVM] trivial guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (vmmcalls=");
    dec(vmmcalls);
    p(", msr=");
    dec(msr_exits);
    p(", ioio=");
    dec(ioio_exits);
    p(", rax=");
    hx(final_v.rax(), 16);
    p(")\n");
    if !pass {
        p("[KVM] why: ");
        p(why);
        p("\n");
    }
}

/// The resume RIP after a trap-style exit: nRIP when the NRIPS feature is
/// present (§15.28.x), else the instruction length by hand.
fn next_rip(v: &Vmcb<'_>, nrips: bool, fallback: u64) -> u64 {
    if nrips && v.nrip() != 0 {
        v.nrip()
    } else {
        v.rip() + fallback
    }
}

/// One serial line of post-mortem for an unexpected exit: code, infos, RIPs
/// and the captured instruction bytes (stored last-byte-first, so print in
/// reverse to show program order). RCX/RDX are the shared registers, not
/// save-area state — passed in from the exit loop's round-trip.
fn dump_exit(v: &Vmcb<'_>, rcx: u64, rdx: u64) {
    p("code=");
    hx(v.exitcode(), 16);
    p(" info1=");
    hx(v.exitinfo1(), 16);
    p(" info2=");
    hx(v.exitinfo2(), 16);
    p(" rip=");
    hx(v.rip(), 16);
    p(" nrip=");
    hx(v.nrip(), 16);
    p(" rcx=");
    hx(rcx, 16);
    p(" rdx=");
    hx(rdx, 16);
    p(" instr=");
    let (count, bytes) = v.guest_instr();
    let count = count.min(15);
    let mut i = count;
    while i > 0 {
        i -= 1;
        hx(u64::from(bytes[i]), 2);
        p(" ");
    }
    p("\n");
}

/// `VMRUN` (§15.4): RAX carries the VMCB physical address. RSP, RAX, RIP
/// and RFLAGS are the host's own again on #VMEXIT (the host save area);
/// every other GP register is shared with the guest, so the asm declares
/// them all clobbered, feeds RCX/RDX in as the guest's starting values,
/// and returns what the guest left in them.
///
/// # Safety
/// `vmcb_pa` must be the 4 KiB-aligned physical address of a fully
/// initialized VMCB page held alive by the caller, and the core must have
/// `VM_HSAVE_PA` pointed at a live host save area with `EFER.SVME` set.
/// Returns with GIF set (`stgi`).
///
/// RBX and RBP are not declared: they cannot be inline-asm operands (LLVM
/// reserves RBX; RBP is the frame pointer). The seed guest never writes
/// them; rung 2 (the Linux rung) needs a real register file to cover it.
#[inline(never)]
unsafe fn vmrun(vmcb_pa: u64, guest_rcx: u64, guest_rdx: u64) -> (u64, u64) {
    let (rcx, rdx);
    // SAFETY: per the contract above — VMRUN with RAX = VMCB PA; the host
    // state it needs (EFER.SVME, VM_HSAVE_PA) is installed by the caller.
    unsafe {
        // `stgi` straight after the exit: #VMEXIT leaves GIF clear, and with GIF
        // clear no interrupt, NMI or SMI reaches the host — the timer goes
        // silent and the watchdog resets the box (metal, 2026-10-21; the CMOS
        // breadcrumb read 9 = the guest had finished).
        core::arch::asm!(
            "vmrun rax",
            "stgi",
            inlateout("rax") vmcb_pa => _,
            inlateout("rcx") guest_rcx => rcx,
            inlateout("rdx") guest_rdx => rdx,
            lateout("rsi") _,
            lateout("rdi") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            lateout("r12") _,
            lateout("r13") _,
            lateout("r14") _,
            lateout("r15") _,
            options(nostack)
        );
    }
    (rcx, rdx)
}
