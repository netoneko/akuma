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
//! Rung-0 simplifications, each deliberate (see the doc's §3/§4): BSP only —
//! `EFER.SVME` and `VM_HSAVE_PA` are per-core and the APs are never enabled;
//! no MSRPM/IOPM (the guest is ten known bytes; adding the 8 KiB maps is
//! rung 2); NMI **is** intercepted, unlike the plan's first cut — a guest run
//! with a null guest IDTR would turn one stray NMI into a triple fault, and
//! losing an NMI out of a microsecond test window costs nothing.

use crate::phys::FreshFrame;
use crate::serial;
use akuma_cpu::msr::{self, Msr};
use akuma_svm_amd64 as svm;
use akuma_svm_amd64::{ctrl, intr0, intr1, save, ExitCode, Seg, SegAttr, Vmcb};

use core::arch::x86_64::__cpuid;

// ---------------------------------------------------------------------------
// Console helpers (hda.rs precedent: `puts` straight to the port, no alloc)
// ---------------------------------------------------------------------------

fn p(s: &str) {
    serial::puts(s);
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
        trivial_guest(nrips);
    }
}

// ---------------------------------------------------------------------------
// The trivial guest
// ---------------------------------------------------------------------------

/// Two `vmmcall`s, then `hlt`, then spin. `VMMCALL` is the hypervisor's own
/// "hello" — an intercept we asked for, at a known length; `HLT` is the stop
/// marker. The trailing `jmp $` would wedge the guest if it were ever
/// reached, which is exactly what makes it a canary.
const GUEST_CODE: [u8; 9] = [
    0x0F, 0x01, 0xD9, // vmmcall (an F3 prefix would make it VMGEXIT, which #GPs outside SEV-ES)
    0x0F, 0x01, 0xD9, // vmmcall
    0xF4, // hlt
    0xEB, 0xFE, // jmp $
];

/// The seed: build a VMCB and a one-page NPT identity map, run the guest,
/// expect two `VMMCALL` exits and one `HLT`. Every frame stays a held
/// `FreshFrame` for the whole run (dropped only at return, freeing to the
/// PMM) — nothing is published to the physmap, so no mapping code runs.
fn trivial_guest(nrips: bool) {
    let Some(mut vmcb_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for VMCB\n");
        return;
    };
    let Some(hsave_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] no frame for host save area\n");
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

    let code_pa = code_f.pa() as u64;
    code_f.bytes_mut()[..GUEST_CODE.len()].copy_from_slice(&GUEST_CODE);

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
        // and the three instructions the protocol uses. No MSR/IOIO (rung 2).
        v.set_intr0(intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT);
        v.set_intr1(intr1::VMRUN | intr1::VMMCALL);
        v.set_excp(svm::excp::DB | svm::excp::UD | svm::excp::GP | svm::excp::PF);
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

    // Enable SVM on this core (BSP): the host save area, then EFER.SVME.
    // Both are one-time for rung 0; AP cores stay non-hypervisor.
    unsafe { msr::write(Msr::VmHsavePa, hsave_f.pa() as u64) };
    let efer = msr::read(Msr::Efer);
    unsafe { msr::write(Msr::Efer, efer | svm::msr::EFER_SVME) };

    p("[KVM] running trivial guest (nrips=");
    p(if nrips { "yes" } else { "no" });
    p(", code_pa=");
    hx(code_pa, 16);
    p(")\n");

    // The run. IRQs stay masked for the whole excursion — the guest is ten
    // bytes long, and the host save area restores everything VMRUN took.
    akuma_cpu::daif::mask_irq();
    let mut vmmcalls = 0u64;
    let mut outcome = &"guest never halted (loop exhausted)";
    for _ in 0..8 {
        unsafe { vmrun(vmcb_f.pa() as u64) };
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Vmmcall => {
                vmmcalls += 1;
                // Trap-style exit: nRIP is the resume point when NRIPS is
                // present; otherwise the vmmcall length (3) by hand.
                let next = if nrips && v.nrip() != 0 { v.nrip() } else { v.rip() + 3 };
                v.set_rip(next);
            }
            ExitCode::Hlt => {
                outcome = &"PASS";
                break;
            }
            _ => {
                outcome = &"FAIL";
                p("[KVM] unexpected exit: ");
                dump_exit(&v);
                break;
            }
        }
    }
    akuma_cpu::daif::unmask_irq();

    p("[KVM] trivial guest: ");
    p(outcome);
    p(" (vmmcalls=");
    dec(vmmcalls);
    p(", final rip=");
    hx(Vmcb::new(vmcb_f.bytes_mut()).rip(), 16);
    p(")\n");
}

/// One serial line of post-mortem for an unexpected exit: code, infos, RIPs
/// and the captured instruction bytes (stored last-byte-first, so print in
/// reverse to show program order).
fn dump_exit(v: &Vmcb<'_>) {
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

/// `VMRUN` (§15.4): RAX carries the VMCB physical address. Every other host
/// register observable after the exit equals its value before it — the host
/// save area received the host state at entry and #VMEXIT restores it — so
/// `rax` is the only operand the asm needs to describe.
///
/// # Safety
/// `vmcb_pa` must be the 4 KiB-aligned physical address of a fully
/// initialized VMCB page held alive by the caller, and the core must have
/// `VM_HSAVE_PA` pointed at a live host save area with `EFER.SVME` set.
#[inline(never)]
unsafe fn vmrun(vmcb_pa: u64) {
    // SAFETY: per the contract above — VMRUN with RAX = VMCB PA; the host
    // state it needs (EFER.SVME, VM_HSAVE_PA) is installed by the caller.
    unsafe {
        core::arch::asm!("vmrun rax", inlateout("rax") vmcb_pa => _, options(nostack));
    }
}
