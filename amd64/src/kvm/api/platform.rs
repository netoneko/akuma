//! What the API VM's guest sees in-kernel: the 8254, the 8259 pair and the local APIC (what
//! `CREATE_IRQCHIP`/`CREATE_PIT2` ask for), the remembered MSRs, and the rule for which ports
//! belong here. Every other port and every other MMIO address is the VMM's (an exit to userspace).
//!
//! This is the `kvm0` platform (`kvm/linux.rs::Platform`) minus what the VMM emulates itself (the
//! 16550, the keyboard controller, the virtio devices). The interrupt arithmetic is the same and is
//! copied, not shared: `Platform` is welded to the console banner matcher and the `kvm0` guests.

use akuma_svm_amd64 as svm;
use akuma_svm_amd64::devices::MsrStore;
use akuma_svm_amd64::lapic::{self, Lapic};
use akuma_svm_amd64::pic::PicPair;
use akuma_svm_amd64::pit::{self, Pit};
use akuma_svm_amd64::{Vmcb, save};

use super::MAX_VCPUS;
use crate::kvm::linux::{
    APIC_BASE_ADDR, APIC_BASE_BSP, APIC_BASE_ENABLE, MSR_APIC_BASE, MSR_CSTAR, MSR_EFER, MSR_FS_BASE, MSR_GS_BASE,
    MSR_KERNEL_GS_BASE, MSR_LSTAR, MSR_PAT, MSR_SFMASK, MSR_STAR, MSR_SYSENTER_CS, MSR_SYSENTER_EIP, MSR_SYSENTER_ESP,
    MSR_TSC,
};

/// Where an interrupt offered to the guest came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Pic,
    Lapic,
}

#[derive(Clone, Copy)]
pub(super) struct Offer {
    pub vector: u8,
    pub source: Source,
}

pub(super) struct Platform {
    pit: Pit,
    pic: PicPair,
    lapics: [Lapic; MAX_VCPUS],
    apic_bases: [u64; MAX_VCPUS],
    msrs: [MsrStore<48>; MAX_VCPUS],
}

impl Platform {
    pub(super) fn new() -> Self {
        let mut lapics = [const { Lapic::new() }; MAX_VCPUS];
        let mut i = 0;
        while i < MAX_VCPUS {
            lapics[i] = Lapic::new().with_id(i as u8);
            i += 1;
        }
        // What firmware leaves behind: the APIC enabled at its default base, processor 0 the BSP.
        let mut apic_bases = [lapic::DEFAULT_BASE | APIC_BASE_ENABLE; MAX_VCPUS];
        apic_bases[0] |= APIC_BASE_BSP;
        Self { pit: Pit::new(), pic: PicPair::new(), lapics, apic_bases, msrs: [const { MsrStore::new() }; MAX_VCPUS] }
    }

    pub(super) fn lapic(&mut self, cpu: usize) -> &mut Lapic {
        &mut self.lapics[cpu]
    }

    pub(super) fn set_apic_base(&mut self, cpu: usize, v: u64) {
        self.apic_bases[cpu] = v;
    }

    pub(super) fn apic_base(&self, cpu: usize) -> u64 {
        self.apic_bases[cpu]
    }

    /// Is processor `cpu`'s local APIC on, and where does its page sit?
    pub(super) fn lapic_page(&self, cpu: usize) -> Option<u64> {
        let base = self.apic_bases[cpu];
        (base & APIC_BASE_ENABLE != 0).then_some(base & APIC_BASE_ADDR)
    }

    /// Raise legacy line `irq` on the PIC (a VMM's interrupt eventfd, the PIT's channel 0).
    pub(super) fn raise_irq(&mut self, irq: u8) {
        self.pic.raise(irq);
    }

    /// Bring every source `cpu` sees up to `now_us` and say which vector, if any, it is offered.
    pub(super) fn offer(&mut self, cpu: usize, now_us: u64) -> Option<Offer> {
        if cpu == 0 && self.pit.take_irq0(pit::ticks_from_us(now_us)) {
            self.pic.raise(0);
        }
        if self.lapic_page(cpu).is_some() {
            let lapic = &mut self.lapics[cpu];
            lapic.poll(lapic::bus_ticks_from_us(now_us));
            if let Some(vector) = lapic.pending() {
                return Some(Offer { vector, source: Source::Lapic });
            }
            // The virtual wire: with LINT0 in ExtINT mode the PIC's output reaches the processor.
            if cpu == 0 && lapic.lint0_extint() {
                return self.pic.pending().map(|vector| Offer { vector, source: Source::Pic });
            }
            return None;
        }
        if cpu == 0 { self.pic.pending().map(|vector| Offer { vector, source: Source::Pic }) } else { None }
    }

    /// `cpu` took `offer`: the INTA cycle. False if the PIC acknowledged a different vector.
    pub(super) fn accept(&mut self, cpu: usize, offer: Offer) -> bool {
        match offer.source {
            Source::Pic => self.pic.acknowledge() == Some(offer.vector),
            Source::Lapic => {
                self.lapics[cpu].acknowledge(offer.vector);
                true
            }
        }
    }

    /// The host `uptime_us` at which `cpu`'s earliest timer next has something to deliver.
    pub(super) fn next_event_us(&self, cpu: usize) -> Option<u64> {
        let pit = if cpu == 0 { self.pit.next_irq0_tick().map(pit::us_from_ticks) } else { None };
        if self.lapic_page(cpu).is_none() {
            return pit;
        }
        let lapic = &self.lapics[cpu];
        let per_us = lapic::bus_ticks_from_us(1);
        let apic = lapic.next_event().map(|t| t.div_ceil(per_us));
        match (apic, if lapic.lint0_extint() { pit } else { None }) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Could an interrupt ever reach `cpu`, halted with `IF` set?
    pub(super) fn can_wake(&self, cpu: usize) -> bool {
        let pit = cpu == 0 && self.pit.next_irq0_tick().is_some();
        if self.lapic_page(cpu).is_none() {
            return pit;
        }
        let lapic = &self.lapics[cpu];
        lapic.next_event().is_some() || (lapic.lint0_extint() && pit)
    }

    /// `IN` from a port this platform owns; `None` is "the VMM's".
    pub(super) fn io_in(&mut self, port: u16, now_ticks: u64) -> Option<u32> {
        Some(match port {
            0x20 | 0x21 | 0xA0 | 0xA1 => u32::from(self.pic.read(port).unwrap_or(0xFF)),
            0x4D0 | 0x4D1 => 0,
            0x40..=0x43 => u32::from(self.pit.read((port - 0x40) as u8, now_ticks)),
            0x61 => u32::from(self.pit.port_b_read(now_ticks)),
            _ => return None,
        })
    }

    /// `OUT` to a port this platform owns; false is "the VMM's".
    pub(super) fn io_out(&mut self, port: u16, val: u32, now_ticks: u64) -> bool {
        match port {
            0x20 | 0x21 | 0xA0 | 0xA1 => {
                self.pic.write(port, val as u8);
            }
            0x4D0 | 0x4D1 => {}
            0x40..=0x43 => self.pit.write((port - 0x40) as u8, val as u8, now_ticks),
            0x61 => self.pit.port_b_write(val as u8, now_ticks),
            _ => return false,
        }
        true
    }

    /// `RDMSR`: the VMCB-backed registers read their save-area field; the rest read what was last
    /// written, or zero.
    pub(super) fn rdmsr(&self, cpu: usize, v: &Vmcb<'_>, msr: u32) -> u64 {
        match msr {
            MSR_EFER => v.efer() & !svm::msr::EFER_SVME,
            MSR_STAR => v.save64(save::STAR),
            MSR_LSTAR => v.save64(save::LSTAR),
            MSR_CSTAR => v.save64(save::CSTAR),
            MSR_SFMASK => v.save64(save::SFMASK),
            MSR_FS_BASE => v.seg(save::FS).base,
            MSR_GS_BASE => v.seg(save::GS).base,
            MSR_KERNEL_GS_BASE => v.save64(save::KERNEL_GS_BASE),
            MSR_SYSENTER_CS => v.save64(save::SYSENTER_CS),
            MSR_SYSENTER_ESP => v.save64(save::SYSENTER_ESP),
            MSR_SYSENTER_EIP => v.save64(save::SYSENTER_EIP),
            MSR_PAT => v.save64(save::G_PAT),
            MSR_TSC => akuma_cpu::tsc::rdtsc(),
            MSR_APIC_BASE => self.apic_bases[cpu],
            _ => self.msrs[cpu].get(msr).unwrap_or(0),
        }
    }

    /// `WRMSR`. False if the remembered-MSR table is full.
    pub(super) fn wrmsr(&mut self, cpu: usize, v: &mut Vmcb<'_>, msr: u32, val: u64) -> bool {
        match msr {
            MSR_EFER => {
                // LMA is the hardware's to set; SVME must stay on or VMRUN refuses the guest.
                let keep = v.efer() & svm::msr::EFER_LMA;
                v.set_efer((val & !svm::msr::EFER_LMA) | keep | svm::msr::EFER_SVME);
            }
            MSR_STAR => v.set_save64(save::STAR, val),
            MSR_LSTAR => v.set_save64(save::LSTAR, val),
            MSR_CSTAR => v.set_save64(save::CSTAR, val),
            MSR_SFMASK => v.set_save64(save::SFMASK, val),
            MSR_FS_BASE => {
                let mut s = v.seg(save::FS);
                s.base = val;
                v.set_seg(save::FS, s);
            }
            MSR_GS_BASE => {
                let mut s = v.seg(save::GS);
                s.base = val;
                v.set_seg(save::GS, s);
            }
            MSR_KERNEL_GS_BASE => v.set_save64(save::KERNEL_GS_BASE, val),
            MSR_SYSENTER_CS => v.set_save64(save::SYSENTER_CS, val),
            MSR_SYSENTER_ESP => v.set_save64(save::SYSENTER_ESP, val),
            MSR_SYSENTER_EIP => v.set_save64(save::SYSENTER_EIP, val),
            MSR_PAT => v.set_save64(save::G_PAT, val),
            // Base and enable are the guest's; the BSP flag is ours.
            MSR_APIC_BASE => {
                let bsp = if cpu == 0 { APIC_BASE_BSP } else { 0 };
                self.apic_bases[cpu] = (val & (APIC_BASE_ADDR | APIC_BASE_ENABLE)) | bsp;
            }
            _ => return self.msrs[cpu].set(msr, val),
        }
        true
    }
}
