//! `/dev/kvm0`: the control surface of the SVM hypervisor, and the one VM slot behind it.
//!
//! herd (or anything with root) brings a guest up by **writing a spec** (`akuma-vm-spec`'s
//! `key=value` text) to the device, ends it by writing `stop`, and reads the guest's console
//! back. The slot's lifetime is the *owner process's*: the process that created the VM is its
//! lease-holder, and a VM whose owner is gone (killed, crashed, `herd stop`) is stopped by the
//! run loop. That is what lets herd supervise a VM with nothing but a pid — the helper it
//! spawns (`herd vm-run`) sits on this device, so "the VM is running while its task is", and a
//! kill of that task ends the guest and triggers herd's restart policy.
//!
//! | write | |
//! |---|---|
//! | a spec | `create`: parsed whole, refused whole (`EINVAL`/`EOPNOTSUPP`/`EBUSY`/`ENODEV`/`ENOMEM`) |
//! | `stop` | ask the running guest to end; the slot reads `Ended` once it has |
//! | `status` | append one `[kvm] status:` line to the console stream |
//!
//! `read` drains the console stream (guest serial plus the hypervisor's own `[KVM]` lines for
//! this VM): bytes, `EAGAIN` while the guest is alive and quiet, `0` (EOF) once it has ended
//! and the stream is empty. A single reader is assumed: the cursor is the device's, not the
//! descriptor's.
//!
//! The control surface is a character device rather than a syscall so a Firecracker-shaped
//! `/dev/kvm` can later sit beside it, and rather than a `/proc` file because it takes effects.
//! `ls /dev` does not list it (the `/dev/power` precedent: opened by the kernel's own `openat`
//! arm).
//!
//! **No allocation anywhere here**: the spec is a `Copy` value in the slot, the console is a
//! static ring, every message is formatted on the stack.
//!
//! Guest RAM comes from a block reserved at boot (`kvmmem=<MiB>`) and sliced per VM by
//! `akuma_vm_spec::arena`; a request that does not fit is **refused**. Without `kvmmem=` the
//! boot-flag guests fall back to the page allocator, as they always did.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use akuma_dmesg::Ring;
use akuma_vm_spec::{arena::Arena, spec::MAX_WIRE, Spec};
use spinning_top::Spinlock;

use crate::fd::errno;

/// Console stream capacity: the helper drains every 100 ms, a boot is ~60 KiB over ~15 s.
const CONSOLE_CAP: usize = 32 << 10;
/// Bytes handed to one `read`.
const READ_CHUNK: usize = 512;
/// How often the run loop re-checks that the owner process is alive (µs).
const LEASE_CHECK_US: u64 = 200_000;
/// Slice alignment: guest RAM is mapped by 2 MiB nested-page-table leaves.
const SLICE_ALIGN: u64 = 2 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// No VM has been asked for.
    Idle,
    /// Created, waiting for the VM task to pick it up.
    Pending,
    Running,
    /// The guest ended (cleanly, by `stop`, by its owner's death, or it never started).
    Ended,
}

impl Phase {
    const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Ended => "ended",
        }
    }
}

struct Slot {
    phase: Phase,
    spec: Option<Spec>,
    /// The creating process (tgid), or 0 for a VM the boot flags asked for: no lease.
    owner: u32,
    started_us: u64,
    /// How many VMs this boot has run (`status`).
    generation: u32,
}

static SLOT: Spinlock<Slot> = Spinlock::new(Slot { phase: Phase::Idle, spec: None, owner: 0, started_us: 0, generation: 0 });

struct Console {
    ring: Ring<CONSOLE_CAP>,
    /// Absolute bytes the reader has taken.
    consumed: u64,
}

static CONSOLE: Spinlock<Console> = Spinlock::new(Console { ring: Ring::new(), consumed: 0 });

/// Set by `stop`; cleared at `create`.
static STOP: AtomicBool = AtomicBool::new(false);
/// While set, the hypervisor's console output goes to [`CONSOLE`] instead of the host's serial
/// line and dmesg: a VM brought up through the device is logged by whoever supervises it.
static ATTACHED: AtomicBool = AtomicBool::new(false);
/// The VM task exists (spawned on the first `create`; it serves every later VM).
static TASK_UP: AtomicBool = AtomicBool::new(false);
static LEASE_CHECKED_US: AtomicU64 = AtomicU64::new(0);
/// Console bytes lost to the ring wrapping before the reader came (`status`).
static LOST: AtomicU32 = AtomicU32::new(0);

fn irq_off<T>(f: impl FnOnce() -> T) -> T {
    akuma_primitives::irq::with_irqs_disabled(f)
}

// ---------------------------------------------------------------------------
// Console stream
// ---------------------------------------------------------------------------

/// Is the VM's output going to the stream (rather than the host console)?
pub(super) fn attached() -> bool {
    ATTACHED.load(Ordering::Acquire)
}

pub(super) fn emit(s: &str) {
    irq_off(|| CONSOLE.lock().ring.push_bytes(s.as_bytes()));
}

pub(super) fn emit_dec(mut v: u64) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    irq_off(|| CONSOLE.lock().ring.push_bytes(&d[i..]));
}

pub(super) fn emit_hex(v: u64, nibbles: u32) {
    let mut d = [0u8; 16];
    let n = (nibbles as usize).clamp(1, 16);
    for (k, slot) in d[..n].iter_mut().enumerate() {
        let sh = 4 * (n - 1 - k) as u32;
        *slot = b"0123456789abcdef"[((v >> sh) & 0xf) as usize];
    }
    irq_off(|| CONSOLE.lock().ring.push_bytes(&d[..n]));
}

/// A stack writer for one status line.
struct Line {
    buf: [u8; 192],
    n: usize,
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.buf.len() - self.n;
        let take = s.len().min(room);
        self.buf[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}

fn status_line() {
    use core::fmt::Write as _;
    let (phase, spec, owner, started, generation) = irq_off(|| {
        let s = SLOT.lock();
        (s.phase, s.spec, s.owner, s.started_us, s.generation)
    });
    let mut l = Line { buf: [0; 192], n: 0 };
    let up_s = if phase == Phase::Running { crate::net::uptime_us().saturating_sub(started) / 1_000_000 } else { 0 };
    let _ = write!(l, "[kvm] status: state={} gen={generation} owner={owner} up_s={up_s}", phase.name());
    if let Some(s) = spec {
        let _ = write!(l, " type={} vcpus={} ram_mib={} forwards={}", s.backend.name(), s.vcpus, s.ram_mib, s.forwards().len());
    }
    if let Some((used, total)) = reserve_usage() {
        let _ = write!(l, " reserve_mib={}/{}", used >> 20, total >> 20);
    }
    let lost = LOST.load(Ordering::Relaxed);
    let _ = writeln!(l, " console_lost={lost}");
    emit(core::str::from_utf8(&l.buf[..l.n]).unwrap_or("[kvm] status: ?\n"));
}

// ---------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------

/// `write(fd, buf, len)` on `/dev/kvm0`.
pub fn dev_write(buf: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > MAX_WIRE {
        return errno::EINVAL;
    }
    let mut raw = [0u8; MAX_WIRE];
    if !crate::uaccess::read_bytes(buf, &mut raw[..len]) {
        return errno::EFAULT;
    }
    let Ok(text) = core::str::from_utf8(&raw[..len]) else { return errno::EINVAL };
    match text.trim() {
        "stop" => {
            STOP.store(true, Ordering::Release);
            // A VM that was created but never picked up ends here too.
            irq_off(|| {
                let mut s = SLOT.lock();
                if s.phase == Phase::Pending {
                    s.phase = Phase::Ended;
                }
            });
            len as u64
        }
        "status" => {
            status_line();
            len as u64
        }
        _ => create(text, len),
    }
}

fn create(text: &str, len: usize) -> u64 {
    let spec = match Spec::parse_wire(text) {
        Ok(s) => s,
        Err((e, line)) => {
            super::p("[KVM] kvm0: spec refused: ");
            super::p(e.describe());
            if line != 0 {
                super::p(" (line ");
                super::dec(line as u64);
                super::p(")");
            }
            super::p("\n");
            return errno::EINVAL;
        }
    };
    if !spec.backend.is_supported() {
        return errno::EOPNOTSUPP;
    }
    if !super::svm_armed() {
        return errno::ENODEV;
    }
    let owner = crate::usermode::current_process().map_or(0, |p| p.tgid);
    let started = irq_off(|| {
        let mut s = SLOT.lock();
        if matches!(s.phase, Phase::Pending | Phase::Running) {
            return false;
        }
        s.phase = Phase::Pending;
        s.spec = Some(spec);
        s.owner = owner;
        s.generation = s.generation.wrapping_add(1);
        true
    });
    if !started {
        return errno::EBUSY;
    }
    STOP.store(false, Ordering::Release);
    irq_off(|| {
        let mut c = CONSOLE.lock();
        c.ring.clear();
        c.consumed = 0;
    });
    LOST.store(0, Ordering::Relaxed);
    ATTACHED.store(true, Ordering::Release);
    if !ensure_task() {
        ATTACHED.store(false, Ordering::Release);
        irq_off(|| SLOT.lock().phase = Phase::Ended);
        return errno::ENOMEM;
    }
    len as u64
}

/// `read(fd, buf, len)` on `/dev/kvm0`: console bytes, `EAGAIN`, or 0 at the end.
pub fn dev_read(buf: u64, len: usize) -> u64 {
    let mut tmp = [0u8; READ_CHUNK];
    let want = len.min(READ_CHUNK);
    let n = irq_off(|| {
        let mut c = CONSOLE.lock();
        let dropped = c.ring.dropped();
        if c.consumed < dropped {
            LOST.fetch_add((dropped - c.consumed).min(u64::from(u32::MAX)) as u32, Ordering::Relaxed);
            c.consumed = dropped;
        }
        let skip = (c.consumed - dropped) as usize;
        c.ring.snapshot_from(skip, &mut tmp[..want])
    });
    if n == 0 {
        let phase = irq_off(|| SLOT.lock().phase);
        return if matches!(phase, Phase::Pending | Phase::Running) { errno::EAGAIN } else { 0 };
    }
    if !crate::uaccess::write_bytes(buf, &tmp[..n]) {
        return errno::EFAULT;
    }
    irq_off(|| CONSOLE.lock().consumed += n as u64);
    n as u64
}

// ---------------------------------------------------------------------------
// The VM task
// ---------------------------------------------------------------------------

/// Spawn the VM task once, pinned to a core that has SVM enabled: the first of `kvmcores=`
/// that does (core 0 when the parameter is absent). **Every virtual processor of every VM runs
/// in this one task, taking turns**; one task per virtual processor, each on its own core, is
/// the multi-vCPU step (`docs/reference/subsystems/kvm.md`).
fn ensure_task() -> bool {
    if TASK_UP.load(Ordering::Acquire) {
        return true;
    }
    let Some(core) = akuma_vm_spec::cores::first(super::vm_cores(), |c| super::core_ready(c as usize)) else {
        super::p("[KVM] kvm0: no core in kvmcores= has SVM enabled\n");
        return false;
    };
    let Some(slot) = crate::sched::spawn_daemon_on(vm_task, core as usize) else {
        super::p("[KVM] kvm0: no task slot for the VM task\n");
        return false;
    };
    TASK_UP.store(true, Ordering::Release);
    super::p("[KVM] kvm0: VM task spawned in slot ");
    super::dec(slot as u64);
    super::p(" on core ");
    super::dec(u64::from(core));
    super::p("\n");
    true
}

/// Take the pending spec, if any, marking it running.
fn take_pending() -> Option<Spec> {
    irq_off(|| {
        let mut s = SLOT.lock();
        if s.phase != Phase::Pending {
            return None;
        }
        s.phase = Phase::Running;
        s.started_us = crate::net::uptime_us();
        s.spec
    })
}

extern "C" fn vm_task() -> ! {
    loop {
        if let Some(spec) = take_pending() {
            LEASE_CHECKED_US.store(0, Ordering::Relaxed);
            super::linux::run(super::nrips(), &super::linux::Vm::from_spec(spec));
            end_of_run();
        }
        // Idle: a tick is the latency of a `create`.
        crate::sched::yield_now();
        crate::sched::allow_tick();
    }
}

/// The run returned (for any reason): publish `Ended` last, after the final console bytes.
fn end_of_run() {
    super::p("[KVM] kvm0: guest ended\n");
    ATTACHED.store(false, Ordering::Release);
    irq_off(|| SLOT.lock().phase = Phase::Ended);
}

/// Boot-flag guests (`kvmserve`, `kvmgentoo`): the same slot with no owner, so no lease, and
/// the console stays on the host's. `spawn_service` builds the spec.
pub(super) fn create_unowned(spec: &Spec) -> bool {
    let ok = irq_off(|| {
        let mut s = SLOT.lock();
        if matches!(s.phase, Phase::Pending | Phase::Running) {
            return false;
        }
        s.phase = Phase::Pending;
        s.spec = Some(*spec);
        s.owner = 0;
        s.generation = s.generation.wrapping_add(1);
        true
    });
    ok && ensure_task()
}

/// Called every turn of a service guest's run loop: has it been asked to end, or has its owner
/// gone? The owner check is throttled; a process lookup is not free.
pub(super) fn should_stop() -> bool {
    if STOP.load(Ordering::Acquire) {
        return true;
    }
    let now = crate::net::uptime_us();
    if now.saturating_sub(LEASE_CHECKED_US.load(Ordering::Relaxed)) < LEASE_CHECK_US {
        return false;
    }
    LEASE_CHECKED_US.store(now, Ordering::Relaxed);
    let owner = irq_off(|| SLOT.lock().owner);
    owner != 0 && akuma_exec::process::lookup_process_shared(owner).is_none()
}

// ---------------------------------------------------------------------------
// Reserved guest RAM
// ---------------------------------------------------------------------------

/// The block is the kernel's for good (never returned to the page allocator), so only its
/// aligned base and the arena over it are kept.
struct Reserve {
    /// Host-physical, 2 MiB-aligned.
    base: u64,
    arena: Arena,
}

static RESERVE: Spinlock<Option<Reserve>> = Spinlock::new(None);

/// Take `mib` MiB of contiguous memory for guests (`kvmmem=`), while memory is unfragmented.
pub(super) fn reserve_init(mib: u64) {
    let bytes = mib << 20;
    let alloc_pages = ((bytes + SLICE_ALIGN) / 4096) as usize;
    let Some(alloc_pa) = akuma_pmm::alloc_pages_contiguous_zeroed(alloc_pages) else {
        super::p("[KVM] kvmmem: no contiguous block of that size; guests use the page allocator\n");
        return;
    };
    let base = (alloc_pa as u64 + SLICE_ALIGN - 1) & !(SLICE_ALIGN - 1);
    irq_off(|| *RESERVE.lock() = Some(Reserve { base, arena: Arena::new(bytes) }));
    super::p("[KVM] kvmmem: reserved ");
    super::dec(mib);
    super::p(" MiB for guests\n");
}

/// Where a slice of guest memory came from.
#[derive(Clone, Copy)]
pub(super) enum Carve {
    /// A slice of the reserve at host-physical `pa`; `off` is its handle for [`uncarve`].
    Reserved { pa: u64, off: u64 },
    /// No reserve was configured: the caller uses the page allocator.
    Pmm,
    /// A reserve exists and has no room: **refuse** (no silent fallback).
    Refused { free: u64 },
}

/// Slice `bytes` (a multiple of 4 KiB) out of the reserve, zeroed.
pub(super) fn carve(bytes: u64) -> Carve {
    let got = irq_off(|| {
        let mut g = RESERVE.lock();
        let r = g.as_mut()?;
        Some(match r.arena.alloc(bytes, SLICE_ALIGN) {
            Some(off) => Ok((r.base + off, off)),
            None => Err(r.arena.total() - r.arena.used()),
        })
    });
    match got {
        None => Carve::Pmm,
        Some(Err(free)) => Carve::Refused { free },
        Some(Ok((pa, off))) => {
            zero_phys(pa, bytes);
            Carve::Reserved { pa, off }
        }
    }
}

pub(super) fn uncarve(off: u64) {
    irq_off(|| {
        if let Some(r) = RESERVE.lock().as_mut() {
            r.arena.free(off);
        }
    });
}

/// Reserve bytes in use and in total, for `status`.
pub(super) fn reserve_usage() -> Option<(u64, u64)> {
    irq_off(|| RESERVE.lock().as_ref().map(|r| (r.arena.used(), r.arena.total())))
}

fn zero_phys(pa: u64, bytes: u64) {
    let z = [0u8; 4096];
    let mut off = 0;
    while off < bytes {
        let _ = akuma_mmu::copy_to_phys((pa + off) as usize, &z);
        off += 4096;
    }
}
