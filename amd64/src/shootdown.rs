//! TLB shootdown: the IPI that makes `TlbTarget::AllCores` true on x86.
//!
//! `invlpg` and a `CR3` reload are core-local, and x86 has no broadcast
//! invalidation instruction — so an address space demoted on one core leaves
//! every peer holding stale translations until they are told. Since
//! `clone(CLONE_VM)` (2026-09-06) an address space is live on several cores at
//! once, and CoW `fork` demotes the **parent's** live PTEs; a peer with a stale
//! writable translation writes straight through a page it is supposed to fault
//! on. That is what `cowstale` measures.
//!
//! This module is the plumbing `akuma_mmu::flush_tlb_*`'s `AllCores` arms reach
//! through the hooks [`crate::boot::install_shared_sinks`] registers: a fixed
//! delivery-mode IPI on [`VECTOR`], a per-core handler that does a full flush
//! and acknowledges through a per-CPU mailbox, and a wait for every peer's
//! acknowledgement. The vocabulary (send at flush, wait at `TlbFlush::drop`)
//! and the deadlock argument live in `akuma-mmu`, next to
//! `set_shootdown_hooks`; this file owns only the effects.
//!
//! # The deadlock argument, restated from the sender's side
//!
//! The sender of a shootdown always holds the BKL (every flush call site is
//! kernel code; the page-fault path takes the BKL before servicing a user
//! fault for exactly this reason). The BKL is the outermost lock on this
//! target, so while the sender waits, a peer is one of:
//!
//! - in ring 3 — interrupts on, the IPI is delivered;
//! - in `hlt` — interrupts on, the IPI wakes the core;
//! - inside an interrupt handler — bounded, and the handlers (timer, this
//!   IPI) take no lock;
//! - spinning IRQ-masked in the BKL ticket wait — the one state that cannot
//!   make progress, which is why [`bkl_spin_assist`] is hooked into
//!   `akuma_bkl`'s acquire loop and acknowledges from inside the spin.
//!
//! A peer IRQ-masked on the address-space or regions lock cannot happen: taking
//! either requires the BKL, which the sender holds. Every peer therefore
//! acknowledges; the wait terminates.
//!
//! # Why the handler takes no lock
//!
//! A peer can be running ring 3 while the sender — holding the BKL — edits
//! page tables; that is the whole point. The IPI lands on that core, and its
//! handler may only do things that need no lock: a full flush is a rewrite of
//! this core's own `CR3` (non-global entries — every user entry — are
//! invalidated; the kernel's upper half never changes after boot), plus the
//! mailbox store and the `EOI`. Any peer state that loads another address
//! space afterwards flushes on the `CR3` write anyway.
//!
//! # The payload
//!
//! A generation counter and "flush everything" — no per-VA mailbox. A range
//! past `FULL_FLUSH_THRESHOLD` (512 pages) degrades to a full flush in
//! `akuma-mmu` already, so the finer payload would buy exactly one thing this
//! target has not needed: complexity on the path that must never wedge.
//! Handler and BKL-assist are idempotent — re-servicing a generation, or
//! servicing one twice (assist then delivered IPI), is a redundant flush, not
//! an error.

use crate::idt;
use crate::smp::{self, MAX_CPUS};
use crate::serial;

/// The TLB shootdown vector. 32 is the LAPIC timer, 0xFF spurious, the AP
/// STARTUP vector is a page number rather than an IDT entry — 33 is free.
pub const VECTOR: u8 = 33;

/// The generation the last broadcast asked for. Written only by the sender,
/// and there is only one sender at a time: sending requires kernel code,
/// which requires the BKL.
static GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Per-CPU mailbox: the generation this core has been asked to invalidate
/// through. Stored by the sender before the IPI goes out, read by the
/// handler **and** by the BKL spin assist — the assist is the point, since a
/// core IRQ-masked in the ticket wait cannot take the IPI at all.
static PENDING: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];

/// Per-CPU mailbox: the newest generation this core has flushed through.
/// `Release` on the store, `Acquire` on the sender's load: an acknowledgement
/// is a promise that every edit the generation's sender made is visible here
/// and invalidated.
static ACKED: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];

/// Shootdowns serviced per core — the proof the IPI arrives, reported as a
/// boot self-test note. Kept permanently: it is diagnostics, not a test-only
/// structure.
static SERVICED: [core::sync::atomic::AtomicU64; MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_CPUS];

/// Spins between "the wait is stuck" reports, mirroring the BKL's
/// `SPIN_WARN_THRESHOLD` cadence. The wait is bounded by the deadlock
/// argument above; a message here means an assumption behind that argument
/// broke, and naming it beats a silent wedge.
const STUCK_REPORT_SPINS: u32 = 1 << 27;

/// Install the handler. Called from `lapic::init` — i.e. on the BSP, on
/// **both** boot protocols, before any AP exists and before the vector can be
/// raised. The IDT is one shared table that every AP loads, so the APs pick
/// the handler up from there.
pub fn install() {
    #[allow(function_casts_as_integer)]
    let entry = shootdown_entry as usize;
    idt::set_handler(VECTOR, entry);
}

/// What the handler and the BKL assist both do: invalidate everything this
/// core could have cached and acknowledge the newest pending generation.
///
/// Reloading the current `CR3` invalidates every non-global entry — all user
/// entries; the kernel's upper half is fixed after boot. A core that loads a
/// different address space afterwards flushes on the `CR3` write anyway, so
/// acknowledging after the reload is sound.
pub fn service_pending() {
    let me = smp::cpu_index();
    let pending = PENDING[me].load(core::sync::atomic::Ordering::Acquire);
    if pending == 0 || pending <= ACKED[me].load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // SAFETY: reloading the CR3 already active changes no mapping — it only
    // forces every non-global TLB entry to be re-walked.
    unsafe { core::arch::asm!("mov rax, cr3", "mov cr3, rax", out("rax") _, options(nostack)) };
    ACKED[me].store(pending, core::sync::atomic::Ordering::Release);
    SERVICED[me].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// The vector's body. No lock, no allocation, no scheduler touch — see the
/// module header for why that is load-bearing.
#[unsafe(no_mangle)]
extern "C" fn shootdown_dispatch(_frame: *const idt::InterruptStackFrame) {
    service_pending();
    crate::lapic::eoi();
}

/// The `akuma_mmu` send hook: broadcast a shootdown to every online peer.
///
/// Returns whether any peer was contacted — `false` before the LAPIC exists,
/// on a one-core machine, and while the hooks are registered but the machine
/// has no secondaries yet (the boot paths arm the hooks early by design, and
/// those early flushes have no peers to reach).
///
/// Must be called with the BKL held; every flush call site is, and the
/// wait-side termination argument depends on it.
pub fn broadcast() -> bool {
    let peers = smp::online_cpus();
    if peers <= 1 || !crate::lapic::ready() {
        return false;
    }
    let me = smp::cpu_index();
    let generation = GEN.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1;
    // Mailbox first, IPI second: whichever way the peer learns of the
    // generation (delivered IPI, or the BKL assist reading the mailbox), the
    // generation is already visible.
    for (idx, pending) in PENDING.iter().enumerate().take(peers) {
        if idx != me {
            pending.store(generation, core::sync::atomic::Ordering::Release);
        }
    }
    for idx in 0..peers {
        if idx != me {
            crate::lapic::send_fixed(smp::lapic_id_of(idx), VECTOR);
        }
    }
    true
}

/// The `akuma_mmu` wait hook: spin until every online peer has acknowledged
/// the newest generation. Termination is the deadlock argument on
/// `set_shootdown_hooks` in `akuma-mmu`; a `[TLB] stuck` line here is that
/// argument failing, not slow hardware.
pub fn wait_for_acks() {
    let peers = smp::online_cpus();
    if peers <= 1 {
        return;
    }
    let me = smp::cpu_index();
    let generation = GEN.load(core::sync::atomic::Ordering::Acquire);
    let mut spins: u32 = 0;
    let mut remaining = peers;
    while remaining > 0 {
        remaining = 0;
        for (idx, acked) in ACKED.iter().enumerate().take(peers) {
            if idx != me
                && acked.load(core::sync::atomic::Ordering::Acquire) < generation
            {
                remaining += 1;
            }
        }
        if remaining == 0 {
            break;
        }
        // A peer that took a fatal exception prints its dump and `halt()`s with
        // interrupts off, so it will never acknowledge this generation and this
        // loop can never end. That is stage 3 of the ssh-wedge chain
        // (`docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`): the
        // sender spins here **holding the BKL**, every other core queues behind
        // it, and the box looks hung rather than crashed. The machine is
        // already dead at this point — one core is gone and its address space
        // half-flushed — so stop, quietly: the dump on the console is the
        // evidence, and a silent stop is what keeps it the last thing on screen.
        // Deliberately not a print: this core would be printing into that dump.
        if crate::idt::fatal_in_progress() {
            crate::halt();
        }
        spins += 1;
        if spins == STUCK_REPORT_SPINS {
            spins = 0;
            serial::puts("  [TLB] stuck: ");
            serial::put_dec(remaining as u64);
            serial::puts(" peer(s) unacked, generation=");
            serial::put_dec(generation);
            serial::puts("\n");
        }
        core::hint::spin_loop();
    }
}

/// The `akuma_bkl` spin assist, hooked from [`crate::boot::install_shared_sinks`].
///
/// A core spinning IRQ-masked for the BKL cannot take the IPI, and the sender
/// it waits for is waiting for *this* core — so the ticket wait services
/// pending shootdowns itself. One relaxed-free load per spin when nothing is
/// pending (`service_pending`'s early-out); the flush runs only when a
/// generation actually arrived.
pub fn bkl_spin_assist() {
    service_pending();
}

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

#[cfg(not(feature = "no-tests"))]
/// Prove the IPI actually arrives and is acknowledged, on every core.
///
/// Cheapest possible shape: the BSP (holding the BKL, like every real
/// sender) broadcasts one generation and every core's *serviced* counter
/// must move. The counters stay in the tree afterwards, so a boot log can
/// answer "did the shootdown reach all four cores" without a debugger.
pub fn smoke_test(t: &mut Suite) {
    if smp::online_cpus() <= 1 {
        t.note("shootdown: single core, IPI checks skipped", 0);
        return;
    }
    let before: [u64; MAX_CPUS] = core::array::from_fn(|i| SERVICED[i].load(core::sync::atomic::Ordering::Relaxed));
    if !broadcast() {
        t.check("shootdown: broadcast reached a peer", false);
        return;
    }
    wait_for_acks();
    let me = smp::cpu_index();
    let mut all = true;
    let mut mask = 0u64;
    // The sender is not in its own broadcast — it flushed locally inside its
    // `flush_tlb_*` call — so "every core acknowledged" means every *peer*.
    for (idx, serviced) in SERVICED.iter().enumerate().take(smp::online_cpus()) {
        if idx == me {
            continue;
        }
        let now = serviced.load(core::sync::atomic::Ordering::Relaxed);
        if now <= before[idx] {
            all = false;
        } else {
            mask |= 1 << idx;
        }
        t.note("shootdown: serviced on a cpu", now - before[idx]);
    }
    t.note("shootdown: cpus that serviced the IPI", mask);
    t.check("shootdown: every core acknowledged the IPI", all);
}

// The entry stub. Hand-assembled on the timer's pattern: `swapgs` on a ring-3
// origin (the handler reads the per-CPU block), the ten pushes keep `rsp`
// 16-aligned at the `call` and give a debugger a frame, and `iretq` resumes
// exactly what was interrupted — the handler edits nothing in the frame.
core::arch::global_asm!(
    r#"
    .section .text
.global shootdown_entry
shootdown_entry:
    test qword ptr [rsp + 8], 3
    jz 1f
    swapgs
1:
    push rbp
    push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    sub rsp, 8
    lea rdi, [rsp + 88]              /* &InterruptStackFrame */
    call shootdown_dispatch
    add rsp, 8
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    pop rbp
    test qword ptr [rsp + 8], 3
    jz 2f
    swapgs
2:
    iretq
"#
);

unsafe extern "C" {
    /// The vector-33 entry point, installed by [`install`].
    fn shootdown_entry();
}
