//! The Linux phase: a real `vmlinux`, entered the PVH way, run until it stops.
//!
//! Everything earlier phases proved composes here: a guest entered 32-bit,
//! unpaged, with EBX = `hvm_start_info` (the PVH phase), under two-level
//! translation (the two-level phase), with the permission maps armed (the
//! trivial phase). What is new is the *payload* — an ELF kernel streamed off the
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
//! The phase's pass is the kernel's banner arriving on the emulated COM1.

use super::{crumb, dec, dump_exit, dump_state, enable_on_core, hx, image_pa, next_rip, p, Iopm, Msrpm};
use crate::phys::FreshFrame;
use akuma_svm_amd64 as svm;
use akuma_svm_amd64::devices::{MsrStore, Uart};
use akuma_svm_amd64::lapic::{self, Lapic};
use akuma_svm_amd64::mmio;
use akuma_svm_amd64::pic::PicPair;
use akuma_svm_amd64::pit::{self, Pit};
use akuma_svm_amd64::virtio::{self, Disk, GuestMem, MmioBlk};
use akuma_svm_amd64::{ctrl, elf, eventinj, excp, exitinfo, intr0, intr1, pvh, save, tables, ExitCode, Seg, SegAttr, Vmcb};
use core::arch::x86_64::__cpuid_count;

/// Where the guest kernel is read from (an uncompressed, PVH-capable ELF —
/// Firecracker's CI `vmlinux-6.1.128` is one).
const LINUX_PATH: &str = "/boot/vmlinux";
/// Where the Akuma guest's kernel is read from: a copy of the amd64 ELF this
/// hypervisor itself runs from (any PVH build of the kernel will do).
const AKUMA_PATH: &str = "/boot/akuma-guest";
/// What the Akuma guest is told. It finds its disk from the command line (a
/// `virtio_mmio.device=` token, the Firecracker spelling), polls it, and has no
/// ACPI, PCI or network. `nosmp`: one virtual CPU. The init is a script on the
/// guest's disk; its arguments are comma-separated because the command line is
/// split on whitespace.
const AKUMA_CMDLINE: &str = "nosmp";
/// Appended when the guest has a disk.
const AKUMA_DISK_ARGS: &str = " virtio_mmio.device=4K@0xd0000000:5 init=/bin/busybox initargs=sh,/etc/guest.sh";
/// The Akuma guest's disk when the machine has no second virtio-blk device.
const AKUMA_DISK_IMAGE_PATH: &str = "/boot/akuma-guest-root.img";
/// The Akuma guest's RAM. The kernel takes a fixed 512 MiB heap
/// (`amd64/src/mem.rs` `HEAP_SIZE`) out of one region and refuses to boot when the
/// region is smaller; the rest is its frame allocator. One page directory of
/// 2 MiB nested leaves maps 1 GiB, which bounds this.
const AKUMA_RAM_BYTES: u64 = 768 << 20;

/// Which kernel the run boots.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Guest {
    /// A Linux `vmlinux`: PIC and PIT, no local APIC, ACPI-described virtio.
    Linux,
    /// The Akuma amd64 kernel itself: local APIC and its timer, virtio from the
    /// command line. The third rung of the ladder (`docs/archive/AKUMA_KVM.md`).
    Akuma,
}

impl Guest {
    const fn path(self) -> &'static str {
        match self {
            Self::Linux => LINUX_PATH,
            Self::Akuma => AKUMA_PATH,
        }
    }

    const fn ram_bytes(self) -> u64 {
        match self {
            Self::Linux => RAM_BYTES,
            Self::Akuma => AKUMA_RAM_BYTES,
        }
    }

    const fn disk_path(self) -> &'static str {
        match self {
            Self::Linux => DISK_IMAGE_PATH,
            Self::Akuma => AKUMA_DISK_IMAGE_PATH,
        }
    }

    /// The first line the guest's console prints: PASS needs it.
    const fn banner(self) -> &'static [u8] {
        match self {
            Self::Linux => b"Linux version",
            Self::Akuma => b"Akuma/amd64",
        }
    }

    /// What the guest's init prints when it has run to the end (Akuma only: a
    /// Linux run ends on the reset pulse its own `reboot` makes).
    const fn done(self) -> Option<&'static [u8]> {
        match self {
            Self::Linux => None,
            Self::Akuma => Some(b"AKUMA-GUEST-DONE"),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Akuma => "akuma",
        }
    }
}
/// What the guest is told. No APIC, no SMP, no PCI: none of them is emulated;
/// the console, the PIT/PIC and (with a disk) one virtio-blk are.
const LINUX_CMDLINE: &str =
    "console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr noapic nolapic nosmp nmi_watchdog=0 pci=off mitigations=off panic=-1";
/// Without a disk there is nothing for ACPI to describe.
const NO_DISK_ARGS: &str = " noacpi";
/// Where the guest's virtio-blk device sits, and the PIC line it raises. The
/// window is above RAM, so the NPT leaves it unmapped and every access is a
/// nested page fault the run loop emulates (`akuma_svm_amd64::{mmio,virtio}`).
const VIRTIO_BASE: u64 = 0xD000_0000;
const VIRTIO_LEN: u64 = 0x1000;
const VIRTIO_IRQ: u8 = 5;
/// COM1's line on the PIC.
const UART_IRQ: u8 = 4;
/// Appended to the guest's command line when it has a disk.
/// The device itself is announced through ACPI (`akuma_svm_amd64::acpi`): the
/// guest kernel is built without `VIRTIO_MMIO_CMDLINE_DEVICES`.
const ROOT_ARGS: &str = " root=/dev/vda rw rootwait";
/// A disk image on the host filesystem, used when the machine has no second
/// virtio-blk device to give the guest.
const DISK_IMAGE_PATH: &str = "/boot/guest-root.img";

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
/// ASID: unique among the phases (ladder 1-13, two-level 14, pvh 15 under
/// QEMU's 16); the first entry flushes, so it cannot see a previous phase's
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
    bytes: u64,
    alloc_pa: usize,
    alloc_pages: usize,
}

impl Ram {
    fn alloc(bytes: u64) -> Option<Self> {
        let alloc_pages = ((bytes + RAM_ALIGN) / 4096) as usize;
        let alloc_pa = akuma_pmm::alloc_pages_contiguous_zeroed(alloc_pages)?;
        let base = (alloc_pa as u64 + RAM_ALIGN - 1) & !(RAM_ALIGN - 1);
        Some(Self { base, bytes, alloc_pa, alloc_pages })
    }

    /// Copy `src` into guest-physical `gpa`. False if it leaves RAM.
    fn write(&self, gpa: u64, src: &[u8]) -> bool {
        gpa.checked_add(src.len() as u64).is_some_and(|end| end <= self.bytes)
            && akuma_mmu::copy_to_phys((self.base + gpa) as usize, src)
    }

    /// Copy guest-physical `gpa` out. False if it leaves RAM.
    fn copy_out(&self, gpa: u64, dst: &mut [u8]) -> bool {
        gpa.checked_add(dst.len() as u64).is_some_and(|end| end <= self.bytes)
            && akuma_mmu::copy_from_phys((self.base + gpa) as usize, dst)
    }
}

impl GuestMem for Ram {
    fn read(&self, gpa: u64, dst: &mut [u8]) -> bool {
        self.copy_out(gpa, dst)
    }

    fn write(&self, gpa: u64, src: &[u8]) -> bool {
        Self::write(self, gpa, src)
    }
}

/// What backs the guest's disk. The host's own boot disk (`vda`) is never
/// offered: a guest with write access to the host's root is not isolated.
#[derive(Clone, Copy)]
enum HostDisk {
    /// Device `idx` of `akuma_virtio::block`: the machine's second virtio-blk,
    /// driven by the same driver the host's root uses.
    Virtio(usize),
    /// A file on the host's filesystem, preallocated.
    File(&'static str),
}

impl HostDisk {
    /// The disk to give the guest, if the machine has one.
    fn pick(file: &'static str) -> Option<Self> {
        if akuma_virtio::block::device_count() > 1 && akuma_virtio::block::device_name(1).is_some() {
            return Some(Self::Virtio(1));
        }
        akuma_vfs_glue::file_size(file).ok().filter(|&n| n >= virtio::SECTOR).map(|_| Self::File(file))
    }

    const fn what(self) -> &'static str {
        match self {
            Self::Virtio(_) => "second virtio-blk device",
            Self::File(path) => path,
        }
    }
}

impl Disk for HostDisk {
    fn sectors(&self) -> u64 {
        match *self {
            Self::Virtio(i) => akuma_virtio::block::with_device_at(i, |d| d.capacity_sectors()).unwrap_or(0),
            Self::File(path) => akuma_vfs_glue::file_size(path).map_or(0, |n| n / virtio::SECTOR),
        }
    }

    fn read(&mut self, offset: u64, dst: &mut [u8]) -> bool {
        match *self {
            Self::Virtio(i) => akuma_virtio::block::read_bytes_at(i, offset, dst).is_ok(),
            Self::File(path) => akuma_vfs_glue::read_at(path, offset as usize, dst).is_ok_and(|n| n == dst.len()),
        }
    }

    fn write(&mut self, offset: u64, src: &[u8]) -> bool {
        match *self {
            Self::Virtio(i) => akuma_virtio::block::write_bytes_at(i, offset, src).is_ok(),
            Self::File(path) => akuma_vfs_glue::write_at(path, offset as usize, src).is_ok_and(|n| n == src.len()),
        }
    }

    fn flush(&mut self) -> bool {
        true
    }
}

/// General register `n` in the instruction encoding's numbering (RAX, RCX, RDX,
/// RBX, RSP, RBP, RSI, RDI, R8-R15): the VMCB holds RAX and RSP, `regs` the rest.
fn get_reg(v: &Vmcb<'_>, r: &GuestRegs, n: u8) -> u64 {
    match n {
        0 => v.rax(),
        1 => r.rcx,
        2 => r.rdx,
        3 => r.rbx,
        4 => v.rsp(),
        5 => r.rbp,
        6 => r.rsi,
        7 => r.rdi,
        8 => r.r8,
        9 => r.r9,
        10 => r.r10,
        11 => r.r11,
        12 => r.r12,
        13 => r.r13,
        14 => r.r14,
        _ => r.r15,
    }
}

fn set_reg(v: &mut Vmcb<'_>, r: &mut GuestRegs, n: u8, val: u64) {
    match n {
        0 => v.set_rax(val),
        1 => r.rcx = val,
        2 => r.rdx = val,
        3 => r.rbx = val,
        4 => v.set_rsp(val),
        5 => r.rbp = val,
        6 => r.rsi = val,
        7 => r.rdi = val,
        8 => r.r8 = val,
        9 => r.r9 = val,
        10 => r.r10 = val,
        11 => r.r11 = val,
        12 => r.r12 = val,
        13 => r.r13 = val,
        14 => r.r14 = val,
        _ => r.r15 = val,
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
    for i in 0..(ram.bytes >> 21) {
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

/// Stream `path`'s `PT_LOAD` segments into guest RAM at their physical
/// addresses and find the PVH entry. `chunk` is the read window (one frame —
/// no kernel-heap copy of a 40 MB image, the exec loader's own rule).
fn load_kernel(path: &str, ram: &Ram, chunk: &mut FreshFrame) -> Result<Loaded, &'static str> {
    const MAX_SEGS: usize = 8;
    let mut loads = [zero_segment(); MAX_SEGS];
    let mut notes = [zero_segment(); MAX_SEGS];
    let (mut nloads, mut nnotes) = (0usize, 0usize);
    {
        let buf = chunk.bytes_mut();
        let got = akuma_vfs_glue::read_at(path, 0, &mut buf[..]).map_err(|_| "cannot read the kernel image")?;
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
        let got = akuma_vfs_glue::read_at(path, s.offset as usize, &mut buf[..want])
            .map_err(|_| "cannot read the note segment")?;
        if let Some(e) = elf::find_pvh_entry(&buf[..got]) {
            entry = Some(e);
            break;
        }
    }
    let entry = entry.ok_or("no PVH entry note (a bzImage, or a kernel built without CONFIG_PVH)")?;

    // Leave the top of RAM to the handoff pages.
    let limit = ram.bytes - 0x4000;
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
            let got = akuma_vfs_glue::read_at(path, (s.offset + done) as usize, &mut buf[..want])
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
    /// The ACPI PM1 registers the FADT points at.
    pm1: svm::acpi::Pm1,
    /// The local APIC, reached through the MMIO page once the guest has set
    /// `IA32_APIC_BASE`'s enable bit. Absent (a Linux guest told `nolapic`
    /// never sets it) the PIC is the whole interrupt fabric.
    lapic: Lapic,
    /// `IA32_APIC_BASE`: base, BSP flag, global enable.
    apic_base: u64,
    msrs: MsrStore<48>,
    cmos_index: u8,
    /// The first console line the guest must print, and (Akuma) the line its
    /// init prints at the end.
    banner: Needle,
    done: Needle,
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

/// A console line being waited for, matched a byte at a time.
struct Needle {
    pat: &'static [u8],
    at: usize,
    seen: bool,
}

impl Needle {
    const fn new(pat: &'static [u8]) -> Self {
        Self { pat, at: 0, seen: false }
    }

    fn feed(&mut self, b: u8) {
        if self.seen || self.pat.is_empty() {
            return;
        }
        if b == self.pat[self.at] {
            self.at += 1;
            self.seen = self.at == self.pat.len();
        } else {
            self.at = usize::from(b == self.pat[0]);
        }
    }
}

/// `IA32_APIC_BASE`: bit 11 global enable, bit 8 BSP, bits 51:12 the base.
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_BSP: u64 = 1 << 8;
const APIC_BASE_ADDR: u64 = 0x000F_FFFF_FFFF_F000;

/// Where an interrupt offered to the guest came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Pic,
    Lapic,
}

/// An interrupt on its way to the guest as a virtual INTR.
#[derive(Clone, Copy)]
struct Offer {
    vector: u8,
    source: Source,
}

impl Platform {
    const fn new(guest: Guest) -> Self {
        Self {
            uart: Uart::new(),
            pit: Pit::new(),
            pic: PicPair::new(),
            pm1: svm::acpi::Pm1::new(),
            lapic: Lapic::new(),
            apic_base: lapic::DEFAULT_BASE | APIC_BASE_BSP,
            msrs: MsrStore::new(),
            cmos_index: 0,
            banner: Needle::new(guest.banner()),
            done: Needle::new(match guest.done() {
                Some(d) => d,
                None => b"",
            }),
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
        self.banner.feed(b);
        self.done.feed(b);
    }

    /// Is the local APIC on, and where does its page sit?
    fn lapic_page(&self) -> Option<u64> {
        (self.apic_base & APIC_BASE_ENABLE != 0).then_some(self.apic_base & APIC_BASE_ADDR)
    }

    /// Bring every interrupt source up to `now_us` and say which vector, if any,
    /// the guest should be offered. With the local APIC on it is the only
    /// source (the PIC reaches the CPU through LINT0, which is not modelled);
    /// without it the PIC is the whole fabric.
    fn offer(&mut self, now_us: u64) -> Option<Offer> {
        if self.pit.take_irq0(pit::ticks_from_us(now_us)) {
            self.pic.raise(0);
        }
        if self.lapic_page().is_some() {
            self.lapic.poll(lapic::bus_ticks_from_us(now_us));
            return self.lapic.pending().map(|vector| Offer { vector, source: Source::Lapic });
        }
        self.pic.pending().map(|vector| Offer { vector, source: Source::Pic })
    }

    /// The guest took `offer` (the processor cleared `V_IRQ`): the INTA cycle.
    /// False if the PIC acknowledged a different vector than it offered.
    fn accept(&mut self, offer: Offer) -> bool {
        match offer.source {
            Source::Pic => self.pic.acknowledge() == Some(offer.vector),
            Source::Lapic => {
                self.lapic.acknowledge(offer.vector);
                true
            }
        }
    }

    /// Could an interrupt ever arrive for a guest halted with `IF` set?
    fn can_wake(&self) -> bool {
        if self.lapic_page().is_some() { self.lapic.next_event().is_some() } else { self.pit.next_irq0_tick().is_some() }
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

    /// Present the UART's interrupt line to the PIC (IRQ4). The 8259 is
    /// edge-triggered and its IRR is sticky, so raising while the level stays
    /// high is the same as raising once.
    fn sync_uart_irq(&mut self) {
        if self.uart.irq_level() {
            self.pic.raise(UART_IRQ);
        }
    }

    fn io_in(&mut self, port: u16, size: u8, now: u64) -> u32 {
        match port {
            0x3F8..=0x3FF => {
                let v = u32::from(self.uart.read_clearing((port - 0x3F8) as u8));
                self.sync_uart_irq();
                v
            }
            // CMOS data port: every register reads zero (status A's
            // update-in-progress bit stays clear, so a wait-for-RTC loop ends).
            0x71 => 0,
            // PIC.
            0x20 | 0x21 | 0xA0 | 0xA1 => u32::from(self.pic.read(port).unwrap_or(0xFF)),
            // ACPI PM1 block (status/enable/control) and the PIC trigger mode
            // registers the ACPI driver reads and writes.
            p if svm::acpi::Pm1::owns(p) => self.pm1.read(p, size),
            0x4D0 | 0x4D1 => 0,
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

    fn io_out(&mut self, port: u16, size: u8, val: u32, now: u64) {
        match port {
            0x3F8..=0x3FF => {
                if let Some(c) = self.uart.write((port - 0x3F8) as u8, val as u8) {
                    self.console(c);
                }
                self.sync_uart_irq();
            }
            0x70 => self.cmos_index = val as u8 & 0x7F,
            // The keyboard controller's CPU-reset pulse: how a PC reboots, and
            // what `panic=-1` ends in.
            0x64 if val as u8 == 0xFE => self.reset_requested = true,
            0x20 | 0x21 | 0xA0 | 0xA1 => {
                self.pic.write(port, val as u8);
            }
            p if svm::acpi::Pm1::owns(p) => self.pm1.write(p, size, val),
            0x4D0 | 0x4D1 => {}
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
            // Base 0xFEE0_0000, BSP; the enable bit is the guest's to set.
            MSR_APIC_BASE => self.apic_base,
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
            // Base and enable are the guest's; the BSP flag is ours.
            MSR_APIC_BASE => self.apic_base = (val & (APIC_BASE_ADDR | APIC_BASE_ENABLE)) | APIC_BASE_BSP,
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
        // is not one this phase has seen.
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

/// A guest store to the virtio window: apply it, and when it rang the doorbell,
/// serve the queue and raise the interrupt line.
#[allow(clippy::too_many_arguments)]
fn mmio_write(
    dev: &mut MmioBlk,
    disk: &mut HostDisk,
    ram: &Ram,
    bounce: &mut [u8],
    pic: &mut PicPair,
    off: u64,
    size: u8,
    val: u64,
) {
    if dev.write(off, size, val) == virtio::Write::Notify(0) {
        let done = dev.process(ram, disk, bounce);
        if done.interrupt {
            pic.raise(VIRTIO_IRQ);
        }
    }
}

/// A device the guest reaches by loads and stores to a window.
trait MmioDev {
    fn load(&mut self, size: u8) -> u64;
    fn store(&mut self, size: u8, val: u64);
}

/// The local APIC's register page, at `off` into it, at bus time `now`.
struct LapicDev<'a> {
    lapic: &'a mut Lapic,
    off: u64,
    now: u64,
}

impl MmioDev for LapicDev<'_> {
    fn load(&mut self, _size: u8) -> u64 {
        u64::from(self.lapic.read(self.off, self.now))
    }

    fn store(&mut self, _size: u8, val: u64) {
        self.lapic.write(self.off, val as u32, self.now);
    }
}

/// The virtio-blk window, at `off`.
struct VirtioDev<'a> {
    dev: &'a mut MmioBlk,
    disk: &'a mut HostDisk,
    ram: &'a Ram,
    bounce: &'a mut [u8],
    pic: &'a mut PicPair,
    off: u64,
}

impl MmioDev for VirtioDev<'_> {
    fn load(&mut self, size: u8) -> u64 {
        self.dev.read(self.off, size)
    }

    fn store(&mut self, size: u8, val: u64) {
        mmio_write(self.dev, self.disk, self.ram, self.bounce, self.pic, self.off, size, val);
    }
}

/// Carry out a decoded access against `dev` and step the guest past the
/// instruction. A compare reads the device once and writes only the flags.
fn mmio_emulate(a: &mmio::Access, dev: &mut dyn MmioDev, v: &mut Vmcb<'_>, regs: &mut GuestRegs) {
    match a.kind {
        mmio::Kind::Load { reg, .. } => {
            let val = dev.load(a.size);
            let old = get_reg(v, regs, reg);
            set_reg(v, regs, reg, a.load_result(old, val));
        }
        mmio::Kind::Store { reg, .. } => dev.store(a.size, a.store_value(get_reg(v, regs, reg))),
        mmio::Kind::StoreImm(_) => dev.store(a.size, a.store_value(0)),
        mmio::Kind::Compare { other, .. } => {
            let device = dev.load(a.size);
            let reg_value = match other {
                mmio::Other::Reg { reg, .. } => get_reg(v, regs, reg),
                mmio::Other::Imm(_) => 0,
            };
            let flags = a.compare_flags(reg_value, device);
            v.set_rflags((v.rflags() & !mmio::ARITH_FLAGS) | flags);
        }
    }
    v.set_rip(v.rip() + u64::from(a.len));
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

/// Run the Linux phase. Called once SVM is enabled and the root filesystem is
/// mounted (`kvm::late`).
/// [`LINUX_CMDLINE`] followed by ` tsc_early_khz=<khz>` (omitted when the host
/// has no calibrated TSC), assembled in `buf`.
fn cmdline_with_tsc<'a>(buf: &'a mut [u8; 400], base: &str, khz: u64, extra: &str) -> &'a str {
    let base = base.as_bytes();
    buf[..base.len()].copy_from_slice(base);
    let mut n = base.len();
    buf[n..n + extra.len()].copy_from_slice(extra.as_bytes());
    n += extra.len();
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
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

pub fn run(nrips: bool, guest: Guest) {
    crumb(0x51);
    p("[KVM] linux: loading ");
    p(guest.name());
    p(" guest ");
    p(guest.path());
    p("\n");
    let ram_bytes = guest.ram_bytes();
    let Some(ram) = Ram::alloc(ram_bytes) else {
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
    let loaded = match load_kernel(guest.path(), &ram, &mut scratch) {
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
    // With a disk, one more page under the handoff pages holds the ACPI tables
    // and the memory map reports it (type 3) instead of as RAM.
    let disk = HostDisk::pick(guest.disk_path());
    // Only Linux is told about its disk through ACPI; the Akuma guest finds it on
    // the command line.
    let acpi = disk.is_some() && guest == Guest::Linux;
    let acpi_gpa = ram_bytes - 0x4000;
    let l = pvh::plan(ram_bytes, 0, if acpi { acpi_gpa } else { ram_bytes - 0x3000 });
    scratch.bytes_mut().fill(0);
    // The guest is told the host's TSC rate rather than left to calibrate it
    // through trapped port reads: a poll loop whose every iteration is a
    // VM exit measures the exits, not the PIT (`tsc_early_khz` is a documented
    // kernel parameter). Built on the stack: no allocation.
    let mut cmd = [0u8; 400];
    let cmdline = match guest {
        Guest::Linux => cmdline_with_tsc(
            &mut cmd,
            LINUX_CMDLINE,
            crate::lapic::tsc_hz() / 1000,
            if disk.is_some() { ROOT_ARGS } else { NO_DISK_ARGS },
        ),
        // The Akuma guest calibrates its own TSC against the virtual PIT.
        Guest::Akuma => cmdline_with_tsc(&mut cmd, AKUMA_CMDLINE, 0, if disk.is_some() { AKUMA_DISK_ARGS } else { "" }),
    };
    match disk {
        Some(d) => {
            p("[KVM] linux: guest disk: ");
            p(d.what());
            p(", ");
            dec(d.sectors() >> 11);
            p(" MiB\n");
        }
        None => {
            p("[KVM] linux: no guest disk (no second virtio-blk, no ");
            p(guest.disk_path());
            p(")\n");
        }
    }
    if !pvh::write_cmdline(scratch.bytes_mut(), cmdline) || !ram.write(l.cmdline_gpa, scratch.bytes_mut()) {
        p("[KVM] linux: cmdline handoff failed\n");
        return;
    }
    // The memory map and start_info are built together: with a disk the ACPI
    // entry and `rsdp_paddr` go into them, and the tables into their own page.
    let mut start_page = FreshFrame::alloc_zeroed();
    let Some(start_page) = start_page.as_mut() else {
        p("[KVM] linux: no frame for start_info\n");
        return;
    };
    scratch.bytes_mut().fill(0);
    pvh::write_memmap(scratch.bytes_mut(), &l);
    pvh::write_start(start_page.bytes_mut(), &l);
    if acpi {
        let dev = svm::acpi::VirtioMmio { base: VIRTIO_BASE as u32, len: VIRTIO_LEN as u32, irq: u32::from(VIRTIO_IRQ) };
        let Some(mut acpi_page) = FreshFrame::alloc_zeroed() else {
            p("[KVM] linux: no frame for the ACPI tables\n");
            return;
        };
        let Some(rsdp) = svm::acpi::build(acpi_page.bytes_mut(), acpi_gpa, &dev) else {
            p("[KVM] linux: ACPI tables do not fit\n");
            return;
        };
        pvh::add_acpi(start_page.bytes_mut(), scratch.bytes_mut(), rsdp, acpi_gpa, 0x1000);
        if !ram.write(acpi_gpa, acpi_page.bytes_mut()) {
            p("[KVM] linux: ACPI handoff failed\n");
            return;
        }
    }
    let memmap_ok = ram.write(l.memmap_gpa, scratch.bytes_mut());
    if !memmap_ok || !ram.write(l.start_gpa, start_page.bytes_mut()) {
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
        // The Akuma guest demand-faults all its user memory: its #PF is routine
        // (nested paging delivers it without an exit), so only Linux's is traced.
        v.set_excp(match guest {
            Guest::Linux => excp::UD | excp::GP | excp::PF | excp::DF,
            Guest::Akuma => excp::UD | excp::GP | excp::DF,
        });
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
    dec(ram_bytes >> 20);
    p(" MiB)\n");

    let mut regs = GuestRegs { rbx: l.start_gpa, ..GuestRegs::default() };
    let mut plat = Platform::new(guest);
    let mut log = ExitLog { cpuid: 0, msr: 0, exc: 0 };
    let (mut exits, mut n_cpuid, mut n_msr, mut n_io, mut n_irq, mut n_exc) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut stopped = "exit budget spent";
    let mut fatal = false;
    let (mut n_inj, mut n_hlt, mut n_mmio) = (0u64, 0u64, 0u64);
    let mut blk = disk.map(|d| (MmioBlk::new(d.sectors()), d));
    if crate::lapic::tsc_uptime_us().is_none() {
        p("[KVM] linux: warning: host uptime is 10 ms ticks; PIT timing will be coarse\n");
    }

    // IRQs on for the whole run: with V_INTR_MASKING the host IF at VMRUN gates
    // physical interrupts, and a tick must reach the host.
    akuma_cpu::daif::unmask_irq();
    let started_us = crate::net::uptime_us();
    plat.last_tx_us = started_us;
    let (mut n_lapic, mut n_ticks) = (0u64, 0u64);
    'run: while exits < MAX_EXITS {
        exits += 1;
        let now_us = crate::net::uptime_us();
        if now_us.saturating_sub(started_us) > RUN_BUDGET_US {
            stopped = "wall-clock budget spent";
            break 'run;
        }
        if plat.banner.seen && now_us.saturating_sub(plat.last_tx_us) > QUIET_BUDGET_US {
            stopped = "guest console quiet after the banner";
            break 'run;
        }
        if plat.done.seen {
            stopped = "guest reported its init ran to the end";
            break 'run;
        }
        // Bring the interrupt sources up to now (the PIT's channel 0 edge, the
        // local APIC's timer), then offer the best vector to the guest as a
        // virtual INTR; the processor takes it when the guest's IF allows
        // (§15.21.4) and writes V_IRQ back cleared when it has.
        let offered = plat.offer(now_us);
        Vmcb::new(vmcb_f.bytes_mut()).set_v_intr(match offered {
            Some(o) => ctrl::v_intr_extint(o.vector),
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
        if let Some(o) = offered
            && v.v_intr() & ctrl::V_IRQ == 0
        {
            n_inj += 1;
            if !plat.accept(o) {
                p("\n[KVM] linux: PIC acknowledged a different vector than it offered\n");
            }
            if o.source == Source::Lapic {
                n_ticks += 1;
            }
            if n_inj <= LOG_FIRST.into() {
                p("\n[KVM] linux: irq vector ");
                hx(u64::from(o.vector), 2);
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
                    let val = u64::from(plat.io_in(io.port, io.size, pit::ticks_from_us(crate::net::uptime_us()))) & io.mask();
                    let rax = v.rax();
                    v.set_rax(if io.size == 4 { val } else { (rax & !io.mask()) | val });
                } else {
                    plat.io_out(io.port, io.size, (v.rax() & io.mask()) as u32, pit::ticks_from_us(crate::net::uptime_us()));
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
                // Idle until a timer can raise an interrupt the guest will be
                // offered. The host tick keeps running underneath (IRQs are on).
                loop {
                    let now = crate::net::uptime_us();
                    if plat.offer(now).is_some() {
                        break;
                    }
                    if !plat.can_wake() {
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
            ExitCode::Npf if plat.lapic_page().is_some_and(|b| (b..b + lapic::PAGE_LEN).contains(&v.exitinfo2())) => {
                n_lapic += 1;
                let off = v.exitinfo2() - (plat.apic_base & APIC_BASE_ADDR);
                let access = guest_code(&ram, &v, v.rip()).and_then(|c| mmio::decode(&c));
                let Some(a) = access else {
                    stopped = "guest touched the local APIC page with an instruction the MMIO decoder does not know";
                    fatal = true;
                    p("\n[KVM] linux: ");
                    p(stopped);
                    p(" rip=");
                    hx(v.rip(), 16);
                    p("\n");
                    dump_code(&ram, &v, v.rip());
                    break 'run;
                };
                let now = lapic::bus_ticks_from_us(crate::net::uptime_us());
                mmio_emulate(&a, &mut LapicDev { lapic: &mut plat.lapic, off, now }, &mut v, &mut regs);
            }
            ExitCode::Npf if blk.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_LEN).contains(&v.exitinfo2()) => {
                n_mmio += 1;
                let off = v.exitinfo2() - VIRTIO_BASE;
                let access = guest_code(&ram, &v, v.rip()).and_then(|c| mmio::decode(&c));
                let Some(a) = access else {
                    stopped = "guest touched the virtio window with an instruction the MMIO decoder does not know";
                    fatal = true;
                    p("\n[KVM] linux: ");
                    p(stopped);
                    p(" rip=");
                    hx(v.rip(), 16);
                    p("\n");
                    dump_code(&ram, &v, v.rip());
                    break 'run;
                };
                let (dev, hostdisk) = blk.as_mut().expect("checked by the guard");
                let mut window =
                    VirtioDev { dev, disk: hostdisk, ram: &ram, bounce: scratch.bytes_mut(), pic: &mut plat.pic, off };
                mmio_emulate(&a, &mut window, &mut v, &mut regs);
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
    p(" mmio=");
    dec(n_mmio);
    p(" lapic=");
    dec(n_lapic);
    p(" lapic_irq=");
    dec(n_ticks);
    p(" console_bytes=");
    dec(plat.tx_bytes);
    p(" rip=");
    hx(v.rip(), 16);
    p("\n");
    // A Linux run passes on its banner; the Akuma guest must also have run its
    // init to the end (the marker the script on its disk prints).
    let pass = plat.banner.seen && !fatal && (guest == Guest::Linux || plat.done.seen);
    p("[KVM] ");
    p(guest.name());
    p(" guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (banner=");
    p(if plat.banner.seen { "yes" } else { "no" });
    if guest == Guest::Akuma {
        p(", init=");
        p(if plat.done.seen { "yes" } else { "no" });
    }
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
