//! The Linux rung: a real `vmlinux`, entered the PVH way, run until it stops.
//!
//! Everything earlier rungs proved composes here: a guest entered 32-bit,
//! unpaged, with EBX = `hvm_start_info` (the PVH rung), under two-level
//! translation (the two-level rung), with the permission maps armed (the
//! trivial rung). What is new is the *payload* — an ELF kernel streamed off the
//! root filesystem into guest RAM — and the thing a payload needs that a
//! dozen-byte test guest never did: **a platform**. The guest asks the
//! hypervisor questions (`CPUID`), touches registers the hypervisor owns
//! (MSRs) and talks to hardware that is not there (port I/O); every one of
//! those is an intercepted exit, and this module is the answers.
//!
//! Spec-first, like the rest: the exit-info layouts, the MSR-backed save-area
//! offsets and the CPUID bit positions are transcribed from APM Vol 2/3 and
//! pinned by `akuma-svm-amd64`'s host tests. No Linux or KVM source was
//! consulted (GPL); what the guest does is learned from its exit trace.
//!
//! # Register file
//!
//! `VMRUN` saves only RSP/RAX/RIP/RFLAGS of the host; every other GPR is
//! shared. A test guest that touches two registers can round-trip them by
//! hand (`kvm::vmrun`); Linux uses all of them, so [`vmrun_regs`] loads the
//! guest's whole register file before `VMRUN` and stores it straight after,
//! before any Rust runs. The state `VMRUN` does *not* swap (FS/GS/TR/LDTR,
//! `KernelGsBase`, `STAR`/`LSTAR`/`CSTAR`/`SFMASK`, `SYSENTER_*`) rides
//! `VMLOAD`/`VMSAVE` — guest in, guest out, host back — which is what lets the
//! kernel's per-CPU `%gs` survive a guest that sets its own.
//!
//! # What is deliberately not here yet
//!
//! No interrupt injection, no timer, no local APIC, no PCI. The command line
//! says so (`noapic nolapic nosmp noacpi`), and a guest that waits for a tick
//! stops the run at `HLT` — which is a finding, not a failure of the loader.
//! The rung's pass is the kernel's banner arriving on the emulated COM1.

use super::{crumb, dec, dump_exit, dump_state, enable_on_core, hx, image_pa, next_rip, p, Iopm, Msrpm};
use crate::phys::FreshFrame;
use akuma_svm_amd64 as svm;
use akuma_svm_amd64::devices::{MsrStore, Uart};
use akuma_svm_amd64::pic::PicPair;
use akuma_svm_amd64::pit::{self, Pit};
use akuma_svm_amd64::{ctrl, elf, eventinj, excp, exitinfo, intr0, intr1, pvh, save, tables, ExitCode, Seg, SegAttr, Vmcb};
use core::arch::x86_64::__cpuid_count;

/// Where the guest kernel is read from (an uncompressed, PVH-capable ELF —
/// Firecracker's CI `vmlinux-6.1.128` is one).
const LINUX_PATH: &str = "/boot/vmlinux";
/// What the guest is told. No APIC, no ACPI, no SMP, no PCI: none of them is
/// emulated; and the console is the one device that is.
const LINUX_CMDLINE: &str =
    "console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr noapic nolapic nosmp noacpi pci=off mitigations=off panic=-1";
/// Guest RAM: contiguous host frames mapped 1:1-by-offset through 2 MiB NPT
/// leaves. The kernel's text loads at 16 MiB and runs to ~40 MiB; the rest is
/// what it has to play with.
const RAM_BYTES: u64 = 128 << 20;
const RAM_ALIGN: u64 = 2 << 20;
/// Exits to serve before giving up — a runaway guest must not hold the boot.
const MAX_EXITS: u64 = 4_000_000;
/// The first this-many exits of each interesting kind are printed whole.
const LOG_FIRST: u32 = 40;
/// Wall-clock budget for the whole run, and for how long the console may stay
/// silent once the banner has been seen. A guest that spins waiting for a timer
/// nobody emulates must not hold the boot — least of all on metal.
const RUN_BUDGET_US: u64 = 120_000_000;
const QUIET_BUDGET_US: u64 = 20_000_000;
/// ASID: unique among the rungs (ladder 1-13, two-level 14, pvh 15 under
/// QEMU's 16); the first entry flushes, so it cannot see a previous rung's
/// stale translations.
const ASID: u32 = 1;

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_CSTAR: u32 = 0xC000_0083;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_SYSENTER_CS: u32 = 0x174;
const MSR_SYSENTER_ESP: u32 = 0x175;
const MSR_SYSENTER_EIP: u32 = 0x176;
const MSR_PAT: u32 = 0x277;
const MSR_TSC: u32 = 0x10;
const MSR_APIC_BASE: u32 = 0x1B;

/// The guest's general registers: the ones `VMRUN` does not carry (RAX, RSP,
/// RIP and RFLAGS live in the VMCB). Field order is the asm's offsets.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GuestRegs {
    rbx: u64, // 0x00
    rcx: u64, // 0x08
    rdx: u64, // 0x10
    rsi: u64, // 0x18
    rdi: u64, // 0x20
    rbp: u64, // 0x28
    r8: u64,  // 0x30
    r9: u64,  // 0x38
    r10: u64, // 0x40
    r11: u64, // 0x48
    r12: u64, // 0x50
    r13: u64, // 0x58
    r14: u64, // 0x60
    r15: u64, // 0x68
}

/// Enter the guest once: load its register file, `VMLOAD` its FS/GS/TR/LDTR
/// and syscall MSRs, `VMRUN`, and on the way out store its registers,
/// `VMSAVE` its state and `VMLOAD` the host's back.
///
/// # Safety
/// `vmcb_pa` must be the 4 KiB-aligned physical address of a fully initialized
/// VMCB held alive by the caller; `host_pa` that of a VMCB-format page the
/// caller has already `VMSAVE`d the host's own FS/GS/TR/LDTR/MSR state into
/// (it is `VMLOAD`ed after every exit — a stale one points the kernel's `%gs`
/// at nothing); and the core must have `EFER.SVME` and `VM_HSAVE_PA` set as for
/// [`super::vmrun`]. Returns with GIF set (`stgi`).
#[inline(never)]
unsafe fn vmrun_regs(vmcb_pa: u64, host_pa: u64, regs: &mut GuestRegs) {
    // SAFETY: per the contract above. The stack pushes are balanced (so no
    // `nostack`); RBX and RBP cannot be operands and are saved by hand; every
    // other GPR is declared clobbered, which is the shared-register contract;
    // RSP is the host's again at #VMEXIT, so `[rsp]`/`[rsp+8]` are intact.
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
            "clgi",
            "vmload rax",
            "vmrun rax",
            "vmsave rax",
            // RAX is the VMCB PA again (the host's own); the guest's
            // registers are all still in the file. Spill them.
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
            inout("rax") vmcb_pa => _,
            inout("rsi") host_pa => _,
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

/// Guest RAM: one contiguous run of host frames, 2-MiB-aligned inside the
/// allocation, so guest-physical `g` is host-physical `base + g`.
struct Ram {
    base: u64,
    alloc_pa: usize,
    alloc_pages: usize,
}

impl Ram {
    fn alloc() -> Option<Self> {
        let alloc_pages = ((RAM_BYTES + RAM_ALIGN) / 4096) as usize;
        let alloc_pa = akuma_pmm::alloc_pages_contiguous_zeroed(alloc_pages)?;
        let base = (alloc_pa as u64 + RAM_ALIGN - 1) & !(RAM_ALIGN - 1);
        Some(Self { base, alloc_pa, alloc_pages })
    }

    /// Copy `src` into guest-physical `gpa`. False if it leaves RAM.
    fn write(&self, gpa: u64, src: &[u8]) -> bool {
        gpa.checked_add(src.len() as u64).is_some_and(|end| end <= RAM_BYTES)
            && akuma_mmu::copy_to_phys((self.base + gpa) as usize, src)
    }

    /// Copy guest-physical `gpa` out. False if it leaves RAM.
    fn copy_out(&self, gpa: u64, dst: &mut [u8]) -> bool {
        gpa.checked_add(dst.len() as u64).is_some_and(|end| end <= RAM_BYTES)
            && akuma_mmu::copy_from_phys((self.base + gpa) as usize, dst)
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        akuma_pmm::free_pages_contiguous(self.alloc_pa, self.alloc_pages);
    }
}

/// The nested page table: PML4 -> PDPT -> PD of 2 MiB leaves, covering exactly
/// guest RAM. Everything else (the local APIC, the I/O APIC, ROMs, the
/// framebuffer) is absent — a guest that touches it takes a nested page fault
/// and the run stops there, naming the address.
struct Npt {
    pml4: FreshFrame,
    _pdpt: FreshFrame,
    _pd: FreshFrame,
}

fn build_npt(ram: &Ram) -> Option<Npt> {
    let mut pml4 = FreshFrame::alloc_zeroed()?;
    let mut pdpt = FreshFrame::alloc_zeroed()?;
    let mut pd = FreshFrame::alloc_zeroed()?;
    svm::wr64(pml4.bytes_mut(), 0, svm::npt::pointer(pdpt.pa() as u64));
    svm::wr64(pdpt.bytes_mut(), 0, svm::npt::pointer(pd.pa() as u64));
    for i in 0..(RAM_BYTES >> 21) {
        svm::wr64(pd.bytes_mut(), i as usize * 8, svm::npt::leaf_2m(ram.base + (i << 21)));
    }
    Some(Npt { pml4, _pdpt: pdpt, _pd: pd })
}

/// What the loader found.
struct Loaded {
    entry: u32,
    segments: usize,
    bytes: u64,
}

fn zero_segment() -> elf::Segment {
    elf::Segment { offset: 0, pa: 0, filesz: 0, memsz: 0 }
}

/// Stream `LINUX_PATH`'s `PT_LOAD` segments into guest RAM at their physical
/// addresses and find the PVH entry. `chunk` is the read window (one frame —
/// no kernel-heap copy of a 40 MB image, the exec loader's own rule).
fn load_vmlinux(ram: &Ram, chunk: &mut FreshFrame) -> Result<Loaded, &'static str> {
    const MAX_SEGS: usize = 8;
    let mut loads = [zero_segment(); MAX_SEGS];
    let mut notes = [zero_segment(); MAX_SEGS];
    let (mut nloads, mut nnotes) = (0usize, 0usize);
    {
        let buf = chunk.bytes_mut();
        let got = akuma_vfs_glue::read_at(LINUX_PATH, 0, &mut buf[..]).map_err(|_| "cannot read /boot/vmlinux")?;
        let image = elf::Image::parse(&buf[..got]).ok_or("not an ELF64 little-endian image")?;
        for s in image.segments() {
            if nloads == MAX_SEGS {
                return Err("more PT_LOAD segments than the loader takes");
            }
            loads[nloads] = s;
            nloads += 1;
        }
        for s in image.note_segments() {
            if nnotes < MAX_SEGS {
                notes[nnotes] = s;
                nnotes += 1;
            }
        }
    }
    if nloads == 0 {
        return Err("no PT_LOAD segments");
    }

    // The entry: the note segment sits in the middle of the file, so read just
    // it (a few hundred bytes).
    let mut entry = None;
    for s in &notes[..nnotes] {
        if s.filesz == 0 || s.filesz > 4096 {
            continue;
        }
        let buf = chunk.bytes_mut();
        let want = s.filesz as usize;
        let got = akuma_vfs_glue::read_at(LINUX_PATH, s.offset as usize, &mut buf[..want])
            .map_err(|_| "cannot read the note segment")?;
        if let Some(e) = elf::find_pvh_entry(&buf[..got]) {
            entry = Some(e);
            break;
        }
    }
    let entry = entry.ok_or("no PVH entry note (a bzImage, or a kernel built without CONFIG_PVH)")?;

    // Leave the top of RAM to the handoff pages.
    let limit = RAM_BYTES - 0x4000;
    let mut bytes = 0u64;
    for s in &loads[..nloads] {
        let end = s.pa.checked_add(s.memsz).ok_or("segment wraps")?;
        if end > limit {
            return Err("a segment does not fit the guest's RAM");
        }
        let mut done = 0u64;
        while done < s.filesz {
            let want = (s.filesz - done).min(4096) as usize;
            let buf = chunk.bytes_mut();
            let got = akuma_vfs_glue::read_at(LINUX_PATH, (s.offset + done) as usize, &mut buf[..want])
                .map_err(|_| "read error inside a segment")?;
            if got == 0 {
                return Err("image is shorter than its program headers say");
            }
            if !ram.write(s.pa + done, &buf[..got]) {
                return Err("copy into guest RAM failed");
            }
            done += got as u64;
        }
        bytes += s.filesz;
    }
    Ok(Loaded { entry, segments: nloads, bytes })
}

/// The platform the guest sees: one 16550, a CMOS that reads zero, masked PICs,
/// an empty PCI bus, no keyboard controller — and a table of MSRs that merely
/// remember what they were given.
struct Platform {
    uart: Uart,
    /// The 8254: channel 2 calibrates the guest's TSC, channel 0 is IRQ0.
    pit: Pit,
    /// The two 8259s: with no APIC they are the whole interrupt fabric.
    pic: PicPair,
    msrs: MsrStore<48>,
    cmos_index: u8,
    /// How much of "Linux version" the console has spelled so far.
    banner: usize,
    banner_seen: bool,
    tx_bytes: u64,
    /// `uptime_us` at the last console byte.
    last_tx_us: u64,
    /// Ports / MSRs answered by default, each logged the first time.
    seen_io: [u16; 32],
    n_io: usize,
    /// The guest pulsed the keyboard controller's reset line (`0xFE` to `0x64`).
    reset_requested: bool,
    seen_msr: [u32; 48],
    n_msr: usize,
}

const BANNER: &[u8] = b"Linux version";

impl Platform {
    const fn new() -> Self {
        Self {
            uart: Uart::new(),
            pit: Pit::new(),
            pic: PicPair::new(),
            msrs: MsrStore::new(),
            cmos_index: 0,
            banner: 0,
            banner_seen: false,
            tx_bytes: 0,
            last_tx_us: 0,
            seen_io: [0; 32],
            n_io: 0,
            reset_requested: false,
            seen_msr: [0; 48],
            n_msr: 0,
        }
    }

    /// One byte of the guest's console: echoed to the host serial, and fed to
    /// the banner matcher.
    fn console(&mut self, b: u8) {
        self.tx_bytes += 1;
        self.last_tx_us = crate::net::uptime_us();
        let one = [b];
        p(core::str::from_utf8(&one).unwrap_or("?"));
        if self.banner_seen {
            return;
        }
        if b == BANNER[self.banner] {
            self.banner += 1;
            self.banner_seen = self.banner == BANNER.len();
        } else {
            self.banner = usize::from(b == BANNER[0]);
        }
    }

    fn note_io(&mut self, port: u16, write: bool, val: u32) {
        if self.seen_io[..self.n_io].contains(&port) {
            return;
        }
        if self.n_io < self.seen_io.len() {
            self.seen_io[self.n_io] = port;
            self.n_io += 1;
        }
        p("\n[KVM] linux: io ");
        p(if write { "out " } else { "in  " });
        hx(u64::from(port), 4);
        if write {
            p(" <- ");
            hx(u64::from(val), 8);
        }
        p(" (default)\n");
    }

    fn io_in(&mut self, port: u16, now: u64) -> u32 {
        match port {
            0x3F8..=0x3FF => u32::from(self.uart.read((port - 0x3F8) as u8)),
            // CMOS data port: every register reads zero (status A's
            // update-in-progress bit stays clear, so a wait-for-RTC loop ends).
            0x71 => 0,
            // PIC.
            0x20 | 0x21 | 0xA0 | 0xA1 => u32::from(self.pic.read(port).unwrap_or(0xFF)),
            // PIT, and system control port B (channel 2's gate and output).
            0x40..=0x43 => u32::from(self.pit.read((port - 0x40) as u8, now)),
            0x61 => u32::from(self.pit.port_b_read(now)),
            // PCI config: no devices.
            0xCF8 => 0,
            0xCFC..=0xCFF => 0xFFFF_FFFF,
            // i8042 / VGA: nothing there.
            0x60 | 0x64 | 0x3C0..=0x3DF => 0xFF,
            _ => {
                self.note_io(port, false, 0);
                0xFFFF_FFFF
            }
        }
    }

    fn io_out(&mut self, port: u16, val: u32, now: u64) {
        match port {
            0x3F8..=0x3FF => {
                if let Some(c) = self.uart.write((port - 0x3F8) as u8, val as u8) {
                    self.console(c);
                }
            }
            0x70 => self.cmos_index = val as u8 & 0x7F,
            // The keyboard controller's CPU-reset pulse: how a PC reboots, and
            // what `panic=-1` ends in.
            0x64 if val as u8 == 0xFE => self.reset_requested = true,
            0x20 | 0x21 | 0xA0 | 0xA1 => {
                self.pic.write(port, val as u8);
            }
            0x40..=0x43 => self.pit.write((port - 0x40) as u8, val as u8, now),
            0x61 => self.pit.port_b_write(val as u8, now),
            // POST, CMOS data, PCI address/data, VGA: accepted.
            0x80 | 0x71 | 0xCF8 | 0xCFC..=0xCFF | 0x3C0..=0x3DF => {}
            _ => self.note_io(port, true, val),
        }
    }

    fn note_msr(&mut self, msr: u32, write: bool, val: u64) {
        if self.seen_msr[..self.n_msr].contains(&msr) {
            return;
        }
        if self.n_msr < self.seen_msr.len() {
            self.seen_msr[self.n_msr] = msr;
            self.n_msr += 1;
        }
        p("\n[KVM] linux: msr ");
        p(if write { "wr " } else { "rd " });
        hx(u64::from(msr), 8);
        if write {
            p(" <- ");
            hx(val, 16);
        }
        p(" (default)\n");
    }

    /// `RDMSR`: the VMCB-backed registers read their save-area field; the rest
    /// read what the guest last wrote, or zero.
    fn rdmsr(&mut self, v: &Vmcb<'_>, msr: u32) -> u64 {
        match msr {
            // The guest's EFER without the SVME the VMCB must carry.
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
            // The host's time-stamp counter, as `RDTSC` already returns it.
            MSR_TSC => akuma_cpu::tsc::rdtsc(),
            // Base 0xFEE0_0000, BSP, global enable clear: a local APIC that is off.
            MSR_APIC_BASE => 0xFEE0_0100,
            _ => {
                self.note_msr(msr, false, 0);
                self.msrs.get(msr).unwrap_or(0)
            }
        }
    }

    /// `WRMSR`: the VMCB-backed registers write their save-area field; the rest
    /// are remembered (so a probe reads back) and otherwise ignored.
    fn wrmsr(&mut self, v: &mut Vmcb<'_>, msr: u32, val: u64) {
        match msr {
            MSR_EFER => {
                // LMA is the hardware's to set; SVME must stay on or VMRUN
                // refuses the guest.
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
            _ => {
                self.note_msr(msr, true, val);
                if !self.msrs.set(msr, val) {
                    p("[KVM] linux: MSR store full\n");
                }
            }
        }
    }
}

/// Print an exit in one line: the first [`LOG_FIRST`] of each kind.
struct ExitLog {
    cpuid: u32,
    msr: u32,
    exc: u32,
}

impl tables::PageSource for Ram {
    fn read(&self, table_pa: u64, off: usize) -> u64 {
        let mut b = [0u8; 8];
        if self.copy_out(table_pa + off as u64, &mut b) {
            u64::from_le_bytes(b)
        } else {
            0
        }
    }
}

/// The 16 bytes at the guest's virtual address `va`: translated through the
/// guest's own page tables when it is paging (long mode: four levels from
/// `CR3`), or taken as guest-physical when it is not.
fn guest_code(ram: &Ram, v: &Vmcb<'_>, va: u64) -> Option<[u8; 16]> {
    let paging = v.cr0() & svm::cr0::PG != 0;
    let gpa = if paging {
        // Only the long-mode walk is implemented; a guest paging any other way
        // is not one this rung has seen.
        if v.efer() & svm::msr::EFER_LMA == 0 {
            return None;
        }
        tables::translate(ram, v.cr3() & !0xFFF, va).ok()?
    } else {
        va
    };
    let mut code = [0u8; 16];
    ram.copy_out(gpa, &mut code).then_some(code)
}

/// One serial line: the guest's instruction bytes at `va`, for a post-mortem.
fn dump_code(ram: &Ram, v: &Vmcb<'_>, va: u64) {
    match guest_code(ram, v, va) {
        Some(code) => {
            p("[KVM] linux: code@");
            hx(va, 16);
            p(" ");
            for b in code {
                hx(u64::from(b), 2);
                p(" ");
            }
            p("\n");
        }
        None => {
            p("[KVM] linux: code@");
            hx(va, 16);
            p(" unreadable (not mapped in the guest's tables, or outside RAM)\n");
        }
    }
}

/// Run the Linux rung. Called once SVM is enabled and the root filesystem is
/// mounted (`kvm::late`).
/// [`LINUX_CMDLINE`] followed by ` tsc_early_khz=<khz>` (omitted when the host
/// has no calibrated TSC), assembled in `buf`.
fn cmdline_with_tsc(buf: &mut [u8; 320], khz: u64) -> &str {
    let base = LINUX_CMDLINE.as_bytes();
    buf[..base.len()].copy_from_slice(base);
    let mut n = base.len();
    if khz != 0 {
        let key = b" tsc_early_khz=";
        buf[n..n + key.len()].copy_from_slice(key);
        n += key.len();
        let mut digits = [0u8; 20];
        let (mut i, mut v) = (digits.len(), khz);
        while v > 0 {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        buf[n..n + digits.len() - i].copy_from_slice(&digits[i..]);
        n += digits.len() - i;
    }
    core::str::from_utf8(&buf[..n]).unwrap_or(LINUX_CMDLINE)
}

pub fn run(nrips: bool) {
    crumb(0x51);
    p("[KVM] linux: loading ");
    p(LINUX_PATH);
    p("\n");
    let Some(ram) = Ram::alloc() else {
        p("[KVM] linux: no contiguous frames for guest RAM\n");
        return;
    };
    crumb(0x52);
    let Some(npt) = build_npt(&ram) else {
        p("[KVM] linux: no frames for the NPT\n");
        return;
    };
    let Some(mut scratch) = FreshFrame::alloc_zeroed() else {
        p("[KVM] linux: no scratch frame\n");
        return;
    };
    let loaded = match load_vmlinux(&ram, &mut scratch) {
        Ok(l) => l,
        Err(why) => {
            p("[KVM] linux: load failed: ");
            p(why);
            p("\n");
            return;
        }
    };
    crumb(0x53);
    p("[KVM] linux: loaded ");
    dec(loaded.bytes >> 10);
    p(" KiB in ");
    dec(loaded.segments as u64);
    p(" segments, PVH entry ");
    hx(u64::from(loaded.entry), 8);
    p("\n");

    // The handoff: cmdline, memmap and start_info under the top of RAM, the
    // memmap's one RAM entry stopping short of them.
    let l = pvh::plan(RAM_BYTES, 0, RAM_BYTES - 0x3000);
    scratch.bytes_mut().fill(0);
    // The guest is told the host's TSC rate rather than left to calibrate it
    // through trapped port reads: a poll loop whose every iteration is a
    // VM exit measures the exits, not the PIT (`tsc_early_khz` is a documented
    // kernel parameter). Built on the stack: no allocation.
    let mut cmd = [0u8; 320];
    let cmdline = cmdline_with_tsc(&mut cmd, crate::lapic::tsc_hz() / 1000);
    if !pvh::write_cmdline(scratch.bytes_mut(), cmdline) || !ram.write(l.cmdline_gpa, scratch.bytes_mut()) {
        p("[KVM] linux: cmdline handoff failed\n");
        return;
    }
    scratch.bytes_mut().fill(0);
    pvh::write_memmap(scratch.bytes_mut(), &l);
    let memmap_ok = ram.write(l.memmap_gpa, scratch.bytes_mut());
    scratch.bytes_mut().fill(0);
    pvh::write_start(scratch.bytes_mut(), &l);
    if !memmap_ok || !ram.write(l.start_gpa, scratch.bytes_mut()) {
        p("[KVM] linux: handoff pages failed\n");
        return;
    }

    let (Some(mut vmcb_f), Some(mut host_f)) = (FreshFrame::alloc_zeroed(), FreshFrame::alloc_zeroed()) else {
        p("[KVM] linux: no frame for the VMCB\n");
        return;
    };
    let msrpm = static_msrpm();
    let iopm = static_iopm();

    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR6, 0xFFFF_0FF0);
    svm::wr64(vmcb_f.bytes_mut(), svm::SAVE_AREA_BASE + save::DR7, 0x400);
    {
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        // Everything the guest can ask the hypervisor about is intercepted: CPUID,
        // every port, every MSR, HLT, the host's own interrupts and NMIs (so a
        // timer tick reaches the host), and a triple fault (so a dead guest is a
        // report and not a reset).
        v.set_intr0(
            intr0::INTR | intr0::NMI | intr0::CPUID | intr0::HLT | intr0::MSR_PROT | intr0::IOIO_PROT | intr0::SHUTDOWN,
        );
        v.set_intr1(intr1::VMRUN | intr1::VMMCALL | intr1::XSETBV);
        // #UD, #GP, #PF and #DF are intercepted only to be seen: each is logged
        // and re-injected, so the guest behaves as if they were not. A triple
        // fault names no cause; the exception before it does.
        v.set_excp(excp::UD | excp::GP | excp::PF | excp::DF);
        v.set_iopm_base(image_pa(iopm));
        v.set_msrpm_base(image_pa(msrpm));
        v.set_asid(ASID);
        v.set_tlb_control(ctrl::TLB_FLUSH_ALL);
        v.set_np_enable(true);
        // The host's IF — not the guest's — decides whether a physical interrupt
        // is taken while the guest runs, so the host tick (scheduler, hardware
        // watchdog) is never starved by a guest running with IF clear.
        v.set_v_intr(ctrl::V_INTR_MASKING);
        v.set_n_cr3(npt.pml4.pa() as u64);
        let data = |base: u64| Seg { selector: 0x18, attr: SegAttr::DATA32_RW_DPL0, limit: 0xFFFF_FFFF, base };
        v.set_seg(
            save::CS,
            Seg { selector: 0x10, attr: SegAttr::CODE32_DPL0, limit: 0xFFFF_FFFF, base: 0 },
        );
        v.set_seg(save::ES, data(0));
        v.set_seg(save::SS, data(0));
        v.set_seg(save::DS, data(0));
        v.set_seg(save::FS, data(0));
        v.set_seg(save::GS, data(0));
        // PVH asks for a 32-bit busy TSS in TR: type 0xB, present, small.
        v.set_seg(
            save::TR,
            Seg {
                selector: 0x20,
                attr: SegAttr::from_fields(0xB, false, 0, true, false, false, false, false),
                limit: 0x67,
                base: 0,
            },
        );
        // 32-bit protected mode, paging off: the PVH entry shape.
        v.set_efer(svm::msr::EFER_SVME);
        v.set_cr0(svm::cr0::PE | svm::cr0::NE);
        v.set_cr3(0);
        v.set_cr4(0);
        v.set_rflags(0x2);
        v.set_cpl(0);
        v.set_rip(u64::from(loaded.entry));
        v.set_rsp(0);
        // The architectural reset PAT (WB, WT, UC-, UC twice): used because
        // nested paging is on.
        v.set_save64(save::G_PAT, 0x0007_0406_0007_0406);
    }
    {
        let v = Vmcb::new(vmcb_f.bytes_mut());
        if let Err(e) = svm::validate_guest_state(&v) {
            p("[KVM] linux: guest state rejected: ");
            p(e.name());
            p("\n");
            return;
        }
    }
    crumb(0x54);
    if !enable_on_core(0) {
        p("[KVM] linux: no host save area for the BSP\n");
        return;
    }

    // The host's FS/GS/TR/LDTR/syscall-MSR state, parked in a VMCB-format page
    // that `vmrun_regs` loads back after every exit.
    let host_pa = host_f.pa() as u64;
    let vmcb_pa = vmcb_f.pa() as u64;
    let _ = host_f.bytes_mut();
    // SAFETY: `host_pa` is a frame this function owns, 4-KiB-aligned; VMSAVE
    // writes the 4 KiB state-save layout into it and nothing else.
    unsafe { core::arch::asm!("vmsave rax", in("rax") host_pa, options(nostack)) };

    crumb(0x55);
    p("[KVM] linux: running (start=");
    hx(l.start_gpa, 8);
    p(", ram=");
    dec(RAM_BYTES >> 20);
    p(" MiB)\n");

    let mut regs = GuestRegs { rbx: l.start_gpa, ..GuestRegs::default() };
    let mut plat = Platform::new();
    let mut log = ExitLog { cpuid: 0, msr: 0, exc: 0 };
    let (mut exits, mut n_cpuid, mut n_msr, mut n_io, mut n_irq, mut n_exc) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut stopped = "exit budget spent";
    let mut fatal = false;
    let (mut n_inj, mut n_hlt) = (0u64, 0u64);
    if crate::lapic::tsc_uptime_us().is_none() {
        p("[KVM] linux: warning: host uptime is 10 ms ticks; PIT timing will be coarse\n");
    }

    // IRQs on for the whole run: with V_INTR_MASKING the host IF at VMRUN gates
    // physical interrupts, and a tick must reach the host.
    akuma_cpu::daif::unmask_irq();
    let started_us = crate::net::uptime_us();
    plat.last_tx_us = started_us;
    'run: while exits < MAX_EXITS {
        exits += 1;
        let now_us = crate::net::uptime_us();
        if now_us.saturating_sub(started_us) > RUN_BUDGET_US {
            stopped = "wall-clock budget spent";
            break 'run;
        }
        if plat.banner_seen && now_us.saturating_sub(plat.last_tx_us) > QUIET_BUDGET_US {
            stopped = "guest console quiet after the banner";
            break 'run;
        }
        // The PIT's channel 0 has had an edge since we last looked: IRQ0 is
        // requested. Then offer the PIC's best vector to the guest as a virtual
        // INTR; the processor takes it when the guest's IF allows (§15.21.4)
        // and writes V_IRQ back cleared when it has.
        if plat.pit.take_irq0(pit::ticks_from_us(now_us)) {
            plat.pic.raise(0);
        }
        let offered = plat.pic.pending();
        Vmcb::new(vmcb_f.bytes_mut()).set_v_intr(match offered {
            Some(vec) => ctrl::v_intr_extint(vec),
            None => ctrl::V_INTR_MASKING,
        });
        crumb(0x57);
        // SAFETY: the VMCB and the host save image are live frames this
        // function owns; SVM is enabled on this core (`enable_on_core`).
        unsafe { vmrun_regs(vmcb_pa, host_pa, &mut regs) };
        crumb(0x58);
        let mut v = Vmcb::new(vmcb_f.bytes_mut());
        // The first entry flushed; the rest need not.
        v.set_tlb_control(ctrl::TLB_DO_NOTHING);
        // V_IRQ comes back cleared once the guest has dispatched the offered
        // vector: that is the INTA cycle, and it happens before any exit this
        // run can report, so the PIC is up to date before a port write (the
        // guest's EOI) is looked at.
        if let Some(vec) = offered
            && v.v_intr() & ctrl::V_IRQ == 0
        {
            n_inj += 1;
            if plat.pic.acknowledge() != Some(vec) {
                p("\n[KVM] linux: PIC acknowledged a different vector than it offered\n");
            }
            if n_inj <= LOG_FIRST.into() {
                p("\n[KVM] linux: irq vector ");
                hx(u64::from(vec), 2);
                p(" taken at rip=");
                hx(v.rip(), 16);
                p("\n");
            }
        }
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Cpuid => {
                n_cpuid += 1;
                let (leaf, sub) = (v.rax() as u32, regs.rcx as u32);
                let h = __cpuid_count(leaf, sub);
                let [a, b, c, d] = svm::cpuid::filter(leaf, [h.eax, h.ebx, h.ecx, h.edx]);
                // The hypervisor leaves (0x4000_0000..) are scanned 256 at a time;
                // one line says so, the rest would drown the log.
                if log.cpuid < LOG_FIRST && !(0x4000_0000..0x5000_0000).contains(&leaf) {
                    log.cpuid += 1;
                    p("\n[KVM] linux: cpuid ");
                    hx(u64::from(leaf), 8);
                    p("/");
                    hx(u64::from(sub), 2);
                    p(" -> ");
                    hx(u64::from(a), 8);
                    p(" ");
                    hx(u64::from(b), 8);
                    p(" ");
                    hx(u64::from(c), 8);
                    p(" ");
                    hx(u64::from(d), 8);
                    p("\n");
                }
                v.set_rax(u64::from(a));
                regs.rbx = u64::from(b);
                regs.rcx = u64::from(c);
                regs.rdx = u64::from(d);
                v.set_rip(next_rip(&v, nrips, 2));
            }
            ExitCode::Msr => {
                n_msr += 1;
                let msr = regs.rcx as u32;
                let write = exitinfo::msr_is_write(v.exitinfo1());
                if write {
                    let val = (regs.rdx << 32) | (v.rax() & 0xFFFF_FFFF);
                    if log.msr < LOG_FIRST {
                        log.msr += 1;
                        p("\n[KVM] linux: wrmsr ");
                        hx(u64::from(msr), 8);
                        p(" <- ");
                        hx(val, 16);
                        p("\n");
                    }
                    plat.wrmsr(&mut v, msr, val);
                } else {
                    let val = plat.rdmsr(&v, msr);
                    if log.msr < LOG_FIRST {
                        log.msr += 1;
                        p("\n[KVM] linux: rdmsr ");
                        hx(u64::from(msr), 8);
                        p(" -> ");
                        hx(val, 16);
                        p("\n");
                    }
                    v.set_rax(val & 0xFFFF_FFFF);
                    regs.rdx = val >> 32;
                }
                v.set_rip(next_rip(&v, nrips, 2));
            }
            ExitCode::Ioio => {
                n_io += 1;
                let io = exitinfo::Io::decode(v.exitinfo1());
                if io.string || io.size == 0 {
                    stopped = "string port I/O (INS/OUTS) is not emulated";
                    fatal = true;
                    p("\n[KVM] linux: ");
                    p(stopped);
                    p(" port=");
                    hx(u64::from(io.port), 4);
                    p("\n");
                    break 'run;
                }
                if io.is_in {
                    let val = u64::from(plat.io_in(io.port, pit::ticks_from_us(crate::net::uptime_us()))) & io.mask();
                    let rax = v.rax();
                    v.set_rax(if io.size == 4 { val } else { (rax & !io.mask()) | val });
                } else {
                    plat.io_out(io.port, (v.rax() & io.mask()) as u32, pit::ticks_from_us(crate::net::uptime_us()));
                }
                if plat.reset_requested {
                    v.set_rip(v.exitinfo2());
                    stopped = "guest requested a reset (keyboard-controller pulse: its panic=-1 reboot)";
                    break 'run;
                }
                v.set_rip(v.exitinfo2());
            }
            ExitCode::Hlt => {
                n_hlt += 1;
                if v.rflags() & (1 << 9) == 0 {
                    stopped = "guest halted with interrupts off";
                    break 'run;
                }
                v.set_rip(next_rip(&v, nrips, 1));
                // Idle until the PIT can raise an interrupt the PIC will offer.
                // The host tick keeps running underneath (IRQs are on).
                loop {
                    let now = crate::net::uptime_us();
                    if plat.pit.take_irq0(pit::ticks_from_us(now)) {
                        plat.pic.raise(0);
                    }
                    if plat.pic.pending().is_some() {
                        break;
                    }
                    if plat.pit.next_irq0_tick().is_none() {
                        stopped = "guest halted waiting for an interrupt nothing can raise";
                        break 'run;
                    }
                    if now.saturating_sub(started_us) > RUN_BUDGET_US {
                        stopped = "wall-clock budget spent";
                        break 'run;
                    }
                    core::hint::spin_loop();
                }
            }
            // A host interrupt or NMI arrived while the guest ran. The host's IF
            // is set, so it was taken at the `stgi` that ended the exit; there
            // is nothing to do but resume (and count it).
            ExitCode::Other(0x60 | 0x61) => n_irq += 1,
            ExitCode::Ud | ExitCode::Gp | ExitCode::Pf | ExitCode::Other(0x48) => {
                n_exc += 1;
                let code = v.exitcode();
                let (name, vector, err) = match code {
                    0x46 => ("#UD", 6u8, None),
                    0x4D => ("#GP", 13, Some(v.exitinfo1() as u32)),
                    0x4E => ("#PF", 14, Some(v.exitinfo1() as u32)),
                    _ => ("#DF", 8, Some(0)),
                };
                let during_delivery = v.exitintinfo() & (1 << 31) != 0;
                if log.exc < 30 || during_delivery {
                    log.exc += 1;
                    p("\n[KVM] linux: ");
                    p(name);
                    p(" rip=");
                    hx(v.rip(), 16);
                    if let Some(e) = err {
                        p(" err=");
                        hx(u64::from(e), 8);
                    }
                    if vector == 14 {
                        p(" addr=");
                        hx(v.exitinfo2(), 16);
                    }
                    if during_delivery {
                        p(" exitintinfo=");
                        hx(v.exitintinfo(), 16);
                    }
                    p("\n");
                    dump_code(&ram, &v, v.rip());
                }
                if during_delivery {
                    stopped = "exception while delivering an event";
                    fatal = true;
                    dump_state(&v);
                    break 'run;
                }
                // The intercept is tested before CR2 is written: the guest's CR2
                // is the VMM's to set (§15.12.15).
                if vector == 14 {
                    let addr = v.exitinfo2();
                    v.set_save64(save::CR2, addr);
                }
                v.set_eventinj(eventinj::exception(vector, err));
            }
            ExitCode::Npf => {
                stopped = "nested page fault (guest touched unmapped guest-physical memory)";
                fatal = true;
                p("\n[KVM] linux: ");
                p(stopped);
                p(": gpa=");
                hx(v.exitinfo2(), 16);
                p(" info1=");
                hx(v.exitinfo1(), 16);
                p(" rip=");
                hx(v.rip(), 16);
                p("\n");
                dump_code(&ram, &v, v.rip());
                break 'run;
            }
            ExitCode::Shutdown => {
                stopped = "triple fault";
                fatal = true;
                p("\n[KVM] linux: triple fault: ");
                dump_exit(&v, regs.rcx, regs.rdx);
                dump_state(&v);
                dump_code(&ram, &v, v.rip());
                break 'run;
            }
            _ => {
                stopped = "unexpected exit";
                fatal = true;
                p("\n[KVM] linux: unexpected exit: ");
                dump_exit(&v, regs.rcx, regs.rdx);
                dump_state(&v);
                dump_code(&ram, &v, v.rip());
                break 'run;
            }
        }
    }
    crumb(0x59);

    let v = Vmcb::new(vmcb_f.bytes_mut());
    p("\n[KVM] linux: stopped: ");
    p(stopped);
    p("\n[KVM] linux: exits=");
    dec(exits);
    p(" cpuid=");
    dec(n_cpuid);
    p(" msr=");
    dec(n_msr);
    p(" io=");
    dec(n_io);
    p(" irq=");
    dec(n_irq);
    p(" exc=");
    dec(n_exc);
    p(" injected=");
    dec(n_inj);
    p(" hlt=");
    dec(n_hlt);
    p(" console_bytes=");
    dec(plat.tx_bytes);
    p(" rip=");
    hx(v.rip(), 16);
    p("\n");
    let pass = plat.banner_seen && !fatal;
    p("[KVM] linux guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (banner=");
    p(if plat.banner_seen { "yes" } else { "no" });
    p(")\n");
}

fn static_msrpm() -> &'static Msrpm {
    &MSRPM_ALL
}

fn static_iopm() -> &'static Iopm {
    &IOPM_ALL
}

/// Every MSR intercepted (both bits of every pair set): a guest's `WRMSR` is
/// the hypervisor's to answer, never the host CPU's.
static MSRPM_ALL: Msrpm = Msrpm([0xFF; svm::msrpm::SIZE]);
/// Every port intercepted.
static IOPM_ALL: Iopm = Iopm(svm::iopm::all());
