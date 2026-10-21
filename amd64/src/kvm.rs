//! SVM hypervisor seed — M1 rung 0 (`docs/archive/AKUMA_KVM.md`).
//!
//! A CPUID probe that prints one `[KVM]` line on every boot, plus — behind the
//! `kvmtest` cmdline flag, the `hdatest` precedent — a trivial guest: two
//! `vmmcall`s and a `hlt` under nested paging, a **long-mode** guest: a
//! paged 64-bit guest whose own page tables are walked *under* the nested
//! ones, and a **two-level** guest: both translations non-identity — the
//! guest page tables map a synthetic VA to a synthetic guest-PA the NPT
//! then relocates onto wherever the PMM put a frame — the shape a real
//! loader stands on. The data side (VMCB vocabulary,
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

/// Probe for SVM and, with `kvmtest` on the command line, run the rehearsal
/// guests (trivial, then long-mode). Safe to call on any x86_64 part: the
/// only MSR touched (`VM_CR`) is
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
        longmode_guest(nrips);
        twolvl_guest(nrips);
        consistency_ladder();
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
                dump_state(&v);
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

// ---------------------------------------------------------------------------
// The long-mode guest
// ---------------------------------------------------------------------------

/// The checkpoint-one magic, carried by the prologue's one `movabs`: a
/// 32-bit decode of the same bytes loads only the low half (`mov eax, imm32`
/// zeroes the high half), so matching the full 64 bits *is* the long-mode
/// proof — plus the `rdmsr` round trip, checkpoint two.
const LONGMODE_MAGIC: u64 = 0x1234_5678_9ABC_DEF0;

/// `movabs rax, magic`, `vmmcall` (checkpoint one), `rdmsr(EFER)` —
/// intercepted by the MSRPM read bit — `vmmcall` (checkpoint two: RAX =
/// EFER[31:0]), `hlt`, spin. Same canary shape as the trivial guest: the
/// trailing `jmp $` wedges the guest if it is ever reached.
const LONGMODE_CODE: [u8; 26] = [
    0x48, 0xB8, 0xF0, 0xDE, 0xBC, 0x9A, 0x78, 0x56, 0x34, 0x12, // movabs rax, LONGMODE_MAGIC
    0x0F, 0x01, 0xD9, // vmmcall — checkpoint one: RAX = the magic
    0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, 0xC000_0080 (EFER)
    0x0F, 0x32, // rdmsr — intercepted by the MSRPM read bit (exit 7Ch)
    0x0F, 0x01, 0xD9, // vmmcall — checkpoint two: RAX = EFER[31:0]
    0xF4, // hlt — the stop marker
    0xEB, 0xFE, // jmp $
];

/// The long-mode rung: the trivial guest's rehearsal with the guest paged
/// and 64-bit. The guest walks its own three-level identity table (the
/// PTE encoding is the same x86-64 one `npt` names, so its builders make
/// guest entries too) *under* the nested tables — proving the two-level
/// translation, guest VA → guest PA → host PA, that the Linux rung stands
/// on. Breadcrumbs 0x11–0x19 mirror the trivial guest's 1–9, so a reset
/// names which guest died.
fn longmode_guest(nrips: bool) {
    crumb(0x11);
    let Some(mut vmcb_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for VMCB\n");
        return;
    };
    let Some(mut npml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for NPT PML4\n");
        return;
    };
    let Some(mut npdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for NPT PDPT\n");
        return;
    };
    let Some(mut npd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for NPT PD\n");
        return;
    };
    let Some(mut gpml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for guest PML4\n");
        return;
    };
    let Some(mut gpdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for guest PDPT\n");
        return;
    };
    let Some(mut gpd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for guest PD\n");
        return;
    };
    let Some(mut code_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] long-mode: no frame for guest code\n");
        return;
    };

    crumb(0x12);
    let code_pa = code_f.pa() as u64;
    code_f.bytes_mut()[..LONGMODE_CODE.len()].copy_from_slice(&LONGMODE_CODE);

    let (msrpm_pa, iopm_pa) = (image_pa(&MSRPM.0), image_pa(&IOPM.0));
    if msrpm_pa % 4096 != 0 || iopm_pa % 4096 != 0 {
        p("[KVM] permission map base not 4-KiB aligned\n");
        return;
    }

    // Nested paging, identity for every guest-physical address the run
    // touches: the code *and* the three guest page-table pages, which the
    // guest paging walk itself reads as guest-physical memory. Two of the
    // four PAs can share a 2 MiB page; writing that entry twice is fine.
    let npml4_pa = npml4_f.pa() as u64;
    let npdpt_pa = npdpt_f.pa() as u64;
    let npd_pa = npd_f.pa() as u64;
    let gpml4_pa = gpml4_f.pa() as u64;
    let gpdpt_pa = gpdpt_f.pa() as u64;
    let gpd_pa = gpd_f.pa() as u64;
    svm::wr64(npml4_f.bytes_mut(), 0, svm::npt::pointer(npdpt_pa));
    svm::wr64(npdpt_f.bytes_mut(), 0, svm::npt::pointer(npd_pa));
    for pa in [code_pa, gpml4_pa, gpdpt_pa, gpd_pa] {
        let idx = ((pa >> 21) & 0x1FF) as usize;
        svm::wr64(npd_f.bytes_mut(), idx * 8, svm::npt::leaf_2m(pa & !0x1F_FFFF));
    }

    // Guest paging, identity at every level: the indices come from
    // `code_pa`, so guest VA == guest PA == host PA wherever the PMM put
    // the frames.
    svm::wr64(gpml4_f.bytes_mut(), ((code_pa >> 39) & 0x1FF) as usize * 8, svm::npt::pointer(gpdpt_pa));
    svm::wr64(gpdpt_f.bytes_mut(), ((code_pa >> 30) & 0x1FF) as usize * 8, svm::npt::pointer(gpd_pa));
    svm::wr64(gpd_f.bytes_mut(), ((code_pa >> 21) & 0x1FF) as usize * 8, svm::npt::leaf_2m(code_pa & !0x1F_FFFF));

    // VMCB — the trivial guest's intercepts and maps; ASID 2, so this
    // guest's translations cannot collide with the trivial guest's in the
    // TLB the ASID tags.
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR6, 0xFFFF_0FF0);
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR7, 0x400);
    {
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        v.set_intr0(
            intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT | intr0::MSR_PROT | intr0::IOIO_PROT,
        );
        v.set_intr1(intr1::VMRUN | intr1::VMMCALL);
        v.set_excp(svm::excp::DB | svm::excp::UD | svm::excp::GP | svm::excp::PF);
        v.set_iopm_base(iopm_pa);
        v.set_msrpm_base(msrpm_pa);
        v.set_asid(2);
        v.set_tlb_control(ctrl::TLB_DO_NOTHING);
        v.set_np_enable(true);
        v.set_n_cr3(npml4_pa);
        let flat = |attr: SegAttr| Seg { selector: 0, attr, limit: 0xFFFF_FFFF, base: 0 };
        v.set_seg(
            save::CS,
            Seg { selector: 0x08, attr: SegAttr::CODE64_DPL0, limit: 0xFFFF_FFFF, base: 0 },
        );
        let data = flat(SegAttr::DATA32_RW_DPL0);
        v.set_seg(save::ES, data);
        v.set_seg(save::SS, data);
        v.set_seg(save::DS, data);
        // FS/GS carry the host bases — same reason as the trivial guest.
        v.set_seg(
            save::FS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::FsBase) },
        );
        v.set_seg(
            save::GS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::GsBase) },
        );
        // 64-bit paged: exactly the state the crate's `long_mode_page` test
        // helper pins as legal. Guest CR3 walks the identity table above;
        // RIP is the guest VA it maps.
        v.set_efer(svm::msr::EFER_SVME | svm::msr::EFER_LME | svm::msr::EFER_LMA);
        v.set_cr0(svm::cr0::PE | svm::cr0::NE | svm::cr0::PG);
        v.set_cr3(gpml4_pa);
        v.set_cr4(svm::cr4::PAE);
        v.set_rflags(0x2);
        v.set_cpl(0);
        v.set_rip(code_pa);
        v.set_rsp(0);
    }

    crumb(0x13);
    {
        let v = Vmcb::new(vmcb_f.bytes_mut());
        if let Err(e) = svm::validate_guest_state(&v) {
            p("[KVM] long-mode guest state rejected: ");
            p(e.name());
            p("\n");
            return;
        }
    }

    crumb(0x14);
    if !enable_on_core(0) {
        p("[KVM] long-mode: no host save area for the BSP\n");
        return;
    }

    crumb(0x15);
    p("[KVM] running long-mode guest (nrips=");
    p(if nrips { "yes" } else { "no" });
    p(", code_pa=");
    hx(code_pa, 16);
    p(")\n");

    // Same excursion shape as the trivial guest: IRQs masked for the whole
    // run, RCX/RDX round-tripped through the loop (Table B-2).
    akuma_cpu::daif::mask_irq();
    let mut vmmcalls = 0u64;
    let mut msr_exits = 0u64;
    let (mut guest_rcx, mut guest_rdx) = (0u64, 0u64);
    let (mut halted, mut why) = (false, "loop exhausted");
    for _ in 0..8 {
        crumb(0x17);
        let (rcx_out, rdx_out) = unsafe { vmrun(vmcb_f.pa() as u64, guest_rcx, guest_rdx) };
        guest_rcx = rcx_out;
        guest_rdx = rdx_out;
        crumb(0x18);
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Msr => {
                msr_exits += 1;
                if guest_rcx != u64::from(svm::msr::EFER) {
                    why = "unexpected MSR access";
                    p("[KVM] long-mode unexpected MSR access: ");
                    dump_exit(&v, guest_rcx, guest_rdx);
                    break;
                }
                // RDMSR of the guest's EFER: value is EDX:EAX, same as the
                // trivial guest's handler.
                let efer = v.efer();
                v.set_rax(efer & 0xFFFF_FFFF);
                guest_rdx = efer >> 32;
                v.set_rip(next_rip(&v, nrips, 2));
            }
            ExitCode::Vmmcall => {
                vmmcalls += 1;
                if vmmcalls == 1 && v.rax() != LONGMODE_MAGIC {
                    why = "checkpoint one: RAX != magic (not a 64-bit decode of the prologue)";
                    p("[KVM] ");
                    p(why);
                    p(": ");
                    dump_exit(&v, guest_rcx, guest_rdx);
                    break;
                }
                if vmmcalls == 2 && v.rax() != v.efer() & 0xFFFF_FFFF {
                    why = "checkpoint two: RAX != EFER[31:0] after RDMSR";
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
                p("[KVM] long-mode unexpected exit: ");
                dump_exit(&v, guest_rcx, guest_rdx);
                dump_state(&v);
                break;
            }
        }
    }
    akuma_cpu::daif::unmask_irq();
    crumb(0x19);

    let final_v = Vmcb::new(vmcb_f.bytes_mut());
    let pass = halted
        && vmmcalls == 2
        && msr_exits == 1
        && final_v.rax() == svm::msr::EFER_SVME | svm::msr::EFER_LME | svm::msr::EFER_LMA;
    p("[KVM] long-mode guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (vmmcalls=");
    dec(vmmcalls);
    p(", msr=");
    dec(msr_exits);
    p(", rax=");
    hx(final_v.rax(), 16);
    p(")\n");
    if !pass {
        p("[KVM] why: ");
        p(why);
        p("\n");
    }
}

// ---------------------------------------------------------------------------
// The two-level rung: both translations non-identity
// ---------------------------------------------------------------------------

/// The checkpoint-one magic — the prologue's `movabs` again, decoded only
/// correctly in 64-bit mode.
const TWOVL_EXEC_MAGIC: u64 = 0x2026_1026_2026_1001;
/// The word at the data VA; its landing in RAX is the second translation
/// doing its job (a different VA → a different guest page → a different
/// host frame than the code's).
const TWOVL_DATA_MAGIC: u64 = 0x2026_2026_5EED_0002;

/// Guest-physical layout, synthetic and deliberately **not** where the PMM
/// puts anything: every page lives in its own 2-MiB NPT region of the 1–2
/// GiB band, and each NPT leaf points at a host window the region base is
/// nothing like — the NPT is non-identity by construction, not by hope.
/// A PMM frame is not 2-MiB aligned, so the leaf maps the frame's aligned
/// window and the page's guest PA carries the frame's own offset inside
/// that window (runtime, not a constant): guest PA = region | frame &
/// 0x1F_FFFF then translates to exactly `frame_pa`, the bytes' home.
const TWOVL_CODE_REGION: u64 = 0x4000_0000;
const TWOVL_DATA_REGION: u64 = 0x4020_0000;
/// The guest page-table pages: PML4, PDPT, PD, PT — each NPT-mapped in its
/// own region, because the guest walker reads them as guest-physical memory
/// (§15.25.5) and four separate frames are not physically contiguous.
const TWOVL_GPT_REGION: [u64; 4] = [0x4040_0000, 0x4060_0000, 0x4080_0000, 0x40A0_0000];

/// Guest-virtual layout, in a third region entirely: the code and the data
/// share a 2-MiB region but sit in **different 4-KiB pages** — if the PT
/// level were skipped (a 2-MiB leaf where the rung builds a 4-KiB one), the
/// data read would land in the code page and checkpoint two fails.
const TWOVL_CODE_VA: u64 = 0x5000_0000;
const TWOVL_DATA_VA: u64 = 0x5000_1000;

/// `movabs r64, imm64` (`48 B8+rd`), the immediate little-endian — built
/// from the layout constants, so the bytes cannot drift from the VAs and
/// magics they encode.
const fn movabs(reg: u8, imm: u64) -> [u8; 10] {
    let b = imm.to_le_bytes();
    [0x48, 0xB8 + reg, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]
}

/// `movabs rax, EXEC_MAGIC`, `vmmcall` (checkpoint one: the fetch and the
/// decode came through gVA → gPA → hPA), `movabs rsi, DATA_VA`,
/// `mov rax, [rsi]` — a *data* walk through the second mapping —
/// `vmmcall` (checkpoint two: RAX = the data word), `hlt`, spin. Same
/// wedge-canary tail as every rehearsal guest.
const TWOVL_CODE: [u8; 32] = {
    let mut c = [0u8; 32];
    let mut i = 0;
    while i < 10 {
        c[i] = movabs(0, TWOVL_EXEC_MAGIC)[i];
        i += 1;
    }
    c[10] = 0x0F;
    c[11] = 0x01;
    c[12] = 0xD9; // vmmcall — checkpoint one
    i = 0;
    while i < 10 {
        c[13 + i] = movabs(6, TWOVL_DATA_VA)[i]; // B8+r6 = rsi
        i += 1;
    }
    c[23] = 0x48;
    c[24] = 0x8B;
    c[25] = 0x06; // mov rax, [rsi] — the data walk
    c[26] = 0x0F;
    c[27] = 0x01;
    c[28] = 0xD9; // vmmcall — checkpoint two: RAX = DATA_MAGIC
    c[29] = 0xF4; // hlt — the stop marker
    c[30] = 0xEB;
    c[31] = 0xFE; // jmp $
    c
};

/// The Linux rung's foundation, proven in isolation: guest VA ≠ guest PA
/// (the guest page tables do real per-page work) *and* guest PA ≠ host PA
/// (the NPT relocates each synthetic region onto wherever the PMM put a
/// frame). The trivial rung proved VMRUN, the long-mode rung proved a paged
/// 64-bit guest over an identity NPT; this rung removes the identity from
/// **both** levels at once — the exact shape a loader stands on when it
/// places a guest image at guest-physical addresses the host does not have.
/// Breadcrumbs 0x31–0x39 mirror the other guests' runs.
fn twolvl_guest(nrips: bool) {
    crumb(0x31);
    let Some(mut vmcb_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for VMCB\n");
        return;
    };
    let Some(mut npml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for NPT PML4\n");
        return;
    };
    let Some(mut npdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for NPT PDPT\n");
        return;
    };
    let Some(mut npd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for NPT PD\n");
        return;
    };
    let Some(mut gpml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest PML4\n");
        return;
    };
    let Some(mut gpdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest PDPT\n");
        return;
    };
    let Some(mut gpd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest PD\n");
        return;
    };
    let Some(mut gpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest PT\n");
        return;
    };
    let Some(mut code_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest code\n");
        return;
    };
    let Some(mut data_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] two-level: no frame for guest data\n");
        return;
    };

    crumb(0x32);
    // The runtime guest-physical address of every page: its region base or'd
    // with the frame's offset inside the aligned window the NPT leaf maps —
    // which is what makes the leaf's translation land on the frame itself.
    let code_gpa = TWOVL_CODE_REGION | (code_f.pa() as u64 & 0x1F_FFFF);
    let data_gpa = TWOVL_DATA_REGION | (data_f.pa() as u64 & 0x1F_FFFF);
    let gpt_gpa = [
        TWOVL_GPT_REGION[0] | (gpml4_f.pa() as u64 & 0x1F_FFFF),
        TWOVL_GPT_REGION[1] | (gpdpt_f.pa() as u64 & 0x1F_FFFF),
        TWOVL_GPT_REGION[2] | (gpd_f.pa() as u64 & 0x1F_FFFF),
        TWOVL_GPT_REGION[3] | (gpt_f.pa() as u64 & 0x1F_FFFF),
    ];
    code_f.bytes_mut()[..TWOVL_CODE.len()].copy_from_slice(&TWOVL_CODE);
    data_f.bytes_mut()[..8].copy_from_slice(&TWOVL_DATA_MAGIC.to_le_bytes());

    let (msrpm_pa, iopm_pa) = (image_pa(&MSRPM.0), image_pa(&IOPM.0));
    if msrpm_pa % 4096 != 0 || iopm_pa % 4096 != 0 {
        p("[KVM] permission map base not 4-KiB aligned\n");
        return;
    }

    // NPT: every synthetic 2-MiB region → the aligned window holding the
    // frame that plays it. All six regions share PDPT index 1 (the 1–2 GiB
    // band), so one NPT PD covers them.
    let npml4_pa = npml4_f.pa() as u64;
    let npdpt_pa = npdpt_f.pa() as u64;
    let npd_pa = npd_f.pa() as u64;
    svm::wr64(npml4_f.bytes_mut(), 0, svm::npt::pointer(npdpt_pa));
    svm::wr64(npdpt_f.bytes_mut(), 8, svm::npt::pointer(npd_pa));
    for (region, frame_pa) in [
        (TWOVL_CODE_REGION, code_f.pa() as u64),
        (TWOVL_DATA_REGION, data_f.pa() as u64),
        (TWOVL_GPT_REGION[0], gpml4_f.pa() as u64),
        (TWOVL_GPT_REGION[1], gpdpt_f.pa() as u64),
        (TWOVL_GPT_REGION[2], gpd_f.pa() as u64),
        (TWOVL_GPT_REGION[3], gpt_f.pa() as u64),
    ] {
        let idx = ((region >> 21) & 0x1FF) as usize;
        svm::wr64(npd_f.bytes_mut(), idx * 8, svm::npt::leaf_2m(frame_pa & !0x1F_FFFF));
    }

    // Guest paging: three pointer entries and two 4-KiB leaves — the PT
    // level is real, and every entry holds a *guest-physical* address for
    // the NPT to relocate. PML4[0] → PDPT, PDPT[1] → PD, PD[0] → PT,
    // PT[0] → code GPA, PT[1] → data GPA.
    svm::wr64(gpml4_f.bytes_mut(), 0, svm::npt::pointer(gpt_gpa[1]));
    svm::wr64(gpdpt_f.bytes_mut(), 8, svm::npt::pointer(gpt_gpa[2]));
    svm::wr64(gpd_f.bytes_mut(), 0, svm::npt::pointer(gpt_gpa[3]));
    svm::wr64(gpt_f.bytes_mut(), 0, svm::npt::leaf_4k(code_gpa));
    svm::wr64(gpt_f.bytes_mut(), 8, svm::npt::leaf_4k(data_gpa));

    // VMCB — the long-mode rung's state with the synthetic CR3 and VA in
    // place of the identity ones; ASID 14 stays clear of the ladder's 1–13.
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR6, 0xFFFF_0FF0);
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR7, 0x400);
    {
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        v.set_intr0(
            intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT | intr0::MSR_PROT | intr0::IOIO_PROT,
        );
        v.set_intr1(intr1::VMRUN | intr1::VMMCALL);
        v.set_excp(svm::excp::DB | svm::excp::UD | svm::excp::GP | svm::excp::PF);
        v.set_iopm_base(iopm_pa);
        v.set_msrpm_base(msrpm_pa);
        v.set_asid(14);
        v.set_tlb_control(ctrl::TLB_DO_NOTHING);
        v.set_np_enable(true);
        v.set_n_cr3(npml4_pa);
        let flat = |attr: SegAttr| Seg { selector: 0, attr, limit: 0xFFFF_FFFF, base: 0 };
        v.set_seg(
            save::CS,
            Seg { selector: 0x08, attr: SegAttr::CODE64_DPL0, limit: 0xFFFF_FFFF, base: 0 },
        );
        let data = flat(SegAttr::DATA32_RW_DPL0);
        v.set_seg(save::ES, data);
        v.set_seg(save::SS, data);
        v.set_seg(save::DS, data);
        // FS/GS carry the host bases, same as every rehearsal guest.
        v.set_seg(
            save::FS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::FsBase) },
        );
        v.set_seg(
            save::GS,
            Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::GsBase) },
        );
        v.set_efer(svm::msr::EFER_SVME | svm::msr::EFER_LME | svm::msr::EFER_LMA);
        v.set_cr0(svm::cr0::PE | svm::cr0::NE | svm::cr0::PG);
        v.set_cr3(gpt_gpa[0]);
        v.set_cr4(svm::cr4::PAE);
        v.set_rflags(0x2);
        v.set_cpl(0);
        v.set_rip(TWOVL_CODE_VA);
        v.set_rsp(0);
    }

    crumb(0x33);
    {
        let v = Vmcb::new(vmcb_f.bytes_mut());
        if let Err(e) = svm::validate_guest_state(&v) {
            p("[KVM] two-level guest state rejected: ");
            p(e.name());
            p("\n");
            return;
        }
    }

    crumb(0x34);
    if !enable_on_core(0) {
        p("[KVM] two-level: no host save area for the BSP\n");
        return;
    }

    crumb(0x35);
    p("[KVM] running two-level guest (gva=");
    hx(TWOVL_CODE_VA, 16);
    p(" gpa=");
    hx(code_gpa, 16);
    p(" hpa=");
    hx(code_f.pa() as u64, 16);
    p(")\n");

    // Same excursion shape as both earlier guests: IRQs masked for the run,
    // RCX/RDX round-tripped (Table B-2; this guest writes RSI only inside
    // itself, before any exit, so the shared-GPR contract does not reach it).
    akuma_cpu::daif::mask_irq();
    let mut vmmcalls = 0u64;
    let (mut guest_rcx, mut guest_rdx) = (0u64, 0u64);
    let (mut halted, mut why) = (false, "loop exhausted");
    for _ in 0..8 {
        crumb(0x37);
        let (rcx_out, rdx_out) = unsafe { vmrun(vmcb_f.pa() as u64, guest_rcx, guest_rdx) };
        guest_rcx = rcx_out;
        guest_rdx = rdx_out;
        crumb(0x38);
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Vmmcall => {
                vmmcalls += 1;
                if vmmcalls == 1 && v.rax() != TWOVL_EXEC_MAGIC {
                    why = "checkpoint one: RAX != exec magic (fetch through gVA/gPA failed)";
                    p("[KVM] ");
                    p(why);
                    p(": ");
                    dump_exit(&v, guest_rcx, guest_rdx);
                    break;
                }
                if vmmcalls == 2 && v.rax() != TWOVL_DATA_MAGIC {
                    why = "checkpoint two: RAX != data magic (the data walk did not land)";
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
                p("[KVM] two-level unexpected exit: ");
                dump_exit(&v, guest_rcx, guest_rdx);
                dump_state(&v);
                break;
            }
        }
    }
    akuma_cpu::daif::unmask_irq();
    crumb(0x39);

    let final_v = Vmcb::new(vmcb_f.bytes_mut());
    let pass = halted && vmmcalls == 2 && final_v.rax() == TWOVL_DATA_MAGIC;
    p("[KVM] two-level guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (vmmcalls=");
    dec(vmmcalls);
    p(", rax=");
    hx(final_v.rax(), 16);
    p(")\n");
    if !pass {
        p("[KVM] why: ");
        p(why);
        p("\n");
    }
}

// ---------------------------------------------------------------------------
// The consistency ladder
// ---------------------------------------------------------------------------

/// `vmmcall; jmp $`. The probe's first instruction is intercepted by
/// `intr1::VMMCALL`, so any exit other than `VMEXIT_INVALID` proves VMRUN
/// accepted the state and entered the guest; the trailing `jmp $` is the
/// wedge-canary every rehearsal guest carries.
const PROBE_CODE: [u8; 5] = [0x0F, 0x01, 0xD9, 0xEB, 0xFE];

/// One ladder row: which ingredients the probe's guest state carries. Everything
/// not named here is identical to the long-mode rung (the intercepts, the
/// permission maps, DR6/DR7, the data segments, rflags, cpl, rip, rsp).
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
struct LadderCfg {
    efer_lme: bool,
    efer_lma: bool,
    cr0_pg: bool,
    cr0_ne: bool,
    cr4_pae: bool,
    cr3_g: bool,
    cs64: bool,
    cs_sel8: bool,
    asid: u32,
}

/// The L0 (KVM under QEMU) rejects the long-mode rung with `VMEXIT_INVALID`
/// while the trivial rung passes, and an audit of KVM v6.6/v6.8/v6.9
/// `__nested_vmcb_check_save`/`__nested_vmcb_check_controls` passes our state
/// on paper, so the failing check is version-specific or outside those two
/// functions. The ladder is the bisect: one VMRUN per row against the trivial
/// baseline, each row flipping one or two ingredients, each verdict printed.
/// Every row passes `akuma_svm_amd64`'s APM §15.5.1 transcription (a row that
/// did not would print `illegal-self` instead of running). The rejection
/// pattern, cross-checked with the L0's own `CC()` WARN in dmesg, names the
/// failing check exactly.
const LADDER: [(&str, LadderCfg); 12] = [
    ("full", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 3 }),
    ("no-lma", LadderCfg { efer_lme: true, efer_lma: false, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 4 }),
    ("lme-off", LadderCfg { efer_lme: false, efer_lma: false, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 5 }),
    ("no-pg", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: false, cr0_ne: true, cr4_pae: true, cr3_g: false, cs64: true, cs_sel8: true, asid: 6 }),
    ("cs32", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: false, cs_sel8: true, asid: 7 }),
    ("sel0", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: false, asid: 8 }),
    ("asid1", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 1 }),
    ("no-ne", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: true, cr0_ne: false, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 9 }),
    ("t-cs64", LadderCfg { efer_lme: false, efer_lma: false, cr0_pg: false, cr0_ne: true, cr4_pae: false, cr3_g: false, cs64: true, cs_sel8: true, asid: 10 }),
    ("t-pg", LadderCfg { efer_lme: false, efer_lma: false, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: false, cs_sel8: false, asid: 11 }),
    ("t-cs64-pg", LadderCfg { efer_lme: false, efer_lma: false, cr0_pg: true, cr0_ne: true, cr4_pae: true, cr3_g: true, cs64: true, cs_sel8: true, asid: 12 }),
    ("t-lme-lma", LadderCfg { efer_lme: true, efer_lma: true, cr0_pg: false, cr0_ne: true, cr4_pae: false, cr3_g: false, cs64: false, cs_sel8: false, asid: 13 }),
];

/// Run the ladder. Each probe gets a freshly zeroed VMCB frame, a unique ASID
/// (except the deliberate `asid1` row) and `TLB_DO_NOTHING`, so consecutive
/// probes cannot poison each other through the L0's shadow state; the frame
/// is freed between rows, which KVM tolerates (its shadow pages key on the
/// L1 GPAs, whose backing HPAs do not move). IRQs stay masked across the
/// whole ladder: one VMRUN per row, no waiting in between.
fn consistency_ladder() {
    crumb(0x21);
    let Some(mut code_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for probe code\n");
        return;
    };
    let Some(mut npml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for NPT PML4\n");
        return;
    };
    let Some(mut npdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for NPT PDPT\n");
        return;
    };
    let Some(mut npd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for NPT PD\n");
        return;
    };
    let Some(mut gpml4_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for guest PML4\n");
        return;
    };
    let Some(mut gpdpt_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for guest PDPT\n");
        return;
    };
    let Some(mut gpd_f) = FreshFrame::alloc_zeroed() else {
        p("[KVM] ladder: no frame for guest PD\n");
        return;
    };

    let code_pa = code_f.pa() as u64;
    code_f.bytes_mut()[..PROBE_CODE.len()].copy_from_slice(&PROBE_CODE);
    let (msrpm_pa, iopm_pa) = (image_pa(&MSRPM.0), image_pa(&IOPM.0));

    // Nested and guest paging, built once for `probe_pa` exactly as the
    // long-mode rung builds them; rows that do not page simply never walk.
    let npml4_pa = npml4_f.pa() as u64;
    let npdpt_pa = npdpt_f.pa() as u64;
    let npd_pa = npd_f.pa() as u64;
    let gpml4_pa = gpml4_f.pa() as u64;
    let gpdpt_pa = gpdpt_f.pa() as u64;
    let gpd_pa = gpd_f.pa() as u64;
    svm::wr64(npml4_f.bytes_mut(), 0, svm::npt::pointer(npdpt_pa));
    svm::wr64(npdpt_f.bytes_mut(), 0, svm::npt::pointer(npd_pa));
    for pa in [code_pa, gpml4_pa, gpdpt_pa, gpd_pa] {
        let idx = ((pa >> 21) & 0x1FF) as usize;
        svm::wr64(npd_f.bytes_mut(), idx * 8, svm::npt::leaf_2m(pa & !0x1F_FFFF));
    }
    svm::wr64(gpml4_f.bytes_mut(), ((code_pa >> 39) & 0x1FF) as usize * 8, svm::npt::pointer(gpdpt_pa));
    svm::wr64(gpdpt_f.bytes_mut(), ((code_pa >> 30) & 0x1FF) as usize * 8, svm::npt::pointer(gpd_pa));
    svm::wr64(gpd_f.bytes_mut(), ((code_pa >> 21) & 0x1FF) as usize * 8, svm::npt::leaf_2m(code_pa & !0x1F_FFFF));

    if !enable_on_core(0) {
        p("[KVM] ladder: no host save area for the BSP\n");
        return;
    }

    p("[KVM] ladder: ");
    dec(LADDER.len() as u64);
    p(" probes, code_pa=");
    hx(code_pa, 16);
    p("\n");

    akuma_cpu::daif::mask_irq();
    for (i, (name, cfg)) in LADDER.iter().enumerate() {
        let Some(mut vmcb_f) = FreshFrame::alloc_zeroed() else {
            p("[KVM] ladder ");
            p(name);
            p(": no frame for VMCB\n");
            break;
        };
        p("[KVM] ladder ");
        p(name);
        p(": ");

        svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR6, 0xFFFF_0FF0);
        svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR7, 0x400);
        {
            let mut v = Vmcb::new(vmcb_f.bytes_mut());
            v.set_intr0(
                intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT | intr0::MSR_PROT | intr0::IOIO_PROT,
            );
            v.set_intr1(intr1::VMRUN | intr1::VMMCALL);
            v.set_excp(svm::excp::DB | svm::excp::UD | svm::excp::GP | svm::excp::PF);
            v.set_iopm_base(iopm_pa);
            v.set_msrpm_base(msrpm_pa);
            v.set_asid(cfg.asid);
            v.set_tlb_control(ctrl::TLB_DO_NOTHING);
            v.set_np_enable(true);
            v.set_n_cr3(npml4_pa);
            let flat = |attr: SegAttr| Seg { selector: 0, attr, limit: 0xFFFF_FFFF, base: 0 };
            v.set_seg(
                save::CS,
                Seg {
                    selector: if cfg.cs_sel8 { 0x08 } else { 0 },
                    attr: if cfg.cs64 { SegAttr::CODE64_DPL0 } else { SegAttr::CODE32_DPL0 },
                    limit: 0xFFFF_FFFF,
                    base: 0,
                },
            );
            let data = flat(SegAttr::DATA32_RW_DPL0);
            v.set_seg(save::ES, data);
            v.set_seg(save::SS, data);
            v.set_seg(save::DS, data);
            // FS/GS carry the host bases, same as every rehearsal guest.
            v.set_seg(
                save::FS,
                Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::FsBase) },
            );
            v.set_seg(
                save::GS,
                Seg { selector: 0, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base: msr::read(Msr::GsBase) },
            );
            let mut efer = svm::msr::EFER_SVME;
            if cfg.efer_lme {
                efer |= svm::msr::EFER_LME;
            }
            if cfg.efer_lma {
                efer |= svm::msr::EFER_LMA;
            }
            v.set_efer(efer);
            let mut cr0 = svm::cr0::PE;
            if cfg.cr0_ne {
                cr0 |= svm::cr0::NE;
            }
            if cfg.cr0_pg {
                cr0 |= svm::cr0::PG;
            }
            v.set_cr0(cr0);
            v.set_cr3(if cfg.cr3_g { gpml4_pa } else { 0 });
            v.set_cr4(if cfg.cr4_pae { svm::cr4::PAE } else { 0 });
            v.set_rflags(0x2);
            v.set_cpl(0);
            v.set_rip(code_pa);
            v.set_rsp(0);
        }
        {
            let v = Vmcb::new(vmcb_f.bytes_mut());
            if let Err(e) = svm::validate_guest_state(&v) {
                p("illegal-self ");
                p(e.name());
                p("\n");
                continue;
            }
        }

        crumb(0x22 + i as u8);
        unsafe { vmrun(vmcb_f.pa() as u64, 0, 0) };
        let v = Vmcb::new(vmcb_f.bytes_mut());
        if ExitCode::from_u64(v.exitcode()) == ExitCode::Invalid {
            p("rejected\n");
            dump_state(&v);
        } else {
            p("entered exit=");
            hx(v.exitcode(), 8);
            p("\n");
        }
    }
    akuma_cpu::daif::unmask_irq();
    crumb(0x2E);
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

/// The state half of a self-contained INVALID dump: which EFER/CR values the
/// L0 was handed when it refused the guest. The exit line plus this one
/// identify the failing L0 check without a second boot.
fn dump_state(v: &Vmcb<'_>) {
    p("[KVM] state: efer=");
    hx(v.efer(), 16);
    p(" cr0=");
    hx(v.cr0(), 16);
    p(" cr3=");
    hx(v.cr3(), 16);
    p(" cr4=");
    hx(v.cr4(), 16);
    p(" asid=");
    dec(u64::from(v.asid()));
    p(" cs.sel=");
    hx(u64::from(v.seg(save::CS).selector), 4);
    p(" cs.attr=");
    hx(u64::from(v.seg(save::CS).attr.0), 4);
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
