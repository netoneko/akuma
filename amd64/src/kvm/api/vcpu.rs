//! A virtual processor of the API VM: its state (a VMCB, the registers `VMRUN` does not carry, the
//! VMM's CPUID table, its FPU image), the marshalling of the `SET_*`/`GET_*` ioctls onto it, and
//! `KVM_RUN`: the world switch in the calling VMM thread's syscall, with in-kernel PIT/PIC/LAPIC
//! and every other port and MMIO access returned to the VMM through the run page
//! ([`akuma_kvm_abi::runpage`]) and completed at the next `RUN`.

use akuma_kvm_abi::fdargs::{Ioeventfd, Irqfd};
use akuma_kvm_abi::runpage::{self, Exit};
use akuma_kvm_abi::slots::Table;
use akuma_kvm_abi::state::{
    CPUID_ENTRY_LEN, CpuidEntry, Dtable, FPU_LEN, LIST_HDR, MSR_ENTRY_LEN, MsrEntry, Regs, Segment, Sregs,
};
use akuma_svm_amd64 as svm;
use akuma_svm_amd64::{ExitCode, Seg, SegAttr, Vmcb, ctrl, eventinj, exitinfo, mmio, save, tables};
use spinning_top::Spinlock;

use super::platform::{Offer, Platform, Source};
use crate::fd::errno;
use crate::kvm::linux::{Guest, GuestRegs, INTR0_BASE, get_reg, init_vmcb, save_host, set_reg, vmrun_regs};
use crate::kvm::{enable_on_core, next_rip};
use crate::phys::FreshFrame;
use crate::sched::FxArea;

pub(super) const MAX_CPUID: usize = 96;
/// The run mapping is two pages: the structure, then room for the coalesced-MMIO ring.
pub(super) const RUN_PAGES: usize = 2;

/// The VMM's eventfd registrations, copied at the start of a `RUN` (so the exit loop holds no VM lock).
#[derive(Clone, Copy)]
pub(super) struct Events {
    io: [Option<(Ioeventfd, u32)>; super::MAX_IOEVENTFDS],
    irq: [Option<(Irqfd, u32)>; super::MAX_IRQFDS],
}

impl Events {
    pub(super) fn snapshot(
        io: &[Option<(Ioeventfd, u32)>; super::MAX_IOEVENTFDS],
        irq: &[Option<(Irqfd, u32)>; super::MAX_IRQFDS],
    ) -> Self {
        Self { io: *io, irq: *irq }
    }

    /// Does a store of `size` bytes of `val` to `addr` ring a registered doorbell? If so, signal it.
    fn doorbell(&self, addr: u64, size: u8, val: u64) -> bool {
        let Some((_, id)) = self.io.iter().flatten().find(|(e, _)| e.matches(addr, u32::from(size), val)) else {
            return false;
        };
        let _ = akuma_syscalls_glue::eventfd::eventfd_write(*id, 1);
        note("doorbell", addr, DOORBELLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed));
        true
    }

    /// Raise the legacy line of every interrupt eventfd that was signalled. gsi `n` < 16 is PIC pin `n`
    /// (the default routing; there is no I/O APIC yet).
    fn poll_irqs(&self, plat: &Spinlock<Option<Platform>>) {
        for (irqfd, id) in self.irq.iter().flatten() {
            if akuma_syscalls_glue::eventfd::eventfd_read(*id).is_ok() && irqfd.gsi < 16 {
                note("irqfd gsi", u64::from(irqfd.gsi), IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed));
                with_plat(plat, |p| p.raise_irq(irqfd.gsi as u8));
            }
        }
    }
}

static DOORBELLS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static IRQS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Log the first few of an event kind (the `kvmapi` census instrument).
fn note(what: &str, v: u64, n: u32) {
    if n < 12 {
        crate::serial::puts("[kvmapi] ");
        crate::serial::puts(what);
        crate::serial::puts(" 0x");
        crate::serial::put_hexn(v, 8);
        crate::serial::puts(" #");
        crate::serial::put_dec(u64::from(n));
        crate::serial::puts("\n");
    }
}

/// What the VMM owes the guest from the exit that returned to it.
#[derive(Clone, Copy)]
enum Pending {
    None,
    /// An `IN`: the value in the run page goes into `RAX` (width `size`), then `RIP` moves to `next`.
    IoIn { size: u8, next: u64 },
    /// An MMIO load or compare: apply the decoded access with the value in the run page.
    Mmio(mmio::Access),
}

pub(super) struct Vcpu {
    id: usize,
    vmcb: FreshFrame,
    host: FreshFrame,
    /// The run mapping's frames (the VMM maps them; the kernel writes exits through the physmap).
    pub(super) run: [FreshFrame; RUN_PAGES],
    regs: GuestRegs,
    cpuid: [CpuidEntry; MAX_CPUID],
    n_cpuid: usize,
    fpu: FxArea,
    pending: Pending,
}

impl Vcpu {
    pub(super) fn new(id: usize) -> Option<Self> {
        let mut vmcb = FreshFrame::alloc_zeroed()?;
        let host = FreshFrame::alloc_zeroed()?;
        let run = [FreshFrame::alloc_zeroed()?, FreshFrame::alloc_zeroed()?];
        init_vmcb(vmcb.bytes_mut(), Guest::Akuma, 0, id as u32 + 1);
        Some(Self {
            id,
            vmcb,
            host,
            run,
            regs: GuestRegs::default(),
            cpuid: [CpuidEntry::default(); MAX_CPUID],
            n_cpuid: 0,
            fpu: FxArea::reset_state(),
            pending: Pending::None,
        })
    }

    fn vmcb(&mut self) -> Vmcb<'_> {
        Vmcb::new(self.vmcb.bytes_mut())
    }

    // ----- state in -----

    pub(super) fn set_regs(&mut self, r: &Regs) {
        self.regs = GuestRegs {
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
        };
        let mut v = self.vmcb();
        v.set_rax(r.rax);
        v.set_rsp(r.rsp);
        v.set_rip(r.rip);
        v.set_rflags(r.rflags | 2);
    }

    pub(super) fn get_regs(&mut self) -> Regs {
        let g = self.regs;
        let v = self.vmcb();
        Regs {
            rax: v.rax(),
            rbx: g.rbx,
            rcx: g.rcx,
            rdx: g.rdx,
            rsi: g.rsi,
            rdi: g.rdi,
            rsp: v.rsp(),
            rbp: g.rbp,
            r8: g.r8,
            r9: g.r9,
            r10: g.r10,
            r11: g.r11,
            r12: g.r12,
            r13: g.r13,
            r14: g.r14,
            r15: g.r15,
            rip: v.rip(),
            rflags: v.rflags(),
        }
    }

    pub(super) fn set_sregs(&mut self, s: &Sregs, plat: &mut Platform) {
        let id = self.id;
        let mut v = self.vmcb();
        let seg = |x: &Segment| Seg { selector: x.selector, attr: SegAttr(x.vmcb_attrib()), limit: x.limit, base: x.base };
        v.set_seg(save::CS, seg(&s.cs));
        v.set_seg(save::DS, seg(&s.ds));
        v.set_seg(save::ES, seg(&s.es));
        v.set_seg(save::FS, seg(&s.fs));
        v.set_seg(save::GS, seg(&s.gs));
        v.set_seg(save::SS, seg(&s.ss));
        v.set_seg(save::TR, seg(&s.tr));
        v.set_seg(save::LDTR, seg(&s.ldt));
        let table = |d: &Dtable| Seg { selector: 0, attr: SegAttr(0), limit: u32::from(d.limit), base: d.base };
        v.set_seg(save::GDTR, table(&s.gdt));
        v.set_seg(save::IDTR, table(&s.idt));
        v.set_cr0(s.cr0);
        v.set_cr3(s.cr3);
        v.set_cr4(s.cr4);
        v.set_save64(save::CR2, s.cr2);
        v.set_efer(s.efer | svm::msr::EFER_SVME);
        v.set_cpl(s.ss.dpl);
        if s.apic_base != 0 {
            plat.set_apic_base(id, s.apic_base);
        }
    }

    pub(super) fn get_sregs(&mut self, plat: &Platform) -> Sregs {
        let apic_base = plat.apic_base(self.id);
        let v = self.vmcb();
        let seg = |off: usize| {
            let s = v.seg(off);
            Segment::from_vmcb(s.base, s.limit, s.selector, s.attr.0)
        };
        let table = |off: usize| {
            let s = v.seg(off);
            Dtable { base: s.base, limit: s.limit as u16 }
        };
        Sregs {
            cs: seg(save::CS),
            ds: seg(save::DS),
            es: seg(save::ES),
            fs: seg(save::FS),
            gs: seg(save::GS),
            ss: seg(save::SS),
            tr: seg(save::TR),
            ldt: seg(save::LDTR),
            gdt: table(save::GDTR),
            idt: table(save::IDTR),
            cr0: v.cr0(),
            cr2: v.save64(save::CR2),
            cr3: v.cr3(),
            cr4: v.cr4(),
            cr8: 0,
            efer: v.efer() & !svm::msr::EFER_SVME,
            apic_base,
            interrupt_bitmap: [0; 4],
        }
    }

    /// `SET_CPUID2`: the VMM's table, kept for the guest's CPUID exits. `false`: more entries than fit.
    pub(super) fn set_cpuid(&mut self, entries: &[u8], n: usize) -> bool {
        if n > MAX_CPUID {
            return false;
        }
        for i in 0..n {
            let Some(e) = CpuidEntry::parse(&entries[i * CPUID_ENTRY_LEN..]) else { return false };
            self.cpuid[i] = e;
        }
        self.n_cpuid = n;
        true
    }

    /// `SET_MSRS`: how many of `n` entries (at `entries`) were taken.
    pub(super) fn set_msrs(&mut self, entries: &[u8], n: usize, plat: &mut Platform) -> usize {
        let id = self.id;
        let mut v = self.vmcb();
        let mut done = 0;
        for i in 0..n {
            let Some(e) = MsrEntry::parse(&entries[i * MSR_ENTRY_LEN..]) else { break };
            if !plat.wrmsr(id, &mut v, e.index, e.data) {
                break;
            }
            done += 1;
        }
        done
    }

    pub(super) fn set_fpu(&mut self, arg: &[u8; FPU_LEN]) {
        let mut legacy = [0u8; 512];
        akuma_kvm_abi::state::fpu_to_fxsave(arg, &mut legacy);
        self.fpu = FxArea::reset_state();
        self.fpu.legacy_mut().copy_from_slice(&legacy);
        self.fpu.sanitize();
    }

    // ----- the run page -----

    fn page(&mut self) -> &mut [u8; runpage::PAGE] {
        self.run[0].bytes_mut()
    }

    fn cpuid_lookup(&self, leaf: u32, sub: u32) -> [u32; 4] {
        for e in &self.cpuid[..self.n_cpuid] {
            if e.function == leaf && (e.index == sub || e.flags & 1 == 0) {
                return [e.eax, e.ebx, e.ecx, e.edx];
            }
        }
        [0; 4]
    }
}

/// Guest memory as the slot table sees it, for the guest's page walks and instruction fetches.
struct SlotMem<'a>(&'a Table);

impl SlotMem<'_> {
    fn read(&self, gpa: u64, dst: &mut [u8]) -> bool {
        self.0
            .slice_offset(gpa)
            .is_some_and(|pa| akuma_mmu::copy_from_phys(pa as usize, dst))
    }
}

impl tables::PageSource for SlotMem<'_> {
    fn read(&self, table_pa: u64, off: usize) -> u64 {
        let mut b = [0u8; 8];
        if self.read(table_pa + off as u64, &mut b) { u64::from_le_bytes(b) } else { 0 }
    }
}

fn guest_code(mem: &SlotMem<'_>, v: &Vmcb<'_>, va: u64) -> Option<[u8; 16]> {
    let paging = v.cr0() & svm::cr0::PG != 0;
    let gpa = if paging {
        if v.efer() & svm::msr::EFER_LMA == 0 {
            return None;
        }
        tables::translate(mem, v.cr3() & !0xFFF, va).ok()?
    } else {
        va
    };
    let mut code = [0u8; 16];
    mem.read(gpa, &mut code).then_some(code)
}

/// The BKL released for as long as the value lives (a guest entry can last a whole quantum).
struct Unlocked;

impl Unlocked {
    fn new() -> Self {
        akuma_bkl::bkl::dropped_window_open();
        Self
    }
}

impl Drop for Unlocked {
    fn drop(&mut self) {
        akuma_bkl::bkl::dropped_window_close();
    }
}

/// The running thread pinned to its core for the duration (the core's `EFER.SVME` and host save area
/// are per-core state), unpinned again on every path out.
struct Pinned;

impl Drop for Pinned {
    fn drop(&mut self) {
        crate::sched::unpin_current();
    }
}

/// A signal the thread may take is waiting: `RUN` returns `EINTR`, as a blocked syscall does.
fn signal_pending() -> bool {
    let tid = akuma_threading::current_thread_id();
    akuma_threading::pending_signals_raw(tid) & !akuma_threading::thread_signal_mask() != 0
}

/// The guest-physical `QUANTUM_US` an entry may last when nothing else bounds it.
const QUANTUM_US: u64 = 5_000;

/// `KVM_RUN`. 0 when the VMM has something to do (the run page says what), `EINTR` for a signal.
pub(super) fn run(vc: &mut Vcpu, plat: &Spinlock<Option<Platform>>, slots: &Table, npt_pa: u64, ev: &Events) -> u64 {
    // The core must have SVM on and the thread must stay on it.
    let mut me = crate::smp::cpu_index();
    for _ in 0..3 {
        if !crate::sched::repin_current(me) {
            return errno::EAGAIN;
        }
        if crate::smp::cpu_index() == me {
            break;
        }
        me = crate::smp::cpu_index();
    }
    let _pin = Pinned;
    if !enable_on_core(me) {
        return errno::ENODEV;
    }
    if runpage::immediate_exit(vc.run[0].bytes_mut()) {
        return errno::EINTR;
    }
    let saved_flags = akuma_cpu::daif::read();
    akuma_cpu::daif::unmask_irq();
    let r = run_inner(vc, plat, slots, npt_pa, ev, me);
    akuma_cpu::daif::restore(saved_flags);
    r
}

fn io_ticks() -> u64 {
    svm::pit::ticks_from_us(crate::net::uptime_us())
}

fn run_inner(vc: &mut Vcpu, plat: &Spinlock<Option<Platform>>, slots: &Table, npt_pa: u64, ev: &Events, _core: usize) -> u64 {
    let mem = SlotMem(slots);
    let id = vc.id;
    let nrips = true;
    let host_pa = vc.host.pa() as u64;
    let vmcb_pa = vc.vmcb.pa() as u64;
    let mut first = true;

    // Finish what the previous exit left the VMM to do.
    complete_pending(vc);

    // The host's FPU/SSE state belongs to the VMM thread: park it, run the guest's, put it back.
    let mut host_fpu = FxArea::zeroed();
    loop {
        if signal_pending() {
            return errno::EINTR;
        }
        // Other host threads (the VMM's own device thread, the network) get the core between entries:
        // the kernel switches cooperatively, and the switch expects interrupts masked.
        akuma_cpu::daif::mask_irq();
        crate::sched::yield_now();
        akuma_cpu::daif::unmask_irq();
        ev.poll_irqs(plat);
        let now = crate::net::uptime_us();
        // Offer the best vector (bringing the PIT and the APIC timer up to now), and find when the next
        // timer will want a host interrupt.
        let (offered, due) = with_plat(plat, |p| (p.offer(id, now), p.next_event_us(id)));
        {
            let mut v = vc.vmcb();
            v.set_n_cr3(npt_pa);
            v.set_intr0(INTR0_BASE);
            v.set_v_intr(match offered {
                Some(o) => ctrl::v_intr_extint(o.vector),
                None => ctrl::V_INTR_MASKING,
            });
            if first {
                v.set_tlb_control(ctrl::TLB_FLUSH_ALL);
                first = false;
            } else {
                v.set_tlb_control(ctrl::TLB_DO_NOTHING);
            }
        }
        // A host interrupt at the guest's next deadline (or this quantum's end) ends the entry.
        let wake = due.unwrap_or(u64::MAX).min(now + QUANTUM_US);
        akuma_cpu::daif::mask_irq();
        let _ = crate::lapic::arm_oneshot_in_us(wake.saturating_sub(crate::net::uptime_us()));
        akuma_cpu::daif::unmask_irq();

        save_host(host_pa);
        crate::sched::fpu_save_live(&mut host_fpu);
        crate::sched::fpu_restore_live(&vc.fpu);
        // SAFETY: the VMCB and host-save frames are live and owned by `vc`; SVM is enabled on this core
        // (`enable_on_core`) and the thread is pinned to it.
        let unlocked = Unlocked::new();
        unsafe { vmrun_regs(vmcb_pa, host_pa, &mut vc.regs) };
        drop(unlocked);
        crate::sched::fpu_save_live(&mut vc.fpu);
        host_fpu.sanitize();
        crate::sched::fpu_restore_live(&host_fpu);

        let mut v = Vmcb::new(vc.vmcb.bytes_mut());
        // The INTA cycle: V_IRQ comes back cleared once the guest has dispatched the vector.
        if let Some(o) = offered
            && v.v_intr() & ctrl::V_IRQ == 0
        {
            with_plat(plat, |p| p.accept(id, o));
        }
        let code = ExitCode::from_u64(v.exitcode());
        match code {
            ExitCode::Cpuid => {
                let (leaf, sub) = (v.rax() as u32, vc.regs.rcx as u32);
                let [a, b, c, d] = vc.cpuid_lookup(leaf, sub);
                let mut v = Vmcb::new(vc.vmcb.bytes_mut());
                v.set_rax(u64::from(a));
                vc.regs.rbx = u64::from(b);
                vc.regs.rcx = u64::from(c);
                vc.regs.rdx = u64::from(d);
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Msr => {
                let msr = vc.regs.rcx as u32;
                let write = exitinfo::msr_is_write(v.exitinfo1());
                if write {
                    let val = (vc.regs.rdx << 32) | (v.rax() & 0xFFFF_FFFF);
                    with_plat(plat, |p| {
                        p.wrmsr(id, &mut v, msr, val);
                    });
                } else {
                    let val = with_plat(plat, |p| p.rdmsr(id, &v, msr));
                    v.set_rax(val & 0xFFFF_FFFF);
                    vc.regs.rdx = val >> 32;
                }
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Ioio => {
                let io = exitinfo::Io::decode(v.exitinfo1());
                if io.string || io.size == 0 {
                    return internal_error(vc, 1);
                }
                let next = v.exitinfo2();
                let ticks = io_ticks();
                if io.is_in {
                    let mine = with_plat(plat, |p| p.io_in(io.port, ticks));
                    match mine {
                        Some(val) => {
                            let val = u64::from(val) & io.mask();
                            let rax = v.rax();
                            v.set_rax(if io.size == 4 { val } else { (rax & !io.mask()) | val });
                            v.complete_instruction(next);
                        }
                        None => {
                            vc.pending = Pending::IoIn { size: io.size, next };
                            runpage::set_io(vc.page(), runpage::IO_IN, io.size, io.port, 0);
                            return 0;
                        }
                    }
                } else {
                    let val = (v.rax() & io.mask()) as u32;
                    let taken = with_plat(plat, |p| p.io_out(io.port, val, ticks));
                    v.complete_instruction(next);
                    if !taken {
                        runpage::set_io(vc.page(), runpage::IO_OUT, io.size, io.port, val);
                        return 0;
                    }
                }
            }
            ExitCode::Hlt => {
                if v.rflags() & (1 << 9) == 0 {
                    // Nothing can wake it: the VMM decides (a halted-forever guest ends the run).
                    runpage::set_exit(vc.page(), Exit::Hlt);
                    return 0;
                }
                let next = next_rip(&v, nrips, 1);
                v.complete_instruction(next);
                if let Some(r) = idle(vc, plat, ev) {
                    return r;
                }
            }
            ExitCode::Other(0x60 | 0x61) => {}
            ExitCode::Pause => {
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Ud | ExitCode::Gp | ExitCode::Pf | ExitCode::Other(0x48) => {
                let (vector, err) = match v.exitcode() {
                    0x46 => (6u8, None),
                    0x4D => (13, Some(v.exitinfo1() as u32)),
                    0x4E => (14, Some(v.exitinfo1() as u32)),
                    _ => (8, Some(0)),
                };
                if v.exitintinfo() & (1 << 31) != 0 {
                    return internal_error(vc, 2);
                }
                if vector == 14 {
                    let addr = v.exitinfo2();
                    v.set_save64(save::CR2, addr);
                }
                v.set_eventinj(eventinj::exception(vector, err));
            }
            ExitCode::Npf => {
                let gpa = v.exitinfo2();
                let lapic_off = with_plat(plat, |p| p.lapic_page(id)).filter(|b| (*b..*b + svm::lapic::PAGE_LEN).contains(&gpa));
                let Some(a) = guest_code(&mem, &v, v.rip()).and_then(|code| mmio::decode(&code)) else {
                    return internal_error(vc, 3);
                };
                if let Some(base) = lapic_off {
                    let off = gpa - base;
                    let now = svm::lapic::bus_ticks_from_us(crate::net::uptime_us());
                    with_plat(plat, |p| {
                        let mut dev = LapicDev { lapic: p.lapic(id), off, now };
                        emulate(&a, &mut dev, &mut v, &mut vc.regs);
                    });
                } else if let Some(r) = mmio_exit(vc, ev, gpa, a) {
                    return r;
                }
            }
            ExitCode::Shutdown => {
                runpage::set_exit(vc.page(), Exit::Shutdown);
                return 0;
            }
            _ => return internal_error(vc, 4),
        }
    }
}

fn with_plat<T>(plat: &Spinlock<Option<Platform>>, f: impl FnOnce(&mut Platform) -> T) -> T {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut g = plat.lock();
        f(g.as_mut().expect("the platform exists while a vCPU runs"))
    })
}

fn internal_error(vc: &mut Vcpu, suberror: u32) -> u64 {
    let page = vc.page();
    runpage::set_exit(page, Exit::InternalError);
    page[runpage::off::UNION..runpage::off::UNION + 4].copy_from_slice(&suberror.to_le_bytes());
    0
}

/// A load/store/compare outside the in-kernel devices: tell the VMM, finish stores now, loads on re-entry.
fn mmio_exit(vc: &mut Vcpu, ev: &Events, gpa: u64, a: mmio::Access) -> Option<u64> {
    let (is_write, val) = {
        let regs = vc.regs;
        let v = vc.vmcb();
        match a.kind {
            mmio::Kind::Store { reg, .. } => (true, a.store_value(get_reg(&v, &regs, reg))),
            mmio::Kind::StoreImm(_) => (true, a.store_value(0)),
            _ => (false, 0),
        }
    };
    if is_write && ev.doorbell(gpa, a.size, val) {
        // A registered doorbell: signal the eventfd and carry on, no trip to the VMM.
        let mut v = vc.vmcb();
        v.complete_instruction(v.rip() + u64::from(a.len));
        return None;
    }
    runpage::set_mmio(vc.page(), gpa, a.size, is_write, val);
    if is_write {
        let mut v = vc.vmcb();
        v.complete_instruction(v.rip() + u64::from(a.len));
    } else {
        vc.pending = Pending::Mmio(a);
    }
    Some(0)
}

/// Apply what the VMM left in the run page for the last exit.
fn complete_pending(vc: &mut Vcpu) {
    let pending = core::mem::replace(&mut vc.pending, Pending::None);
    match pending {
        Pending::None => {}
        Pending::IoIn { size, next } => {
            let val = u64::from(runpage::io_in_value(vc.run[0].bytes_mut(), size));
            let mask = if size >= 4 { 0xFFFF_FFFF } else { (1u64 << (u32::from(size) * 8)) - 1 };
            let mut v = vc.vmcb();
            let rax = v.rax();
            v.set_rax(if size == 4 { val & mask } else { (rax & !mask) | (val & mask) });
            v.complete_instruction(next);
        }
        Pending::Mmio(a) => {
            let val = runpage::mmio_value(vc.run[0].bytes_mut());
            let mut dev = ValueDev(val);
            let mut regs = vc.regs;
            let mut v = vc.vmcb();
            emulate(&a, &mut dev, &mut v, &mut regs);
            vc.regs = regs;
        }
    }
}

/// A "device" whose only content is the value the VMM supplied for a load.
struct ValueDev(u64);

impl MmioDev for ValueDev {
    fn load(&mut self, _size: u8) -> u64 {
        self.0
    }

    fn store(&mut self, _size: u8, _val: u64) {}
}

trait MmioDev {
    fn load(&mut self, size: u8) -> u64;
    fn store(&mut self, size: u8, val: u64);
}

struct LapicDev<'a> {
    lapic: &'a mut svm::lapic::Lapic,
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

/// Carry out a decoded access against `dev` and step the guest past the instruction (the `kvm0`
/// `mmio_emulate`, over this module's device trait).
fn emulate(a: &mmio::Access, dev: &mut dyn MmioDev, v: &mut Vmcb<'_>, regs: &mut GuestRegs) {
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
    v.complete_instruction(v.rip() + u64::from(a.len));
}

/// `HLT` with `IF` set: sleep until the guest has something to be offered, a signal arrives, or a
/// host deadline passes. `Some(errno)` ends the `RUN`.
fn idle(vc: &mut Vcpu, plat: &Spinlock<Option<Platform>>, ev: &Events) -> Option<u64> {
    let id = vc.id;
    loop {
        if signal_pending() {
            return Some(errno::EINTR);
        }
        ev.poll_irqs(plat);
        let now = crate::net::uptime_us();
        let (offer, due, can_wake): (Option<Offer>, Option<u64>, bool) =
            with_plat(plat, |p| (p.offer(id, now), p.next_event_us(id), p.can_wake(id)));
        if let Some(o) = offer {
            // The offer was consumed by `offer`'s polling only as a peek; the entry loop re-offers it.
            let _: Source = o.source;
            return None;
        }
        let deadline = due.unwrap_or(now + 1_000).min(now + 1_000);
        let _ = can_wake;
        akuma_cpu::daif::mask_irq();
        crate::sched::block_until_deadline(deadline.max(now + 50));
        akuma_cpu::daif::unmask_irq();
    }
}
