//! SMP bring-up (stage S-b): the MADT names every enabled LAPIC; the BSP sends
//! each one INIT then STARTUP; the AP starts in real mode on the trampoline
//! page `akuma-boot-amd64`'s `boot.s` carries, walks up to long mode on a
//! page-table root built here, and lands in [`ap_entry64`].
//!
//! The twin of `amd64/src/smp.rs` (1,202 lines, welded to amd64's `sched` and
//! `lapic`); this file keeps only what bring-up needs, so the extraction into
//! `akuma-smp-amd64` has a small surface to move. Design: ring 3 runs in
//! parallel, every kernel entry takes one big lock (`akuma-bkl`), so the
//! kernel's statics stay single-writer (`taoxichuan/docs/reference/scheduling.md`
//! § SMP).
//!
//! Stage S-b ends with every AP **online and halted** (`cli; hlt`): no timer,
//! no task. Stage S-c gives them the picker.

#[cfg(feature = "smp")]
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use akuma_tokio_kernel_core::bkl;

/// Most cores the per-CPU tables hold (`akuma_ring3_amd64::MAX_CPUS`).
pub const MAX_CPUS: usize = akuma_ring3_amd64::MAX_CPUS;

#[cfg(feature = "smp")]
use akuma_boot_amd64::ap::{self, AP_MB_CR3, AP_MB_ENTRY, AP_MB_INDEX, AP_MB_STACK, AP_STARTUP_VECTOR, mailbox_write};
#[cfg(feature = "smp")]
/// An AP's boot stack is also its idle thread's: it runs `netpoll` (smoltcp) and
/// takes ticks that nest another `netpoll`, so it is far deeper than amd64's 32 KiB.
const AP_STACK_SIZE: usize = 128 * 1024;
#[cfg(feature = "smp")]
/// Microseconds to wait after INIT, and the budget (spins) for an AP to report.
const AP_INIT_DELAY_US: u64 = 10_000;
#[cfg(feature = "smp")]
const AP_ONLINE_BUDGET_US: u64 = 200_000;


static ONLINE: AtomicUsize = AtomicUsize::new(1);

/// The kernel's own PML4 (the boot root, recorded before any user space is
/// loaded). A core with nothing to run switches to it: an idle core must not
/// sit on a task's root, because another core can reap that task and free the
/// page tables underneath it.
static KERNEL_ROOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn record_bsp_lapic_id(id: u32) {
    LAPIC_OF[0].store(id, Ordering::Relaxed);
}

pub fn record_kernel_root() {
    KERNEL_ROOT.store(akuma_boot_amd64::paging::active_root(), Ordering::Relaxed);
}

#[must_use]
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

// ── the big kernel lock ─────────────────────────────────────────────────────
//
// `akuma_tokio_kernel_core::bkl`, the lock both machines run under; a waiting
// core services shootdowns ([`service_pending`]).

/// This machine as the core's lock sees it.
pub struct Amd64Cpu;

impl bkl::BklCpu for Amd64Cpu {
    fn index() -> usize {
        akuma_ring3_amd64::cpu_index()
    }
    fn while_waiting() {
        service_pending();
    }
    // Interrupts stay off for the whole wait (see `bkl::enter`): a tick that landed mid-wait would queue its own `enter` behind this one.
    fn irq_save() -> u64 {
        akuma_cpu::daif::save_and_mask_irq()
    }
    fn irq_restore(saved: u64) {
        akuma_cpu::daif::restore(saved);
    }
}

/// Does this core own the kernel lock? (A diagnostic for the switch.)
#[must_use]
pub fn lock_held() -> bool {
    bkl::held::<Amd64Cpu>()
}

/// Take the lock (or nest, if this core already owns it).
pub fn bkl_enter() {
    bkl::enter::<Amd64Cpu>();
}

/// Leave one level; release at zero.
pub fn bkl_leave() {
    bkl::leave::<Amd64Cpu>();
}

/// Release completely (ring 3 is next, or a halt), returning the depth held.
pub fn bkl_leave_all() -> u32 {
    bkl::leave_all::<Amd64Cpu>()
}

/// Run `f` with the lock released (a halt, a spin that waits on another core).
pub fn bkl_unlocked<R>(f: impl FnOnce() -> R) -> R {
    bkl::unlocked::<Amd64Cpu, R>(f)
}

/// `transfer_lock_depth`: see `akuma_tokio_kernel_core::bkl::transfer_depth`.
pub fn transfer_depth(from: usize, to: usize) {
    bkl::transfer_depth::<Amd64Cpu>(from, to);
}

pub use bkl::born_locked;

static AP_UP: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

// ── TLB shootdown ───────────────────────────────────────────────────────────
//
// `invlpg` and a CR3 reload are core-local. A mapping removed or tightened on
// one core stays usable on a peer that cached it until the peer is told, so
// the sender (always inside the kernel lock) sends [`SHOOT_VECTOR`] to every
// other online core, whose handler reloads CR3 and acknowledges a generation;
// the sender waits for every acknowledgement before it frees a frame or
// returns to ring 3. The handler takes no lock. A core spinning IRQ-masked in
// [`bkl_enter`] cannot take the IPI, so the spin services pending generations
// itself ([`service_pending`]) - without that, sender-waits-for-spinner and
// spinner-waits-for-sender is a deadlock.

/// The IDT vector of the shootdown IPI (the timer is 32).
pub const SHOOT_VECTOR: u8 = 33;
static SHOOT_GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SHOOT_PENDING: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];
static SHOOT_ACKED: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];
/// LAPIC id of each started core, for the IPI destination.
static LAPIC_OF: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// Service a pending shootdown on THIS core: a full non-global flush.
pub fn service_pending() {
    let me = akuma_ring3_amd64::cpu_index();
    let pending = SHOOT_PENDING[me].load(Ordering::Acquire);
    if pending == 0 || pending <= SHOOT_ACKED[me].load(Ordering::Relaxed) {
        return;
    }
    // Rewriting CR3 with its own value is the architectural full non-global flush; the root stays valid.
    akuma_cpu::ctlreg::flush_non_global();
    SHOOT_ACKED[me].store(pending, Ordering::Release);
}

/// The vector-33/34 handler body: service a pending shootdown on this core and acknowledge the interrupt.
/// Registered with the entry stub (`akuma_boot_amd64::shootdown`) before the vectors are installed.
pub fn shootdown_handler() {
    service_pending();
    akuma_lapic_amd64::eoi();
}

/// The entry stub's address, for `IDT.set(SHOOT_VECTOR, ..)`.
#[must_use]
pub fn shootdown_entry_addr() -> usize {
    akuma_boot_amd64::shootdown::set_handler(shootdown_handler);
    akuma_boot_amd64::shootdown::entry_addr()
}

/// Make every other online core drop its cached user translations, and wait
/// until each has. The caller holds the kernel lock. A no-op on one core.
pub fn shootdown() {
    if online_cpus() <= 1 {
        return;
    }
    let me = akuma_ring3_amd64::cpu_index();
    let generation = SHOOT_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    for idx in 0..MAX_CPUS {
        if idx != me && (idx == 0 || AP_UP[idx].load(Ordering::Acquire)) {
            SHOOT_PENDING[idx].store(generation, Ordering::Release);
        }
    }
    for idx in 0..MAX_CPUS {
        if idx != me && (idx == 0 || AP_UP[idx].load(Ordering::Acquire)) {
            akuma_lapic_amd64::send_fixed(LAPIC_OF[idx].load(Ordering::Relaxed), SHOOT_VECTOR);
        }
    }
    let mut spins = 0u64;
    for idx in 0..MAX_CPUS {
        if idx == me || !(idx == 0 || AP_UP[idx].load(Ordering::Acquire)) {
            continue;
        }
        while SHOOT_ACKED[idx].load(Ordering::Acquire) < generation {
            core::hint::spin_loop();
            spins += 1;
            if spins == 400_000_000 {
                akuma_primitives::safe_print!(96, "[shootdown] cpu {} never acknowledged gen {}\n", idx, generation);
            }
        }
    }
}

// ── the wake IPI ────────────────────────────────────────────────────────────
//
// A thread readied on one core while a peer sits in `hlt` used to wait for that
// peer's next 10 ms tick. A core announces it is about to halt ([`park_halt`]);
// a wake kicks one halted core with [`WAKE_VECTOR`], whose handler only
// acknowledges - arriving ends the `hlt`, and the idle lap re-runs the picker.

/// The wake IPI's vector (shares the shootdown stub: its `service_pending` is
/// a no-op when nothing is pending).
pub const WAKE_VECTOR: u8 = 34;
static HALTED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Halt this core until something wakes it - the tick, a device, or a wake IPI -
/// with the kernel lock released. Does not halt if work is already visible.
pub fn park_halt() {
    let me = akuma_ring3_amd64::cpu_index();
    bkl_unlocked(|| {
        HALTED[me].store(true, Ordering::SeqCst);
        if !akuma_threading::x86_runnable_exists() {
            akuma_cpu::park::wfi();
        }
        HALTED[me].store(false, Ordering::SeqCst);
    });
}

/// Kick one halted core other than this one (the `trigger_sgi` hook: a wake
/// just made a thread READY). A no-op on one core or when none is halted.
pub fn kick_halted_core() {
    if online_cpus() <= 1 {
        return;
    }
    let me = akuma_ring3_amd64::cpu_index();
    for idx in 0..MAX_CPUS {
        if idx != me && HALTED[idx].load(Ordering::SeqCst) && (idx == 0 || AP_UP[idx].load(Ordering::Acquire)) {
            akuma_lapic_amd64::send_fixed(LAPIC_OF[idx].load(Ordering::Relaxed), WAKE_VECTOR);
            return;
        }
    }
}

/// CPUs running the kernel: the BSP plus every AP that reported in.
#[must_use]
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
fn tsc_delay_us(us: u64) {
    let hz = akuma_lapic_amd64::tsc_hz().max(1);
    let end = akuma_cpu::tsc::rdtsc().wrapping_add(us * hz / 1_000_000);
    while akuma_cpu::tsc::rdtsc().wrapping_sub(end).cast_signed() < 0 {
        core::hint::spin_loop();
    }
}

#[cfg(feature = "smp")]
/// Copy the trampoline to its page (`akuma_boot_amd64::ap`). Returns false if it does not fit below the mailbox.
fn install_trampoline() -> bool {
    ap::install_trampoline_once()
}

#[cfg(feature = "smp")]
/// The APIC ids of every enabled CPU the firmware tables list, or `None`.
fn madt_cpus(cpus: &mut [u8; 32]) -> Option<usize> {
    use akuma_ryzen_amd64::acpi;
    let rsdp = acpi::find_rsdp(&crate::Physmap)?;
    let table = acpi::find_table(&crate::Physmap, &rsdp, b"APIC")?;
    let madt = acpi::parse_madt(&crate::Physmap, &table)?;
    let ids = madt.cpus();
    let n = ids.len().min(cpus.len());
    cpus[..n].copy_from_slice(&ids[..n]);
    Some(n)
}

#[cfg(feature = "smp")]
/// Start every AP the MADT lists, up to `cap` CPUs in total. Returns how many
/// APs came online. Serial, one core at a time through one mailbox.
pub fn start_secondaries(cap: usize) -> usize {
    let mut ids = [0u8; 32];
    let Some(n) = madt_cpus(&mut ids) else {
        akuma_primitives::safe_print!(64, "  smp: no MADT - single core\n");
        return 0;
    };
    let bsp_id = akuma_lapic_amd64::apic_id();
    let ap_ids: Vec<u32> = ids[..n].iter().map(|&i| u32::from(i)).filter(|&i| i != bsp_id).collect();
    if ap_ids.is_empty() || cap <= 1 {
        akuma_primitives::safe_print!(64, "  smp: {} cpu in the MADT, cap {} - single core\n", n, cap);
        return 0;
    }
    let root = ap::ap_boot_root(akuma_boot_amd64::paging::active_root());
    if root >= (4 << 30) {
        akuma_primitives::safe_print!(96, "  smp: [FAIL] AP boot root above 4 GiB\n");
        return 0;
    }
    if !install_trampoline() {
        akuma_primitives::safe_print!(96, "  smp: [FAIL] trampoline does not fit its page\n");
        return 0;
    }
    let mut started = 0usize;
    for (i, &id) in ap_ids.iter().enumerate() {
        let idx = i + 1;
        if idx >= MAX_CPUS || 1 + started >= cap {
            break;
        }
        let stack = alloc::vec![0u8; AP_STACK_SIZE].leak();
        let stack_top = (stack.as_ptr() as usize + AP_STACK_SIZE) & !0xf;
        // The AP's idle thread: a context that exists and is about to run.
        // Adopted (ON_CPU latched) before the core starts so no peer picks up
        // a stack this core is executing; its running slot is preset.
        LAPIC_OF[idx].store(id, Ordering::Relaxed);
        let idle = crate::exec_twin::idle_tid(idx);
        crate::sched::adopt_idle(idle, idx);
        akuma_ring3_amd64::preset_cpu_slot(idx, idle as u64);
        mailbox_write(AP_MB_CR3, root);
        mailbox_write(AP_MB_STACK, (stack_top - 8) as u64);
        #[allow(function_casts_as_integer)]
        mailbox_write(AP_MB_ENTRY, ap_entry64 as usize as u64);
        mailbox_write(AP_MB_INDEX, idx as u64);
        core::sync::atomic::fence(Ordering::SeqCst);

        akuma_lapic_amd64::send_init(id);
        tsc_delay_us(AP_INIT_DELAY_US);
        akuma_lapic_amd64::send_startup(id, AP_STARTUP_VECTOR);
        if !wait_online(idx) {
            akuma_lapic_amd64::send_startup(id, AP_STARTUP_VECTOR);
            tsc_delay_us(AP_INIT_DELAY_US);
            if !wait_online(idx) {
                akuma_primitives::safe_print!(96, "  smp: [FAIL] cpu {} (lapic id {}) did not come online\n", idx, id);
                continue;
            }
        }
        started += 1;
    }
    ONLINE.store(1 + started, Ordering::Release);
    akuma_primitives::safe_print!(64, "  smp: {} cpus online\n", 1 + started);
    started
}

#[cfg(feature = "smp")]
fn wait_online(idx: usize) -> bool {
    let mut waited = 0u64;
    while !AP_UP[idx].load(Ordering::Acquire) {
        if waited >= AP_ONLINE_BUDGET_US {
            return false;
        }
        tsc_delay_us(100);
        waited += 100;
    }
    true
}

#[cfg(feature = "smp")]
/// Where the trampoline's `jmp` lands, in long mode on the stack the BSP
/// allocated, `rdi` = this core's index. Brings the core to "kernel code can
/// run here" (GDT/TSS, per-CPU block, IDT, SSE, syscall MSRs) and halts it.
extern "C" fn ap_entry64(idx: u64) -> ! {
    let idx = idx as usize;
    // Hardware leaves a started core's APIC enabled; a hypervisor's model may
    // not (Akuma's SVM guest starts non-boot vCPUs with the bit clear), and the
    // ID read just below would fault on an unmapped page. Set it, as the BSP does.
    akuma_cpu::msr::enable_apic_global();
    // The AP boot root's identity slot is no longer needed: nothing low runs.
    crate::gdt_init_cpu(idx);
    let _ = akuma_ring3_amd64::install_percpu(idx, akuma_lapic_amd64::apic_id());
    crate::idt_load();
    crate::enable_cpu_features();
    akuma_lapic_amd64::software_enable(0xFF);
    akuma_ring3_amd64::enable_syscalls_this_core();
    AP_UP[idx].store(true, Ordering::Release);
    // S-c: this core joins the scheduler. Its tick is armed (the BSP alone
    // advances the clock - `timer_trap`), and its idle thread is the one the
    // BSP adopted for it: the same loop the BSP's idle thread runs.
    akuma_lapic_amd64::start_timer();
    crate::exec_twin::idle_main()
}
