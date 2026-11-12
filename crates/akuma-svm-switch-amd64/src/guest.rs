//! A guest: its VMCB, its ASID and its nested page table, entered only through [`Guest::run`].
//!
//! `VMRUN` hands the core to whatever the VMCB describes. The caller may write any field it
//! likes — the guest's register state, its intercepts, event injection — through
//! [`Guest::vmcb`]; the fields that decide what the guest can *reach* are rewritten here, from
//! values this crate owns, on every entry (APM Vol 2 §15, Table B-1):
//!
//! - **Memory.** `NP_ENABLE` is exactly nested paging (no SEV, SEV-ES, PML or other mode bits:
//!   each of those makes the CPU use a host-physical pointer from the VMCB), and `N_CR3` is the
//!   guest's own [`Npt`], which maps nothing but memory it owns. `VMLOAD`, `VMSAVE` (whose
//!   operand is a *host*-physical address when nested paging is not virtualizing it), `VMRUN`,
//!   `SKINIT`, `STGI` and `CLGI` are intercepted, and virtual `VMLOAD`/`VMSAVE` and AVIC (a
//!   host-physical backing page) are off.
//! - **Ports and MSRs.** `IOIO_PROT` and `MSR_PROT` are intercepted and both permission maps
//!   are this crate's, with every bit set: no `in`/`out` reaches a device and no `rdmsr`/`wrmsr`
//!   reaches the host's MSRs. `XSETBV` is intercepted (XCR0 is not swapped by `VMRUN`; a guest
//!   could otherwise grow the host's XSAVE layout under it).
//! - **The host's interrupts.** `V_INTR_MASKING` is set — the host's `IF` decides whether a
//!   physical interrupt ends the entry and the guest's `CR8` is virtual — and `SHUTDOWN` is
//!   intercepted, so a guest triple fault is an exit rather than a reset of the machine.
//! - **The TLB.** `GUEST_ASID` is the guest's own [`Asid`], unique among live guests, and the
//!   first entry on each core (and the first after [`Guest::set_npt`]) flushes that ASID, so no
//!   entry a previous holder of the number left behind can be used.
//! - **The VMCB clean bits** are zero: the CPU reloads everything, so a VMCB that moved between
//!   cores, or was edited, is never run from stale cached state (§15.15).
//!
//! The host's own state is `VMSAVE`d into a [`HostState`] page on every entry, after GIF is
//! cleared on the core that will run the guest, and `VMLOAD`ed back after the exit: a host
//! task that moved `FS_BASE` or `KernelGsBase` between two entries, or a guest entered on a
//! different core than the last one, cannot get stale host state restored.

use alloc::sync::Arc;
use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicU8, Ordering};

use akuma_phys::FreshFrame;
use akuma_svm_amd64::{ctrl, intr0, intr1, msrpm, iopm, rd32, rd64, wr32, wr64, Vmcb, VMCB_SIZE};

use crate::asid::Asid;
use crate::npt::Npt;
use crate::{enter, GuestRegs, HostState};

/// `V_INTR` bit 31: AVIC enable (Table B-1).
const V_INTR_AVIC: u64 = 1 << 31;
/// `V_INTR` bit 30: x2AVIC enable.
const V_INTR_X2AVIC: u64 = 1 << 30;
/// Offset `0B8h` bit 1: virtual `VMLOAD`/`VMSAVE` enable.
const VIRT_VMLOAD_VMSAVE: (usize, u32) = (0x0B8, 1 << 1);
/// Cores the entered-mask tracks; a core past it flushes on every entry.
const MASK_CORES: usize = 256;

/// Both permission maps, every bit set. 4 KiB aligned; physically contiguous because the kernel
/// image is loaded contiguously at `VA - KERNEL_VMA` (the `.bss`/`.data` argument the wiring's
/// maps always rested on).
#[repr(C, align(4096))]
struct Msrpm([u8; msrpm::SIZE]);
#[repr(C, align(4096))]
struct Iopm([u8; iopm::SIZE]);
static MSRPM_ALL: Msrpm = Msrpm([0xFF; msrpm::SIZE]);
static IOPM_ALL: Iopm = Iopm(iopm::all());

fn image_pa<T>(r: &T) -> u64 {
    akuma_primitives::addr::virt_to_phys(core::ptr::from_ref(r) as usize) as u64
}

/// The `TLB_CONTROL` that flushes one ASID: `FLUSH_GUEST` where the CPU has `FlushByAsid`
/// (`CPUID Fn8000_000A_EDX[6]`), the whole TLB where it does not. 0 until read.
static FLUSH: AtomicU8 = AtomicU8::new(0);
/// `CPUID Fn8000_000A_EDX[0]`, nested paging: 0 unread, 1 absent, 2 present.
static NP: AtomicU8 = AtomicU8::new(0);

fn svm_features() -> (bool, u8) {
    if NP.load(Ordering::Relaxed) == 0 {
        let edx = if __cpuid(0x8000_0000).eax >= 0x8000_000A { __cpuid(0x8000_000A).edx } else { 0 };
        FLUSH.store(if edx & (1 << 6) != 0 { ctrl::TLB_FLUSH_GUEST } else { ctrl::TLB_FLUSH_ALL }, Ordering::Relaxed);
        NP.store(if edx & 1 != 0 { 2 } else { 1 }, Ordering::Relaxed);
    }
    (NP.load(Ordering::Relaxed) == 2, FLUSH.load(Ordering::Relaxed))
}

/// Why [`Guest::run`] did not enter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No nested page table yet ([`Guest::set_npt`]).
    NoNpt,
    /// The CPU has no nested paging: there is no way to confine a guest's memory.
    NoNestedPaging,
}

/// One virtual processor's VMCB, ASID and nested page table.
pub struct Guest {
    vmcb: FreshFrame,
    asid: Asid,
    npt: Option<Arc<Npt>>,
    /// Cores this guest has entered since its table was set: bit `c` of word `c / 64`.
    entered: [u64; MASK_CORES / 64],
}

impl Guest {
    /// A zeroed VMCB for `asid`, no table yet. The confinement fields are written at once (and
    /// again on every entry), so the VMCB reads as it will run — host-side validation
    /// (`akuma_svm_amd64::validate_guest_state`) sees the real ASID.
    #[must_use]
    pub fn new(asid: Asid) -> Option<Self> {
        let mut g = Self { vmcb: FreshFrame::alloc_zeroed()?, asid, npt: None, entered: [0; MASK_CORES / 64] };
        g.preset();
        Some(g)
    }

    /// Write the confinement fields now (without a flush), from what the guest holds.
    fn preset(&mut self) {
        let root = self.npt.as_ref().map_or(0, |n| n.root().get());
        let asid = self.asid.get();
        confine(self.vmcb.bytes_mut(), root, asid, None);
    }

    /// [`Guest::new`] with `npt` set.
    #[must_use]
    pub fn with_npt(asid: Asid, npt: Arc<Npt>) -> Option<Self> {
        let mut g = Self::new(asid)?;
        g.set_npt(npt);
        Some(g)
    }

    /// Use `npt` from the next entry on. Every core flushes this guest's ASID on its next
    /// entry: the old table's translations must not survive into the new one.
    pub fn set_npt(&mut self, npt: Arc<Npt>) {
        self.npt = Some(npt);
        self.entered = [0; MASK_CORES / 64];
        self.preset();
    }

    /// The table, if set.
    #[must_use]
    pub fn npt(&self) -> Option<&Arc<Npt>> {
        self.npt.as_ref()
    }

    /// The ASID's number.
    #[must_use]
    pub const fn asid(&self) -> u32 {
        self.asid.get()
    }

    /// The VMCB, typed.
    pub fn vmcb(&mut self) -> Vmcb<'_> {
        Vmcb::new(self.vmcb.bytes_mut())
    }

    /// The VMCB's raw bytes (fields [`Vmcb`] has no setter for).
    pub fn page_mut(&mut self) -> &mut [u8; VMCB_SIZE] {
        self.vmcb.bytes_mut()
    }

    /// Enter the guest once with its whole register file: confine the VMCB (module header),
    /// `VMSAVE` the host into `host`, load `regs`, `VMLOAD` + `VMRUN` the guest, and on the way
    /// out store its registers into `regs`, `VMSAVE` it into its VMCB and `VMLOAD` the host
    /// back. Returns with GIF set.
    ///
    /// The CPU must have `EFER.SVME` set on this core (`crate::enable`); where it does not,
    /// the first SVM instruction raises `#UD`, a kernel fault in the open, not a corruption.
    ///
    /// # Errors
    /// [`Refused`], before anything is touched.
    pub fn run(&mut self, host: &mut HostState, regs: &mut GuestRegs) -> Result<(), Refused> {
        let (np, flush) = svm_features();
        if !np {
            return Err(Refused::NoNestedPaging);
        }
        let root = self.npt.as_ref().ok_or(Refused::NoNpt)?.root().get();
        crate::clgi();
        // GIF is clear from here to the STGI in `enter`: no interrupt is taken, so this task
        // cannot be moved to another core between choosing the flush and the VMRUN.
        let core = akuma_cpu::percpu::core_id() as usize;
        let (w, b) = (core / 64, core % 64);
        let first = core >= MASK_CORES || self.entered[w] & (1 << b) == 0;
        let asid = self.asid.get();
        let page = self.vmcb.bytes_mut();
        let asked_tlb = page[ctrl::GUEST_ASID + 4];
        confine(page, root, asid, first.then_some(flush));
        if core < MASK_CORES {
            self.entered[w] |= 1 << b;
        }
        host.save();
        enter(self.vmcb.pa(), host, regs);
        if first {
            // The caller's own choice applies again from the next entry.
            self.vmcb.bytes_mut()[ctrl::GUEST_ASID + 4] = asked_tlb;
        }
        Ok(())
    }
}

/// Rewrite the fields that bound what the guest can reach (module header).
fn confine(page: &mut [u8; VMCB_SIZE], npt_root: u64, asid: u32, flush: Option<u8>) {
    let i0 = rd32(page, ctrl::INSTR_INTERCEPT0);
    wr32(page, ctrl::INSTR_INTERCEPT0, i0 | intr0::IOIO_PROT | intr0::MSR_PROT | intr0::SHUTDOWN);
    let i1 = rd32(page, ctrl::INSTR_INTERCEPT1);
    let must1 = intr1::VMRUN | intr1::VMLOAD | intr1::VMSAVE | intr1::STGI | intr1::CLGI | intr1::SKINIT | intr1::XSETBV;
    wr32(page, ctrl::INSTR_INTERCEPT1, i1 | must1);
    wr64(page, ctrl::IOPM_BASE, image_pa(&IOPM_ALL));
    wr64(page, ctrl::MSRPM_BASE, image_pa(&MSRPM_ALL));
    wr32(page, ctrl::GUEST_ASID, asid);
    if let Some(f) = flush {
        page[ctrl::GUEST_ASID + 4] = f;
    }
    let vi = rd64(page, ctrl::V_INTR);
    wr64(page, ctrl::V_INTR, (vi | ctrl::V_INTR_MASKING) & !(V_INTR_AVIC | V_INTR_X2AVIC));
    wr64(page, ctrl::NP_ENABLE, 1);
    wr64(page, ctrl::N_CR3, npt_root);
    let (off, bit) = VIRT_VMLOAD_VMSAVE;
    let lbr = rd32(page, off);
    wr32(page, off, lbr & !bit);
    wr32(page, ctrl::VMCB_CLEAN, 0);
}

const _: () = {
    assert!(core::mem::size_of::<Msrpm>() == msrpm::SIZE);
    assert!(core::mem::size_of::<Iopm>() == iopm::SIZE);
};
