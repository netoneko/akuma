//! The AMD SVM world switch: every SVM instruction the kernel executes, and nothing else.
//!
//! `akuma-svm-amd64` is the hypervisor's vocabulary (VMCB layouts, NPT encodings, device
//! models) and forbids `unsafe`; `akuma-drivers-amd64`'s `kvm` module is the wiring. Between
//! them sit the instructions only ring 0 can execute: `wrmsr VM_HSAVE_PA`, `EFER.SVME`,
//! `VMSAVE`, `VMLOAD` and `VMRUN`. They were open-coded in the wiring (two MSR writes, three
//! `asm!` blocks, seven `unsafe` call sites); here each is behind the type that discharges
//! its obligation, the `akuma-port` / `akuma-psci` shape — a leaf crate whose only job is
//! one stated contract.
//!
//! What closes by type, and is therefore safe:
//!
//! - **[`enable`]** takes a [`HostSaveArea`], which can only be made by giving up a
//!   [`FreshFrame`]: the page `VM_HSAVE_PA` names is the CPU's for the life of the kernel
//!   (APM Vol 2 §15.30.4), so it is leaked, and a value that is consumed by `enable` cannot
//!   name the same page for a second core. It checks CPUID and `VM_CR.SVMDIS` first: setting
//!   `EFER.SVME` where SVM is absent or firmware-locked `#GP`s.
//! - **[`HostState::save`]** is `VMSAVE` into a page the value owns alone.
//!
//! - **[`Guest::run`]** is `VMRUN`. The guest's reach is whatever its VMCB says, so a
//!   [`Guest`] owns everything that decides it — its VMCB page, its [`Asid`], its [`Npt`] (which
//!   owns the [`GuestRam`] and pages it maps, and maps nothing else) — and rewrites every
//!   confinement field from those values on each entry (`guest.rs` lists them). Nothing a caller
//!   writes into the VMCB can widen what the guest reaches; a caller can only make it fault.
//!
//! The `unsafe` left here is the instructions themselves, each behind the type that discharges
//! its obligation: the two MSR writes in [`enable`], `VMSAVE` in [`HostState::save`], `CLGI`,
//! and the one `VMLOAD`/`VMRUN`/`VMSAVE` block in `enter`, reachable only from [`Guest::run`].
//! The wiring (`akuma-kvm-amd64`) forbids `unsafe`.
//!
//! Memory safety, not harmlessness (the `akuma-port` rule): `VMSAVE` or `VMRUN` on a core
//! whose `EFER.SVME` is clear raises `#UD`, which faults the kernel in the open; it is not a
//! silent corruption, so [`HostState::save`] does not pay an `rdmsr` per entry to avoid it.
//!
//! Spec-first, like its siblings: every bit traces to the AMD APM (24593); no Linux/KVM source.

#![no_std]
#![cfg(all(target_arch = "x86_64", target_os = "none"))]
// `deny`, not `forbid`: the instructions are `unsafe` by nature and each block below carries
// its own `allow` and SAFETY argument.
#![deny(unsafe_code)]

extern crate alloc;

mod asid;
mod guest;
mod npt;
mod ram;

pub use asid::Asid;
pub use guest::{Guest, Refused};
pub use npt::{MapError, Npt, RamId};
pub use ram::{reserve, GuestRam};

use akuma_cpu::msr::{self, Msr};
use akuma_phys::FreshFrame;
use akuma_primitives::addr::PhysAddr;
use core::arch::x86_64::__cpuid;

/// `EFER.SVME`, bit 12 (APM Vol 2 §15.4).
const EFER_SVME: u64 = 1 << 12;
/// `VM_CR.SVMDIS`, bit 4 (APM Vol 2 §15.30.1).
const VM_CR_SVMDIS: u64 = 1 << 4;

/// Why SVM cannot be enabled on this core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unavailable {
    /// CPUID does not advertise SVM (`Fn8000_0001.ECX[2]`), or the extended range stops short.
    Absent,
    /// `VM_CR.SVMDIS` is set: the firmware turned SVM off.
    FirmwareDisabled,
}

/// Can SVM be enabled on this core? CPUID first: `VM_CR` is only read once SVM is advertised,
/// because an `rdmsr` of an unimplemented MSR `#GP`s.
///
/// # Errors
/// Which of the two conditions failed.
pub fn available() -> Result<(), Unavailable> {
    if __cpuid(0x8000_0000).eax < 0x8000_0001 || __cpuid(0x8000_0001).ecx & (1 << 2) == 0 {
        return Err(Unavailable::Absent);
    }
    if msr::read(Msr::VmCr) & VM_CR_SVMDIS != 0 {
        return Err(Unavailable::FirmwareDisabled);
    }
    Ok(())
}

/// Is `EFER.SVME` set on this core?
#[must_use]
pub fn enabled() -> bool {
    msr::read(Msr::Efer) & EFER_SVME != 0
}

/// A host save area: the page `VM_HSAVE_PA` names, where `VMRUN` parks the host's state for
/// the duration of a guest entry. One per core, live for the life of the kernel.
///
/// Made only from a [`FreshFrame`], which it gives up for good: nothing else can map, free or
/// write the page afterwards. Not `Clone`: [`enable`] consumes it, so one page can never be
/// two cores' save area (two cores entering guests at once would each restore the other's
/// stack pointer on exit).
pub struct HostSaveArea {
    pa: u64,
}

impl HostSaveArea {
    /// Take `frame` for good.
    #[must_use]
    pub fn new(frame: FreshFrame) -> Self {
        Self { pa: frame.publish() as u64 }
    }
}

/// Point this core's `VM_HSAVE_PA` at `area` and set `EFER.SVME` (APM Vol 2 §15.4,
/// §15.30.4). Both are per-core MSR state, so every core that will enter a guest calls this
/// once, on itself.
///
/// # Errors
/// [`available`]'s, before either MSR is touched; `area` is then dropped (leaked, as it was
/// already given up).
// Taking `area` by value is the point: the page can then be no other core's save area.
#[allow(clippy::needless_pass_by_value)]
pub fn enable(area: HostSaveArea) -> Result<(), Unavailable> {
    available()?;
    let HostSaveArea { pa } = area;
    let efer = msr::read(Msr::Efer);
    #[allow(unsafe_code)]
    // SAFETY: `VM_HSAVE_PA` must name a live 4-KiB-aligned page that nothing else uses and
    // that is not a VMCB (§15.30.4): `area` is a PMM frame given up by `HostSaveArea::new`,
    // so it is aligned, and it is consumed here, so no other core and no VMCB can be it.
    // `EFER.SVME` only makes the SVM instructions executable; the read-modify-write keeps
    // what paging and syscall init set (LME, LMA, NXE, SCE). SVM is present and not
    // firmware-locked (checked above), so neither write `#GP`s.
    unsafe {
        msr::write(Msr::VmHsavePa, pa);
        msr::write(Msr::Efer, efer | EFER_SVME);
    }
    Ok(())
}

/// The host's own FS/GS/TR/LDTR/`KernelGsBase`/`STAR`/`LSTAR`/`CSTAR`/`SFMASK`/
/// `SYSENTER_*` state, in a VMCB-format page: what `VMRUN` itself does not save and
/// [`Guest::run`] `VMLOAD`s back after every exit.
pub struct HostState {
    frame: FreshFrame,
}

impl HostState {
    /// A zeroed page for it. [`Guest::run`] saves into it on every entry.
    #[must_use]
    pub fn alloc() -> Option<Self> {
        FreshFrame::alloc_zeroed().map(|frame| Self { frame })
    }

    /// `VMSAVE` this core's current state into the page. [`Guest::run`] does it on every
    /// entry, with GIF clear: another host task may have moved `KernelGsBase` or `FS_BASE`
    /// since the last one, or the task may be on a different core, and a stale save would put
    /// the wrong value back on exit.
    // `&mut`: VMSAVE writes the page, and no other borrow may observe it mid-write.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(crate) fn save(&mut self) {
        let pa = self.frame.pa().get();
        #[allow(unsafe_code)]
        // SAFETY: `pa` is a 4-KiB-aligned PMM frame this value owns alone (`&mut self`, and no
        // byte view of it is ever handed out); VMSAVE writes the state-save layout into that
        // page and touches no other memory.
        unsafe {
            core::arch::asm!("vmsave rax", in("rax") pa, options(nostack, preserves_flags));
        }
    }
}

/// The guest general registers `VMRUN` does not carry (RAX, RSP, RIP and RFLAGS live in the
/// VMCB). Field order is `enter`'s offsets; the layout is pinned below.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GuestRegs {
    pub rbx: u64, // 0x00
    pub rcx: u64, // 0x08
    pub rdx: u64, // 0x10
    pub rsi: u64, // 0x18
    pub rdi: u64, // 0x20
    pub rbp: u64, // 0x28
    pub r8: u64,  // 0x30
    pub r9: u64,  // 0x38
    pub r10: u64, // 0x40
    pub r11: u64, // 0x48
    pub r12: u64, // 0x50
    pub r13: u64, // 0x58
    pub r14: u64, // 0x60
    pub r15: u64, // 0x68
}

const _: () = {
    assert!(core::mem::size_of::<GuestRegs>() == 0x70);
    assert!(core::mem::offset_of!(GuestRegs, rdi) == 0x20);
    assert!(core::mem::offset_of!(GuestRegs, rbp) == 0x28);
    assert!(core::mem::offset_of!(GuestRegs, r15) == 0x68);
};

/// Clear GIF (APM Vol 2 §15.17): no interrupt, NMI or SMI is taken until [`enter`]'s `STGI`.
fn clgi() {
    #[allow(unsafe_code)]
    // SAFETY: CLGI touches no memory and only holds interrupts pending; `Guest::run` calls it
    // only on its way into `enter`, which ends with STGI on every path.
    unsafe {
        core::arch::asm!("clgi", options(nomem, nostack, preserves_flags));
    }
}

/// Load `regs`, `VMLOAD` the guest's FS/GS/TR/LDTR and syscall MSRs, `VMRUN`, and on the way
/// out store its registers back into `regs`, `VMSAVE` its state into its VMCB and `VMLOAD`
/// `host`'s. Returns with GIF set. Called with GIF clear, only by [`Guest::run`].
#[inline(never)]
fn enter(vmcb: PhysAddr, host: &HostState, regs: &mut GuestRegs) {
    #[allow(unsafe_code)]
    // SAFETY: `vmcb` is the VMCB page of the `Guest` whose `run` is the only caller, which owns
    // it and has just rewritten its confinement fields from owned values (`guest.rs`): nested
    // paging over that guest's own `Npt`, every port and MSR intercepted, VMLOAD/VMSAVE/VMRUN/
    // STGI/CLGI/SKINIT/XSETBV intercepted, its own ASID flushed on a first entry per core. So
    // the guest reaches nothing but memory it owns. `host` was VMSAVEd on this core just
    // before, with GIF already clear, so the VMLOAD after the exit restores this core's state.
    // The stack pushes are balanced (so no `nostack`); RBX and
    // RBP cannot be operands and are saved by hand; every other GPR is declared clobbered,
    // which is the shared-register contract; RSP is the host's again at #VMEXIT, so
    // `[rsp]` (`regs`) and `[rsp+8]` (`host`'s page) are intact. `host`'s page is only read
    // (VMLOAD), so a shared borrow is enough; `regs` is written, through the `&mut`.
    unsafe {
        core::arch::asm!(
            "push rbp",
            "push rbx",
            "push rsi",
            "push rdi",
            "mov rbx, [rdi + 0x00]",
            "mov rcx, [rdi + 0x08]",
            "mov rdx, [rdi + 0x10]",
            "mov rsi, [rdi + 0x18]",
            "mov rbp, [rdi + 0x28]",
            "mov r8,  [rdi + 0x30]",
            "mov r9,  [rdi + 0x38]",
            "mov r10, [rdi + 0x40]",
            "mov r11, [rdi + 0x48]",
            "mov r12, [rdi + 0x50]",
            "mov r13, [rdi + 0x58]",
            "mov r14, [rdi + 0x60]",
            "mov r15, [rdi + 0x68]",
            "mov rdi, [rdi + 0x20]",
            "vmload rax",
            "vmrun rax",
            "vmsave rax",
            // RAX is the VMCB PA again (the host's own); the guest's registers are all still
            // in the file. Spill them.
            "mov rax, [rsp]",
            "mov [rax + 0x00], rbx",
            "mov [rax + 0x08], rcx",
            "mov [rax + 0x10], rdx",
            "mov [rax + 0x18], rsi",
            "mov [rax + 0x20], rdi",
            "mov [rax + 0x28], rbp",
            "mov [rax + 0x30], r8",
            "mov [rax + 0x38], r9",
            "mov [rax + 0x40], r10",
            "mov [rax + 0x48], r11",
            "mov [rax + 0x50], r12",
            "mov [rax + 0x58], r13",
            "mov [rax + 0x60], r14",
            "mov [rax + 0x68], r15",
            "mov rax, [rsp + 8]",
            "vmload rax",
            "stgi",
            "add rsp, 16",
            "pop rbx",
            "pop rbp",
            inout("rax") vmcb.get() => _,
            inout("rsi") host.frame.pa().get() => _,
            inout("rdi") core::ptr::from_mut(regs) => _,
            lateout("rcx") _,
            lateout("rdx") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            lateout("r12") _,
            lateout("r13") _,
            lateout("r14") _,
            lateout("r15") _,
            options()
        );
    }
}
