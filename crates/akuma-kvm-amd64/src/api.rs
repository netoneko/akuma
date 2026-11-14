//! `/dev/kvm`: the Linux-shaped KVM ioctl protocol for an unmodified VMM (Firecracker).
//!
//! Armed by the `kvmapi` boot flag (with `kvmctl`). `open("/dev/kvm")` is the *system* descriptor;
//! `CREATE_VM` returns a *VM* descriptor and `CREATE_VCPU` a *vCPU* descriptor. All three are
//! `FileDescriptor::DevKvm(id)` with `id > 0` (0 is `/dev/kvm0`), the id naming a slot in
//! [`HANDLES`], so `dup` needs no reference counts; a handle lives until its owner process is gone.
//! The protocol (request codes, argument layouts, the memslot rules) is the pure crate
//! `akuma-kvm-abi`; everything here is effect: user memory, the reserved RAM, the descriptors.
//!
//! **Guest RAM** is a slice of the `kvmmem=` reserve. `SET_USER_MEMORY_REGION` carves it, copies the
//! VMM's range into it and remaps the VMM's PTEs onto it (`mm::remap_user_to_phys`), so the VMM and
//! the guest share the frames. One VM at a time, and not while a `/dev/kvm0` VM exists.
//!
//! Every ioctl is logged to the serial port (`[kvmapi]`), with the argument structure before and
//! after: this is still the census instrument of `docs/archive/AKUMA_FIRECRACKER_KVM.md`, now with
//! answers. No allocation anywhere: handle and VM tables are statics.

mod platform;
mod vcpu;

use akuma_exec_core::process::FileDescriptor;
use akuma_kvm_abi::fdargs::{IOEVENTFD_DEASSIGN, IOEVENTFD_LEN, IRQFD_LEN, Ioeventfd, Irqfd};
use akuma_kvm_abi::req::{self, Kind, Op};
use akuma_kvm_abi::slots::{self, MAX_SLOTS, Region, Table};
use akuma_kvm_abi::state::{CPUID_ENTRY_LEN, CpuidEntry, LIST_HDR};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spinning_top::Spinlock;

use akuma_kernel_amd64::drivers::services::errno;
use akuma_kernel_amd64::drivers::services::serial;
use alloc::boxed::Box;
use alloc::sync::Arc;
use akuma_svm_switch_amd64::{reserve, GuestRam, Npt};
use platform::Platform;
use vcpu::Vcpu;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU32 = AtomicU32::new(0);
/// `RUN` calls since the VM was created (the first few are logged: the hot path stays quiet after).
static RUN_CALLS: AtomicU32 = AtomicU32::new(0);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
    if on {
        serial::puts("[kvmapi] /dev/kvm armed\n");
    }
}

#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// Handles and the one VM
// ---------------------------------------------------------------------------

const MAX_HANDLES: usize = 32;
const RUN_LOG_FIRST: u32 = 0;
const MAX_VCPUS: usize = 4;
pub(super) const MAX_IRQFDS: usize = 16;
pub(super) const MAX_IOEVENTFDS: usize = 32;
/// The size of the vCPU's run page the VMM maps (two 4 KiB pages).
const RUN_MAP_SIZE: u64 = 0x2000;
/// Capabilities the VMM asked about; all of them answered "yes" (1). Anything else is 0.
const CAPS_YES: [u32; 16] = [0x00, 0x24, 0x20, 3, 4, 0x21, 0x23, 0x27, 0x32, 0x0e, 0x29, 0x38, 0x37, 7, 0x0a, 0xd0];

#[derive(Clone, Copy)]
struct Handle {
    kind: Kind,
    /// The owning process (tgid). A handle whose owner is gone is reclaimed on the next `CREATE_VM`.
    owner: u32,
    vcpu: u8,
}

static HANDLES: Spinlock<[Option<Handle>; MAX_HANDLES]> = Spinlock::new([None; MAX_HANDLES]);

struct Vm {
    owner: u32,
    slots: Table,
    /// Each slot's slice of the reserve, until the nested page table adopts it at the first `RUN`
    /// (a slot set after that stays here, unmapped, until the VM is destroyed).
    ram: [Option<GuestRam>; MAX_SLOTS],
    irqchip: bool,
    pit: bool,
    tss: u64,
    /// The vCPUs; `None` while one is inside `RUN` (its thread owns it) or not yet created.
    /// A one-element array rather than `Box<Vcpu>`: [`boxed`] makes it fallibly without a cast.
    vcpus: [Option<Box<[Vcpu; 1]>>; MAX_VCPUS],
    created: [bool; MAX_VCPUS],
    /// Host-physical addresses of each vCPU's run frames, for `mmap`.
    run_pas: [[usize; vcpu::RUN_PAGES]; MAX_VCPUS],
    /// Built at the first `RUN`; `Err` when it could not map every slot (its RAM is still held, so
    /// the VMM's mappings of it stay the VM's).
    npt: Option<Result<Arc<Npt>, Arc<Npt>>>,
    /// Each with the kernel id of its eventfd (a reference is held until the VM is destroyed).
    irqfds: [Option<(Irqfd, u32)>; MAX_IRQFDS],
    ioeventfds: [Option<(Ioeventfd, u32)>; MAX_IOEVENTFDS],
}

impl Vm {
    const fn new(owner: u32) -> Self {
        Self {
            owner,
            slots: Table::new(),
            ram: [const { None }; MAX_SLOTS],
            irqchip: false,
            pit: false,
            tss: 0,
            vcpus: [const { None }; MAX_VCPUS],
            created: [false; MAX_VCPUS],
            run_pas: [[0; vcpu::RUN_PAGES]; MAX_VCPUS],
            npt: None,
            irqfds: [None; MAX_IRQFDS],
            ioeventfds: [None; MAX_IOEVENTFDS],
        }
    }
}

static VM: Spinlock<Option<Vm>> = Spinlock::new(None);
/// The in-kernel PIT/PIC/LAPIC, created by `CREATE_IRQCHIP`.
static PLAT: Spinlock<Option<Platform>> = Spinlock::new(None);

/// The nested page table over the memslots, built at the first `RUN`: each slot's slice adopted and
/// mapped at its guest-physical address (2 MiB leaves: slots and slices are 2 MiB aligned). Slots may
/// sit below or above the 3..4 GiB device window (`slots::check` refuses in/crossing it), which maps
/// nothing.
fn build_npt(slots: &Table, ram: &mut [Option<GuestRam>; MAX_SLOTS]) -> Option<Result<Arc<Npt>, Arc<Npt>>> {
    let mut npt = Npt::new()?;
    let mut ok = true;
    for s in slots.iter() {
        let Some(r) = ram.get_mut(s.region.slot as usize).and_then(Option::take) else {
            ok = false;
            continue;
        };
        match npt.adopt_ram(r) {
            Ok(id) => ok &= npt.map_ram(id, 0, s.region.size, s.region.gpa).is_ok(),
            Err(r) => {
                ok = false;
                ram[s.region.slot as usize] = Some(r);
            }
        }
    }
    let npt = Arc::new(npt);
    Some(if ok { Ok(npt) } else { Err(npt) })
}

fn irq_off<T>(f: impl FnOnce() -> T) -> T {
    akuma_primitives::irq::with_irqs_disabled(f)
}

/// A process is forking: if it owns the API VM, say so on the console.
///
/// The guest's RAM is the VMM's memory remapped onto the reserved slice, and a fork would CoW-share frames the guest writes through, so the
/// child and the guest would diverge. Not prevented (the VMM does not fork after `CREATE_VM`), only visible.
pub fn note_fork() {
    if !enabled() {
        return;
    }
    let tgid = current_tgid();
    if irq_off(|| VM.lock().as_ref().is_some_and(|v| v.owner == tgid)) {
        serial::puts("[kvmapi] WARNING: pid ");
        serial::put_dec(u64::from(tgid));
        serial::puts(" forked while it owns a /dev/kvm VM: the child's view of guest RAM is not the guest's\n");
    }
}

/// Is an API VM alive (so a `/dev/kvm0` VM must wait)?
#[must_use]
pub fn vm_live() -> bool {
    let Some(owner) = irq_off(|| VM.lock().as_ref().map(|v| v.owner)) else { return false };
    if owner_gone(owner) {
        destroy_vm();
        return false;
    }
    true
}

/// Give back a VM's reserve slices and drop every handle. The mappings the VMM still holds of the
/// slices must not outlive this: it is only called when the owner is gone (or at `CREATE_VM`
/// finding a dead owner).
fn destroy_vm() {
    let taken = irq_off(|| VM.lock().take());
    if let Some(vm) = taken {
        // Dropping `vm` gives its slices back to the reserve (a vCPU still out in `RUN` keeps the
        // nested page table, and the slices it adopted, alive until it returns).
        for (_, id) in vm.irqfds.into_iter().flatten() {
            akuma_syscalls_glue::eventfd::eventfd_close(id);
        }
        for (_, id) in vm.ioeventfds.into_iter().flatten() {
            akuma_syscalls_glue::eventfd::eventfd_close(id);
        }
    }
    irq_off(|| *PLAT.lock() = None);
    reap_handles();
}

/// Drop every handle whose owner process is gone (a `dup` into a live process keeps its own handle
/// only if it was opened there: the table is keyed by the opener).
fn reap_handles() {
    for i in 0..MAX_HANDLES {
        let owner = irq_off(|| HANDLES.lock()[i].map(|h| h.owner));
        if owner.is_some_and(owner_gone) {
            irq_off(|| HANDLES.lock()[i] = None);
        }
    }
}

fn owner_gone(owner: u32) -> bool {
    owner != 0 && akuma_exec::process::lookup_process_shared(owner).is_none()
}

fn current_tgid() -> u32 {
    akuma_kernel_amd64::drivers::services::usermode::current_process().map_or(0, |p| p.tgid)
}

fn new_handle(kind: Kind, vcpu: u8) -> u64 {
    let owner = current_tgid();
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return errno::ESRCH;
    };
    let slot = irq_off(|| {
        let mut h = HANDLES.lock();
        let i = h.iter().position(Option::is_none)?;
        h[i] = Some(Handle { kind, owner, vcpu });
        Some(i)
    });
    let Some(i) = slot else { return errno::EMFILE };
    u64::from(proc.alloc_fd(FileDescriptor::DevKvm(i as u32 + 1)))
}

/// `open("/dev/kvm")`: the system descriptor.
#[must_use]
pub fn open(flags: u32) -> u64 {
    reap_handles();
    let r = new_handle(Kind::System, 0);
    if r.cast_signed() >= 0 && flags & akuma_kernel_amd64::drivers::services::fd::open_flags::O_CLOEXEC != 0
        && let Some(proc) = akuma_exec::process::current_process_shared()
    {
        proc.set_cloexec(r as u32);
    }
    r
}

// ---------------------------------------------------------------------------
// The ioctl
// ---------------------------------------------------------------------------

fn answer(h: Handle, op: Op, arg: u64) -> u64 {
    match op {
        Op::GetApiVersion => 12,
        Op::CheckExtension => u64::from(CAPS_YES.contains(&(arg as u32))),
        Op::GetSupportedCpuid => supported_cpuid(arg),
        Op::GetMsrIndexList => if akuma_kernel_amd64::drivers::services::uaccess::write_val::<u32>(arg, 0) { 0 } else { errno::EFAULT },
        Op::GetVcpuMmapSize => RUN_MAP_SIZE,
        Op::CreateVm => create_vm(),
        Op::SetTssAddr => with_vm(|v| {
            v.tss = arg;
            0
        }),
        Op::CreateIrqchip => with_vm(|v| {
            if v.irqchip {
                return errno::EEXIST;
            }
            v.irqchip = true;
            irq_off(|| *PLAT.lock() = Some(Platform::new()));
            0
        }),
        Op::CreatePit2 => with_vm(|v| {
            if v.pit {
                return errno::EEXIST;
            }
            v.pit = true;
            0
        }),
        Op::CreateVcpu => create_vcpu(arg),
        Op::SetUserMemoryRegion => set_memory_region(arg),
        Op::Irqfd => irqfd(arg),
        Op::Ioeventfd => ioeventfd(arg),
        Op::SetMsrs => set_msrs(h, arg),
        Op::SetCpuid2 => set_cpuid2(h, arg),
        Op::SetRegs => match user_bytes::<{ akuma_kvm_abi::state::REGS_LEN }>(arg).and_then(|b| akuma_kvm_abi::state::Regs::parse(&b)) {
            Some(r) => with_vcpu(h.vcpu, |vc, _| {
                vc.set_regs(&r);
                0
            }),
            None => errno::EFAULT,
        },
        Op::SetFpu => match user_bytes::<{ akuma_kvm_abi::state::FPU_LEN }>(arg) {
            Some(b) => with_vcpu(h.vcpu, |vc, _| {
                vc.set_fpu(&b);
                0
            }),
            None => errno::EFAULT,
        },
        Op::SetSregs => match user_bytes::<{ akuma_kvm_abi::state::SREGS_LEN }>(arg).and_then(|b| akuma_kvm_abi::state::Sregs::parse(&b)) {
            Some(s) => with_vcpu(h.vcpu, |vc, p| {
                vc.set_sregs(&s, p);
                0
            }),
            None => errno::EFAULT,
        },
        Op::GetSregs => with_vcpu(h.vcpu, |vc, p| {
            let mut out = [0u8; akuma_kvm_abi::state::SREGS_LEN];
            vc.get_sregs(p).encode(&mut out);
            if akuma_kernel_amd64::drivers::services::uaccess::write_bytes(arg, &out) { 0 } else { errno::EFAULT }
        }),
        Op::GetLapic | Op::SetLapic => 0, // slice 3: the LAPIC register image
        Op::Run => run_vcpu(h.vcpu),
    }
}

/// `SET_MSRS`: a header `{nmsrs, pad}` and that many 16-byte entries; returns how many were taken.
fn set_msrs(h: Handle, arg: u64) -> u64 {
    let Some(n) = akuma_kernel_amd64::drivers::services::uaccess::read_val::<u32>(arg).map(|n| n as usize) else { return errno::EFAULT };
    if n > 64 {
        return errno::E2BIG;
    }
    let mut buf = [0u8; 64 * akuma_kvm_abi::state::MSR_ENTRY_LEN];
    let len = n * akuma_kvm_abi::state::MSR_ENTRY_LEN;
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg + akuma_kvm_abi::state::LIST_HDR as u64, &mut buf[..len]) {
        return errno::EFAULT;
    }
    with_vcpu(h.vcpu, |vc, p| vc.set_msrs(&buf[..len], n, p) as u64)
}

/// `SET_CPUID2`: a header `{nent, pad}` and that many 40-byte entries.
fn set_cpuid2(h: Handle, arg: u64) -> u64 {
    let Some(n) = akuma_kernel_amd64::drivers::services::uaccess::read_val::<u32>(arg).map(|n| n as usize) else { return errno::EFAULT };
    if n > vcpu::MAX_CPUID {
        return errno::E2BIG;
    }
    let mut buf = [0u8; vcpu::MAX_CPUID * CPUID_ENTRY_LEN];
    let len = n * CPUID_ENTRY_LEN;
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg + LIST_HDR as u64, &mut buf[..len]) {
        return errno::EFAULT;
    }
    with_vcpu(h.vcpu, |vc, _| if vc.set_cpuid(&buf[..len], n) { 0 } else { errno::E2BIG })
}

fn with_vm(f: impl FnOnce(&mut Vm) -> u64) -> u64 {
    irq_off(|| VM.lock().as_mut().map_or(errno::EINVAL, f))
}

fn create_vm() -> u64 {
    if !super::svm_armed() {
        return errno::ENODEV;
    }
    if super::ctl::vm_active() {
        return errno::EBUSY;
    }
    let stale = irq_off(|| VM.lock().as_ref().map(|v| v.owner));
    match stale {
        Some(owner) if owner_gone(owner) => destroy_vm(),
        Some(_) => return errno::EBUSY,
        None => {}
    }
    RUN_CALLS.store(0, Ordering::Relaxed);
    vcpu::reset_trace();
    irq_off(|| *VM.lock() = Some(Vm::new(current_tgid())));
    let fd = new_handle(Kind::Vm, 0);
    if fd.cast_signed() < 0 {
        irq_off(|| *VM.lock() = None);
    }
    fd
}

fn create_vcpu(id: u64) -> u64 {
    if id as usize >= MAX_VCPUS {
        return errno::EINVAL;
    }
    let id = id as usize;
    let r = with_vm(|v| if v.created[id] { errno::EEXIST } else { 0 });
    if r != 0 {
        return r;
    }
    let Some(vc) = Vcpu::new(id).and_then(boxed) else { return errno::ENOMEM };
    let pas = [vc[0].run[0].pa().as_usize(), vc[0].run[1].pa().as_usize()];
    let r = with_vm(|v| {
        v.created[id] = true;
        v.run_pas[id] = pas;
        v.vcpus[id] = Some(vc);
        0
    });
    if r != 0 {
        return r;
    }
    new_handle(Kind::Vcpu, id as u8)
}

/// One fallible heap allocation holding `v` (the vCPU is kept off the kernel stack: 32 KiB is all there is).
/// The result stays a one-element array: turning it into `Box<T>` is a pointer cast, and
/// indexing `[0]` costs nothing.
fn boxed<T>(v: T) -> Option<Box<[T; 1]>> {
    let mut one = alloc::vec::Vec::new();
    one.try_reserve_exact(1).ok()?;
    one.push(v);
    one.into_boxed_slice().try_into().ok()
}

/// Run `f` on vCPU `id` if it is here (not running on another thread).
fn with_vcpu(id: u8, f: impl FnOnce(&mut Vcpu, &mut Platform) -> u64) -> u64 {
    irq_off(|| {
        let mut vm = VM.lock();
        let mut plat = PLAT.lock();
        let (Some(v), Some(p)) = (vm.as_mut(), plat.as_mut()) else { return errno::EINVAL };
        match v.vcpus[id as usize].as_mut() {
            Some(vc) => f(&mut vc[0], p),
            None => errno::EBUSY,
        }
    })
}

fn user_bytes<const N: usize>(arg: u64) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg, &mut b).then_some(b)
}

fn run_vcpu(id: u8) -> u64 {
    let taken = irq_off(|| {
        let mut g = VM.lock();
        let v = g.as_mut()?;
        if v.npt.is_none() {
            v.npt = build_npt(&v.slots, &mut v.ram);
        }
        let npt = match v.npt.as_ref()? {
            Ok(n) => Arc::clone(n),
            Err(_) => return Some(Err(())),
        };
        let mut vc = v.vcpus[id as usize].take()?;
        vc[0].refresh_events(&v.ioeventfds, &v.irqfds);
        Some(Ok((vc, v.slots, npt)))
    });
    let (mut vc, slots, npt) = match taken {
        None => return errno::EBUSY,
        Some(Err(())) => {
            serial::puts("[kvmapi] the nested page table could not map every memslot\n");
            return errno::ENOMEM;
        }
        Some(Ok(t)) => t,
    };
    if irq_off(|| PLAT.lock().is_none()) {
        irq_off(|| {
            if let Some(v) = VM.lock().as_mut() {
                v.vcpus[id as usize] = Some(vc);
            }
        });
        return errno::EINVAL;
    }
    let r = vcpu::run(&mut vc[0], &PLAT, &slots, &npt);
    irq_off(|| {
        if let Some(v) = VM.lock().as_mut() {
            v.vcpus[id as usize] = Some(vc);
        }
    });
    r
}

/// `mmap` of a vCPU descriptor: the run page(s) as shared memory.
#[must_use]
pub fn mmap_vcpu(id: u32, addr: u64, len: u64, prot: u64, flags: u64) -> u64 {
    let Some(h) = irq_off(|| HANDLES.lock().get(id as usize - 1).copied().flatten()) else { return errno::EBADF };
    if h.kind != Kind::Vcpu {
        return errno::ENODEV;
    }
    let pages = (len as usize).div_ceil(4096);
    if pages == 0 || pages > vcpu::RUN_PAGES {
        return errno::EINVAL;
    }
    let pas = irq_off(|| VM.lock().as_ref().map(|v| v.run_pas[h.vcpu as usize]));
    let Some(pas) = pas else { return errno::EINVAL };
    akuma_kernel_amd64::drivers::services::mm::mmap_frames(addr, flags as u32, prot as u32, &pas[..pages])
}

fn set_memory_region(arg: u64) -> u64 {
    let mut raw = [0u8; slots::ARG_LEN];
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg, &mut raw) {
        return errno::EFAULT;
    }
    let Some(r) = Region::parse(&raw) else { return errno::EFAULT };
    let checked = with_vm(|v| {
        if v.slots.get(r.slot).is_some() {
            return errno::EEXIST; // v1: a slot is set once
        }
        match v.slots.check(&r) {
            Ok(()) => 0,
            Err(slots::Refused::Overlap) => errno::EEXIST,
            Err(_) => errno::EINVAL,
        }
    });
    if checked != 0 {
        return checked;
    }
    let ram = match reserve::carve(r.size) {
        reserve::Carve::Ram(ram) => ram,
        reserve::Carve::NoReserve => {
            serial::puts("[kvmapi] guest RAM needs the kvmmem= reserve\n");
            return errno::ENOMEM;
        }
        reserve::Carve::Refused { .. } => {
            serial::puts("[kvmapi] guest RAM does not fit the kvmmem= reserve\n");
            return errno::ENOMEM;
        }
    };
    let pa = ram.pa();
    if !akuma_kernel_amd64::drivers::services::mm::remap_user_to_phys(r.uaddr as usize, r.size as usize, pa as usize) {
        return errno::ENOMEM;
    }
    // The VMM's range now lives on the slice: from here on it may only be given back when the VM
    // dies (freeing it under a live mapping would hand the VMM another guest's memory).
    let mut ram = Some(ram);
    let set = with_vm(|v| match v.slots.set(r, pa) {
        Ok(()) => {
            v.ram[r.slot as usize] = ram.take();
            0
        }
        Err(_) => errno::EINVAL,
    });
    if let Some(orphan) = ram {
        // Refused (or the VM is gone): no table holds it, so it is leaked for good.
        core::mem::forget(orphan);
    }
    set
}

fn irqfd(arg: u64) -> u64 {
    let mut raw = [0u8; IRQFD_LEN];
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg, &mut raw) {
        return errno::EFAULT;
    }
    let Some(a) = Irqfd::parse(&raw) else { return errno::EFAULT };
    let deassign = a.flags & 1 != 0;
    let id = akuma_kernel_amd64::drivers::services::fd::eventfd_id_of(u64::from(a.fd));
    with_vm(|v| {
        if deassign {
            for s in &mut v.irqfds {
                if let Some((x, eid)) = *s
                    && x.fd == a.fd
                    && x.gsi == a.gsi
                {
                    akuma_syscalls_glue::eventfd::eventfd_close(eid);
                    *s = None;
                }
            }
            return 0;
        }
        let Some(eid) = id else { return errno::EBADF };
        match v.irqfds.iter_mut().find(|s| s.is_none()) {
            Some(s) => {
                akuma_syscalls_glue::eventfd::eventfd_clone_ref(eid);
                *s = Some((a, eid));
                0
            }
            None => errno::ENOMEM,
        }
    })
}

fn ioeventfd(arg: u64) -> u64 {
    let mut raw = [0u8; IOEVENTFD_LEN];
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg, &mut raw) {
        return errno::EFAULT;
    }
    let Some(a) = Ioeventfd::parse(&raw) else { return errno::EFAULT };
    let id = akuma_kernel_amd64::drivers::services::fd::eventfd_id_of(a.fd.cast_unsigned().into());
    with_vm(|v| {
        if a.flags & IOEVENTFD_DEASSIGN != 0 {
            for s in &mut v.ioeventfds {
                if let Some((x, eid)) = *s
                    && x.addr == a.addr
                    && x.fd == a.fd
                {
                    akuma_syscalls_glue::eventfd::eventfd_close(eid);
                    *s = None;
                }
            }
            return 0;
        }
        let Some(eid) = id else { return errno::EBADF };
        match v.ioeventfds.iter_mut().find(|s| s.is_none()) {
            Some(s) => {
                akuma_syscalls_glue::eventfd::eventfd_clone_ref(eid);
                *s = Some((a, eid));
                0
            }
            None => errno::ENOMEM,
        }
    })
}

/// `ioctl(fd, req, arg)` on a `/dev/kvm` descriptor with handle `id`.
pub fn ioctl(fd: u64, id: u32, req: u32, arg: u64) -> u64 {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    // `RUN` is the hot path: log the first few, then only count (a line per exit would be the cost).
    if req == 0xAE80 && n > RUN_LOG_FIRST + 40 {
        if RUN_CALLS.fetch_add(1, Ordering::Relaxed) < 8 {
            serial::puts("[kvmapi] RUN called\n");
        }
        return match irq_off(|| HANDLES.lock().get(id as usize - 1).copied().flatten()) {
            Some(h) if h.kind == Kind::Vcpu => run_vcpu(h.vcpu),
            _ => errno::EBADF,
        };
    }
    let h = irq_off(|| HANDLES.lock().get(id as usize - 1).copied().flatten());
    let size = ((req >> 16) & 0x3fff) as usize;
    if size > 0 && req >> 30 & 1 != 0 {
        serial::puts("[kvmapi]   in: ");
        dump(arg, var_len(arg, req).unwrap_or_else(|| size.min(288)));
    }
    let r = match h {
        None => errno::EBADF,
        Some(h) => match req::decode(req, h.kind) {
            Some(op) => answer(h, op, arg),
            None => errno::ENOTTY,
        },
    };
    serial::puts("[kvmapi] #");
    serial::put_dec(u64::from(n));
    serial::puts(" fd=");
    serial::put_dec(fd);
    serial::puts(" req=0x");
    serial::put_hexn(u64::from(req), 8);
    serial::puts(" nr=0x");
    serial::put_hexn(u64::from(req & 0xff), 2);
    serial::puts(" arg=0x");
    serial::put_hexn(arg, 16);
    serial::puts(" -> ");
    if r.cast_signed() < 0 {
        serial::puts("-");
        serial::put_dec(r.cast_signed().unsigned_abs());
    } else {
        serial::put_dec(r);
    }
    serial::puts("\n");
    if size > 0 && req >> 30 & 2 != 0 {
        serial::puts("[kvmapi]   out: ");
        dump(arg, var_len(arg, req).unwrap_or_else(|| size.min(288)));
    }
    r
}

/// Upper bound on entries offered.
const MAX_LEAVES: usize = 128;

/// The (leaf, subleaf) pairs offered: every basic and extended leaf the host has, with the subleaves
/// of the leaves that have them. Values are the host's, through the guest filter.
fn leaves(out: &mut [(u32, u32); MAX_LEAVES]) -> usize {
    use core::arch::x86_64::__cpuid_count as cpuid;
    let mut n = 0;
    let mut push = |l: u32, s: u32| {
        if n < MAX_LEAVES {
            out[n] = (l, s);
            n += 1;
        }
    };
    let basic = cpuid(0, 0).eax.min(0x20);
    for l in 0..=basic {
        let subs = match l {
            4 => 5,
            7 => cpuid(7, 0).eax.min(2) + 1,
            0xb => 3,
            0xd => 3,
            _ => 1,
        };
        for s in 0..subs {
            push(l, s);
        }
    }
    let ext = cpuid(0x8000_0000, 0).eax.min(0x8000_0021);
    for l in 0x8000_0000..=ext {
        let subs = if l == 0x8000_001d { 5 } else { 1 };
        for s in 0..subs {
            push(l, s);
        }
    }
    n
}

fn supported_cpuid(arg: u64) -> u64 {
    let Some(nent) = akuma_kernel_amd64::drivers::services::uaccess::read_val::<u32>(arg) else {
        return errno::EFAULT;
    };
    let mut table = [(0u32, 0u32); MAX_LEAVES];
    let n = leaves(&mut table);
    if (nent as usize) < n {
        let _ = akuma_kernel_amd64::drivers::services::uaccess::write_val::<u32>(arg, n as u32);
        return (-7i64) as u64; // E2BIG
    }
    for (i, &(leaf, sub)) in table[..n].iter().enumerate() {
        let r = core::arch::x86_64::__cpuid_count(leaf, sub);
        let f = akuma_svm_amd64::cpuid::filter(leaf, [r.eax, r.ebx, r.ecx, r.edx]);
        let mut bytes = [0u8; CPUID_ENTRY_LEN];
        CpuidEntry { function: leaf, index: sub, flags: 0, eax: f[0], ebx: f[1], ecx: f[2], edx: f[3] }.encode(&mut bytes);
        if !akuma_kernel_amd64::drivers::services::uaccess::write_bytes(arg + LIST_HDR as u64 + (i * CPUID_ENTRY_LEN) as u64, &bytes) {
            return errno::EFAULT;
        }
    }
    let _ = akuma_kernel_amd64::drivers::services::uaccess::write_val::<u32>(arg, n as u32);
    0
}

/// The bytes of the argument structure (first `n`), eight words per line.
fn dump(arg: u64, n: usize) {
    let mut b = [0u8; 288];
    if !akuma_kernel_amd64::drivers::services::uaccess::read_bytes(arg, &mut b[..n]) {
        serial::puts("(arg unreadable)\n");
        return;
    }
    for (i, c) in b[..n].iter().enumerate() {
        serial::put_hexn(u64::from(*c), 2);
        if i % 4 == 3 {
            serial::puts(" ");
        }
    }
    serial::puts("\n");
}

/// Log an `mmap` of a `/dev/kvm` descriptor (fd, address hint, length, prot, flags, offset).
pub fn note_mmap(fd: u64, addr: u64, len: u64, prot: u64, flags: u64, off: u64) {
    serial::puts("[kvmapi] mmap fd=");
    serial::put_dec(fd);
    serial::puts(" addr=0x");
    serial::put_hexn(addr, 16);
    serial::puts(" len=0x");
    serial::put_hexn(len, 8);
    serial::puts(" prot=");
    serial::put_dec(prot);
    serial::puts(" flags=0x");
    serial::put_hexn(flags, 4);
    serial::puts(" off=0x");
    serial::put_hexn(off, 8);
    serial::puts("\n");
}

/// How much of a variable-length argument to dump: the 8-byte header plus up to six entries.
fn var_len(arg: u64, req: u32) -> Option<usize> {
    let (stride, _) = match req & 0xff00_ffff {
        0x4000_ae90 | 0xc000_ae05 | 0x4000_ae91 => (40usize, 0),
        0x4000_ae89 | 0xc000_ae88 => (16, 0),
        _ => return None,
    };
    let n = akuma_kernel_amd64::drivers::services::uaccess::read_val::<u32>(arg)? as usize;
    Some(8 + stride * n.min(6))
}
