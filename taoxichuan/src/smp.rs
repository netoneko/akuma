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

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use akuma_boot_amd64::phys::phys_ptr;

/// Most cores the per-CPU tables hold (`akuma_ring3_amd64::MAX_CPUS`).
pub const MAX_CPUS: usize = akuma_ring3_amd64::MAX_CPUS;

const AP_TRAMPOLINE_PA: u64 = 0x8000;
const AP_STARTUP_VECTOR: u8 = (AP_TRAMPOLINE_PA >> 12) as u8;
/// Mailbox offsets, mirrored as `AP_MB_*` in `boot.s`.
const AP_MB_CR3: u64 = AP_TRAMPOLINE_PA + 0xF00;
const AP_MB_STACK: u64 = AP_TRAMPOLINE_PA + 0xF08;
const AP_MB_ENTRY: u64 = AP_TRAMPOLINE_PA + 0xF10;
const AP_MB_INDEX: u64 = AP_TRAMPOLINE_PA + 0xF18;
const AP_STACK_SIZE: usize = 32 * 1024;
const PTE_P_RW: u64 = 0x3;
/// Microseconds to wait after INIT, and the budget (spins) for an AP to report.
const AP_INIT_DELAY_US: u64 = 10_000;
const AP_ONLINE_BUDGET_US: u64 = 200_000;

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static __ap_pml4: u8;
    static __pdpt_low: u8;
}

static ONLINE: AtomicUsize = AtomicUsize::new(1);

// ── the big kernel lock ─────────────────────────────────────────────────────
//
// Ring 3 runs in parallel; every kernel entry holds this lock, so the kernel's
// statics stay single-writer (`BorrowCell`s keep working: they become a check
// that the lock is held). Re-entrant by owner core - a tick that lands while
// this core already holds it nests - and the *depth* is per thread: a switch
// leaves the lock with the core and hands the depth over (`transfer_depth`),
// except that a thread of depth 0 (an idle thread, a task in ring 3) runs
// with the lock released.

const NO_OWNER: usize = usize::MAX;
static BKL_OWNER: AtomicUsize = AtomicUsize::new(NO_OWNER);
/// The owner's recursion depth (only the owner writes it).
static BKL_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Per-thread (tid-indexed) depth, saved at a switch away.
const THREAD_SLOTS: usize = 128;
static THREAD_DEPTH: [AtomicU32; THREAD_SLOTS] = [const { AtomicU32::new(0) }; THREAD_SLOTS];

fn me() -> usize {
    akuma_ring3_amd64::cpu_index()
}

/// Take the lock (or nest, if this core already owns it).
pub fn bkl_enter() {
    let cpu = me();
    if BKL_OWNER.load(Ordering::Relaxed) == cpu {
        BKL_DEPTH.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut spins = 0u64;
    while BKL_OWNER
        .compare_exchange_weak(NO_OWNER, cpu, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
        spins += 1;
        if spins == 400_000_000 {
            akuma_primitives::safe_print!(
                96,
                "[BKL] cpu {} stuck: owner cpu {}\n",
                cpu,
                BKL_OWNER.load(Ordering::Relaxed) as i64
            );
        }
    }
    BKL_DEPTH.store(1, Ordering::Relaxed);
}

/// Leave one level; release at zero. A no-op from a core that does not own it.
pub fn bkl_leave() {
    if BKL_OWNER.load(Ordering::Relaxed) != me() {
        return;
    }
    let d = BKL_DEPTH.load(Ordering::Relaxed);
    if d > 1 {
        BKL_DEPTH.store(d - 1, Ordering::Relaxed);
    } else {
        BKL_DEPTH.store(0, Ordering::Relaxed);
        BKL_OWNER.store(NO_OWNER, Ordering::Release);
    }
}

/// Release completely (ring 3 is next, or a halt), returning the depth held.
pub fn bkl_leave_all() -> u32 {
    if BKL_OWNER.load(Ordering::Relaxed) != me() {
        return 0;
    }
    let d = BKL_DEPTH.swap(0, Ordering::Relaxed);
    BKL_OWNER.store(NO_OWNER, Ordering::Release);
    d
}

/// Take the lock back at depth `d` (from [`bkl_leave_all`]); `d == 0` is a no-op.
pub fn bkl_restore(d: u32) {
    if d == 0 {
        return;
    }
    bkl_enter();
    BKL_DEPTH.store(d, Ordering::Relaxed);
}

/// Run `f` with the lock released (a halt, a spin that waits on another core).
pub fn bkl_unlocked<R>(f: impl FnOnce() -> R) -> R {
    let d = bkl_leave_all();
    let r = f();
    bkl_restore(d);
    r
}

/// `X86ArchHooks::transfer_lock_depth`: called on the outgoing core with `from`
/// still current. Saves this core's depth into `from`, loads `to`'s.
pub fn transfer_depth(from: usize, to: usize) {
    let held = if BKL_OWNER.load(Ordering::Relaxed) == me() {
        BKL_DEPTH.load(Ordering::Relaxed)
    } else {
        0
    };
    if let Some(t) = THREAD_DEPTH.get(from) {
        t.store(held, Ordering::Relaxed);
    }
    let want = THREAD_DEPTH.get(to).map_or(0, |t| t.load(Ordering::Relaxed));
    if want == 0 {
        let _ = bkl_leave_all();
    } else if held == 0 {
        bkl_enter();
        BKL_DEPTH.store(want, Ordering::Relaxed);
    } else {
        BKL_DEPTH.store(want, Ordering::Relaxed);
    }
}

/// A freshly claimed kernel thread is born holding the lock at depth 1 (its
/// entry code is kernel code until it crosses to ring 3).
pub fn born_locked(tid: usize) {
    if let Some(t) = THREAD_DEPTH.get(tid) {
        t.store(1, Ordering::Relaxed);
    }
}
static AP_UP: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// CPUs running the kernel: the BSP plus every AP that reported in.
#[must_use]
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

fn tsc_delay_us(us: u64) {
    let hz = akuma_lapic_amd64::tsc_hz().max(1);
    let end = akuma_cpu::tsc::rdtsc().wrapping_add(us * hz / 1_000_000);
    while akuma_cpu::tsc::rdtsc().wrapping_sub(end).cast_signed() < 0 {
        core::hint::spin_loop();
    }
}

fn mailbox_write(pa: u64, v: u64) {
    // SAFETY: inside the trampoline page, which `start_secondaries` owns for
    // the duration of bring-up (RAM below 1 MiB, clear of the PVH block).
    unsafe { phys_ptr::<u64>(pa).write_volatile(v) };
}

/// Copy the active kernel PML4 into `__ap_pml4` with slot 0 pointing at
/// `boot.s`'s still-intact identity PDPT, so the trampoline has somewhere to
/// stand before it jumps high. Returns the root's physical address (it must be
/// below 4 GiB: CR3 is 32-bit in the trampoline's protected-mode phase).
fn ap_boot_root() -> u64 {
    // `.bss.pagetables` is linked VMA = LMA (low), so a symbol's address IS its
    // physical address (as in `amd64/src/smp.rs`).
    let root = (&raw const __ap_pml4) as u64;
    let identity = (&raw const __pdpt_low) as u64;
    let kernel_root = akuma_boot_amd64::paging::active_root();
    // SAFETY: both symbols are page-aligned pages in the kernel image
    // (`boot.s`), reachable through the physmap; `kernel_root` is the live
    // PML4, likewise inside it.
    unsafe {
        core::ptr::copy_nonoverlapping(phys_ptr::<u8>(kernel_root), phys_ptr::<u8>(root), 4096);
        phys_ptr::<u64>(root).write_volatile(identity | PTE_P_RW);
    }
    root
}

fn install_trampoline() -> bool {
    let (src, len) = {
        let s = &raw const ap_trampoline_start;
        let e = &raw const ap_trampoline_end;
        (s, e as usize - s as usize)
    };
    if len > (AP_MB_CR3 - AP_TRAMPOLINE_PA) as usize {
        return false;
    }
    // SAFETY: the destination page is RAM inside the physmap owned by bring-up.
    unsafe { core::ptr::copy_nonoverlapping(src, phys_ptr::<u8>(AP_TRAMPOLINE_PA), len) };
    true
}

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
    let root = ap_boot_root();
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

/// Where the trampoline's `jmp` lands, in long mode on the stack the BSP
/// allocated, `rdi` = this core's index. Brings the core to "kernel code can
/// run here" (GDT/TSS, per-CPU block, IDT, SSE, syscall MSRs) and halts it.
extern "C" fn ap_entry64(idx: u64) -> ! {
    let idx = idx as usize;
    // The AP boot root's identity slot is no longer needed: nothing low runs.
    crate::gdt_init_cpu(idx);
    let _ = akuma_ring3_amd64::install_percpu(idx, akuma_lapic_amd64::apic_id());
    crate::idt_load();
    crate::enable_cpu_features();
    akuma_lapic_amd64::software_enable(0xFF);
    akuma_ring3_amd64::enable_syscalls_this_core();
    AP_UP[idx].store(true, Ordering::Release);
    loop {
        // SAFETY: halting a core that has nothing to run (S-b); IF is clear.
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}
