//! The machine a run shares between the host tasks that execute its virtual processors.
//!
//! Until 2026-10-23 every virtual processor of every guest took turns inside **one** host task
//! on **one** core, and the whole run was one 850-line function over a dozen locals. A guest
//! with more than one processor therefore got latency, not compute (`docs/archive/
//! KVM_CPU_ACCOUNTING.md`: two vCPUs halved the relay's bulk rate). This module is the same
//! loop split along the one seam the hardware already draws:
//!
//! * **Per processor** (a [`serve`] group's locals): the VMCB frame, the guest's register file,
//!   the host save page of the core it runs on, the `VMRUN`/exit sequence, its counters.
//! * **Shared** ([`Shared`]): guest RAM and the nested page table (never written once built, so
//!   there is no shootdown), the **devices** behind one lock ([`Devices`]: the PIT/PIC/UART/
//!   local APICs/MSR tables, the virtio-blk and virtio-net windows, the gateway, the relay),
//!   each processor's state word, and the stop request.
//!
//! A group is the set of processors one host task runs. With no placement (the default, the
//! boot-flag guests, the rehearsals) there is one group holding all of them and the behaviour
//! is exactly the old loop's: processors take turns, `PAUSE` is intercepted, a quantum bounds a
//! turn. With a placement (`vm_cores` against `kvmcores=`, `akuma_vm_spec::cores::place`) each
//! host core gets its own group, run by a task pinned to it; the group that holds processor 0
//! is the **owner** (the relay and the NIC's receive side are polled from there) and runs in the
//! task that created the machine.
//!
//! **What serialises the groups.** An exit is handled with the device lock held and, for a
//! service guest, **without the big kernel lock**: the whole of [`serve`] runs inside one
//! dropped-BKL window ([`Unlocked`]), so two groups' exits contend on the device lock alone and
//! the BKL is taken only where host state that still depends on it is touched - a disk image on
//! the host's filesystem or block devices (`HostDisk`), the lease check's foreign-pid lookup
//! (`ctl::should_stop`), and `yield_now`'s own switch. The relay's `socket_*` calls and the NIC
//! window need nothing: the socket table and `NETWORK` sit behind their own locks (the netpoll
//! daemon drains them BKL-free for the same reason). The lock order is **device lock, then BKL**
//! and never the reverse, so a disk read under the device lock may wait for the BKL but nobody
//! holding the BKL ever waits for the device lock. Rehearsal guests (`service` false) keep the
//! old shape: BKL held, released only around `VMRUN`. **Never hold the device lock across
//! `yield_now`, `VMRUN` or a sleep**: another group would take the lock while its owner is parked.
//!
//! **Cross-processor signals** are state words and a kick, in that order: the sender changes
//! the target's atomics (`state`, `sipi`, `init`, a local APIC's pending bits under the device
//! lock), sets its `poke` flag, then interrupts its core with the wake IPI
//! ([`crate::smp::kick_core`]). The target clears `poke` at the top of its loop and re-checks
//! it with interrupts masked just before it sleeps, so a kick that lands between its last look
//! and its `hlt` is not lost (an IPI taken at the wrong moment is consumed by the handler, not
//! latched). A target running its guest takes a `#VMEXIT` for the IPI and looks again.

use super::*;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use spinning_top::Spinlock;

/// Longest the owner sleeps with nothing to do and no relayed connection: bounds the latency of
/// a new connection (the relay's `accept` runs from this loop) and of a lease check.
const OWNER_IDLE_CAP_US: u64 = 2000;
/// The same while a relayed connection waits on this loop to move its bytes.
const OWNER_ACTIVE_CAP_US: u64 = 250;

impl CpuState {
    const fn code(self) -> u8 {
        match self {
            Self::Parked => 0,
            Self::Runnable => 1,
            Self::Halted => 2,
            Self::Dead => 3,
        }
    }

    const fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Runnable,
            2 => Self::Halted,
            3 => Self::Dead,
            _ => Self::Parked,
        }
    }
}

/// One virtual processor's shared control block.
struct VcpuCtl {
    state: AtomicU8,
    /// A STARTUP IPI waiting for the processor's own task: `0x100 | vector`, or 0.
    sipi: AtomicU32,
    /// An INIT arrived. The sender has already parked the processor and reset its local APIC.
    init: AtomicBool,
    /// "Look again": set by whoever changed something this processor must notice, before the kick.
    poke: AtomicBool,
    /// Where the guest was when its task last stopped (the summary line).
    rip: AtomicU64,
    /// The VMCB frame, until the processor's task takes it.
    vmcb: Spinlock<Option<FreshFrame>>,
}

impl VcpuCtl {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(CpuState::Parked.code()),
            sipi: AtomicU32::new(0),
            init: AtomicBool::new(false),
            poke: AtomicBool::new(false),
            rip: AtomicU64::new(0),
            vmcb: Spinlock::new(None),
        }
    }

    fn state(&self) -> CpuState {
        CpuState::from_code(self.state.load(Ordering::Acquire))
    }

    /// Move `from` to `to` unless something else (INIT) moved it first.
    fn cas(&self, from: CpuState, to: CpuState) -> bool {
        self.state.compare_exchange(from.code(), to.code(), Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    fn set(&self, to: CpuState) {
        self.state.store(to.code(), Ordering::Release);
    }
}

/// What the loop counted, per task and added up at the end.
#[derive(Clone, Copy, Default)]
struct Counters {
    exits: u64,
    cpuid: u64,
    msr: u64,
    io: u64,
    irq: u64,
    exc: u64,
    inj: u64,
    hlt: u64,
    mmio: u64,
    lapic: u64,
    ticks: u64,
    ipi: u64,
    init: u64,
    sipi: u64,
    pause: u64,
    net: u64,
}

impl Counters {
    fn add(&mut self, o: &Self) {
        self.exits += o.exits;
        self.cpuid += o.cpuid;
        self.msr += o.msr;
        self.io += o.io;
        self.irq += o.irq;
        self.exc += o.exc;
        self.inj += o.inj;
        self.hlt += o.hlt;
        self.mmio += o.mmio;
        self.lapic += o.lapic;
        self.ticks += o.ticks;
        self.ipi += o.ipi;
        self.init += o.init;
        self.sipi += o.sipi;
        self.pause += o.pause;
        self.net += o.net;
    }

    /// The per-kind exit counts [`Acct::report`] prints rates for: hlt, lapic page, port I/O,
    /// MSR, host irq, NIC, disk.
    fn kinds(&self) -> [u64; 7] {
        [self.hlt, self.lapic, self.io, self.msr, self.irq, self.net, self.mmio]
    }
}

/// Why the run ended: the first request wins.
struct Why {
    set: bool,
    fatal: bool,
    reason: &'static str,
}

/// Everything behind the device lock.
struct Devices {
    plat: Platform,
    blk: Option<(MmioBlk, HostDisk)>,
    net: Option<(MmioNet, Gateway<'static>)>,
    relay: Option<Relay>,
    /// The bounce page the virtio devices copy through.
    scratch: FreshFrame,
    log: ExitLog,
}

struct Shared {
    guest: Guest,
    nrips: bool,
    service: bool,
    n_cpu: usize,
    ram: Ram,
    /// Keeps the nested page table's frames alive for as long as any processor can run.
    _npt: Npt,
    dev: Spinlock<Devices>,
    cpu: [VcpuCtl; MAX_VCPUS],
    /// The host core each processor runs on.
    place: [u8; MAX_VCPUS],
    stop: AtomicBool,
    why: Spinlock<Why>,
    started_us: u64,
    totals: Spinlock<Counters>,
    /// `hvm_start_info`'s address: the boot processor's `EBX`.
    start_gpa: u64,
}

impl Shared {
    /// End the run (the first reason is the one reported) and make every task notice.
    fn request_stop(&self, reason: &'static str, fatal: bool) {
        {
            let mut w = self.why.lock();
            if !w.set {
                *w = Why { set: true, fatal, reason };
            } else {
                w.fatal |= fatal;
            }
        }
        self.stop.store(true, Ordering::Release);
        for j in 0..self.n_cpu {
            self.poke(j);
        }
    }

    /// Tell processor `j`'s task to look again, whichever core it is on.
    fn poke(&self, j: usize) {
        self.cpu[j].poke.store(true, Ordering::Release);
        crate::smp::kick_core(usize::from(self.place[j]));
    }

    /// Can no processor ever run again? (Called by a task whose own group has nothing runnable.)
    /// A parked processor does not count: only another processor can start it.
    fn nothing_can_run(&self, d: &Devices) -> bool {
        (0..self.n_cpu).all(|j| {
            let c = &self.cpu[j];
            match c.state() {
                CpuState::Runnable => false,
                _ if c.sipi.load(Ordering::Acquire) != 0 => false,
                // A service guest that halts with no timer is not dead: the relay may feed it.
                CpuState::Halted => !(self.service || d.plat.can_wake(j)),
                CpuState::Parked | CpuState::Dead => true,
            }
        })
    }
}

/// A dropped-BKL window for as long as the value lives: a service guest's tasks handle their
/// exits with the big kernel lock released (see the module comment). Nests with the window
/// around `VMRUN`, whose close then leaves the lock released.
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

/// The device lock, taken without ever waiting **while holding the big kernel lock**. A rehearsal
/// guest handles its exits under the BKL, so a task spinning here on a lock whose holder needs the
/// BKL back (after a `yield_now` inside the relay or a network call) would hold the BKL against
/// the one task that can release this lock: a two-party deadlock. Dropping the BKL between
/// attempts lets the holder finish. (A no-op for a service guest, which does not hold it.)
fn lock_dev(sh: &Shared) -> spinning_top::guard::SpinlockGuard<'_, Devices> {
    loop {
        if let Some(g) = sh.dev.try_lock() {
            return g;
        }
        crate::smp::bkl_drop_window();
        core::hint::spin_loop();
    }
}

/// Memory for the gateway's queue and rings, which it borrows for its whole life. A machine's
/// tasks share it, so the borrow has to be `'static`; the storage is allocated once and the
/// gateway is handed from guest to guest (`Gateway::reset`) rather than leaked afresh per start.
static GATEWAY: Spinlock<Option<Gateway<'static>>> = Spinlock::new(None);

fn take_gateway() -> Option<Gateway<'static>> {
    let pooled = GATEWAY.lock().take();
    match pooled {
        Some(mut g) => {
            g.reset();
            Some(g)
        }
        None => {
            let mut v = alloc::vec::Vec::new();
            v.try_reserve_exact(svm::gateway::STORAGE).ok()?;
            v.resize(svm::gateway::STORAGE, 0u8);
            Gateway::new(v.leak())
        }
    }
}

/// A worker's job: serve `group` of `sh`.
struct Job {
    sh: Arc<Shared>,
    group: u32,
}

static JOBS: [Spinlock<Option<Job>>; crate::smp::MAX_CPUS] = [const { Spinlock::new(None) }; crate::smp::MAX_CPUS];
static WORKERS: [AtomicBool; crate::smp::MAX_CPUS] = [const { AtomicBool::new(false) }; crate::smp::MAX_CPUS];

/// A worker lives for the kernel's life on its core and serves whatever job is posted for it;
/// it sleeps (a tick is the latency of a new machine) while there is none.
extern "C" fn worker_task() -> ! {
    let core = crate::smp::cpu_index();
    loop {
        let job = JOBS[core].lock().take();
        match job {
            Some(Job { sh, group }) => {
                serve(&sh, group, false);
                // Dropping the Arc is the signal the coordinator waits for.
                drop(sh);
            }
            None => {
                crate::sched::yield_now();
                crate::sched::allow_tick();
            }
        }
    }
}

/// Make sure `core` has a worker. False if there is no task slot for it.
fn ensure_worker(core: usize) -> bool {
    if WORKERS[core].load(Ordering::Acquire) {
        return true;
    }
    let Some(slot) = crate::sched::spawn_daemon_on(worker_task, core) else {
        return false;
    };
    WORKERS[core].store(true, Ordering::Release);
    p("[KVM] linux: vCPU worker task ");
    dec(slot as u64);
    p(" on core ");
    dec(core as u64);
    p("\n");
    true
}

/// The next runnable processor in `group` after `after`, round robin.
fn next_runnable(sh: &Shared, group: u32, after: usize) -> Option<usize> {
    (1..=MAX_VCPUS).map(|d| (after + d) % MAX_VCPUS).find(|&j| group & (1 << j) != 0 && sh.cpu[j].state() == CpuState::Runnable)
}

/// Run the processors in `group` on this core until the machine stops.
#[allow(clippy::too_many_lines)]
fn serve(sh: &Shared, group: u32, owner: bool) {
    let guest = sh.guest;
    let nrips = sh.nrips;
    let service = sh.service;
    let me = crate::smp::cpu_index();
    let members = || (0..MAX_VCPUS).filter(move |&j| group & (1 << j) != 0 && j < sh.n_cpu);
    let first_member = members().next().unwrap_or(0);

    let Some(mut host_f) = FreshFrame::alloc_zeroed() else {
        sh.request_stop("no frame for the host save area", true);
        return;
    };
    if !enable_on_core(me) {
        sh.request_stop("no host save area for a processor's core", true);
        return;
    }
    // The host's FS/GS/TR/LDTR/syscall-MSR state, parked in a VMCB-format page that
    // `vmrun_regs` loads back after every exit.
    let host_pa = host_f.pa() as u64;
    let _ = host_f.bytes_mut();
    save_host(host_pa);

    let mut vmcbs = [const { None::<FreshFrame> }; MAX_VCPUS];
    for j in members() {
        vmcbs[j] = sh.cpu[j].vmcb.lock().take();
    }
    let mut regs_all = [GuestRegs::default(); MAX_VCPUS];
    regs_all[0].rbx = sh.start_gpa;

    let mut cur = members().last().unwrap_or(first_member);
    let shares_core = members().count() > 1;
    let mut acct = Acct::new();
    let mut c = Counters::default();
    let mut next_relay_us = 0u64;
    let mut next_status_us = 0u64;
    let started_us = sh.started_us;
    let mut next_acct_us = crate::net::uptime_us() + ACCT_PERIOD_US;

    // IRQs on for the whole run: with V_INTR_MASKING the host IF at VMRUN gates physical
    // interrupts, and a tick must reach the host.
    akuma_cpu::daif::unmask_irq();
    // A service guest's exits are handled BKL-free; closed when `serve` returns.
    let _unlocked = service.then(Unlocked::new);
    'run: loop {
        if sh.stop.load(Ordering::Acquire) {
            break 'run;
        }
        for j in members() {
            sh.cpu[j].poke.store(false, Ordering::Release);
        }
        let now_us = crate::net::uptime_us();
        if !service {
            // The rehearsal guests are bounded; a service guest is not.
            if c.exits >= MAX_EXITS {
                sh.request_stop("exit budget spent", false);
                break 'run;
            }
            if now_us.saturating_sub(started_us) > RUN_BUDGET_US {
                sh.request_stop("wall-clock budget spent", false);
                break 'run;
            }
            let (banner, last_tx, done) = {
                let d = lock_dev(sh);
                (d.plat.banner.seen, d.plat.last_tx_us, d.plat.done.seen)
            };
            if banner && now_us.saturating_sub(last_tx) > QUIET_BUDGET_US {
                sh.request_stop("guest console quiet after the banner", false);
                break 'run;
            }
            if done {
                sh.request_stop("guest reported its init ran to the end", false);
                break 'run;
            }
        }
        if service && super::super::ctl::should_stop() {
            sh.request_stop("stopped (stop written to /dev/kvm0, or its owner is gone)", false);
            break 'run;
        }
        if service {
            // Other host tasks (the network, sshd) get the core between guest entries. The
            // kernel runs with interrupts masked outside its idle loop, and the switch
            // expects that.
            akuma_cpu::daif::mask_irq();
            let t0 = akuma_cpu::tsc::rdtsc();
            crate::sched::yield_now();
            acct.yielded += akuma_cpu::tsc::rdtsc().wrapping_sub(t0);
            akuma_cpu::daif::unmask_irq();
            if now_us >= next_acct_us {
                next_acct_us = now_us + ACCT_PERIOD_US;
                acct.report(c.exits, c.kinds(), me);
            }
        }

        // Requests other processors left for ours: INIT parks, STARTUP starts.
        for j in members() {
            let ctl = &sh.cpu[j];
            if ctl.init.swap(false, Ordering::AcqRel) {
                // Already parked by the sender; whatever this processor had in flight is void.
                regs_all[j] = GuestRegs::default();
            }
            let sipi = ctl.sipi.swap(0, Ordering::AcqRel);
            if sipi != 0 && ctl.state() == CpuState::Parked {
                let vector = (sipi & 0xFF) as u8;
                set_startup_state(&mut vmcb_of(&mut vmcbs, j), vector);
                regs_all[j] = GuestRegs::default();
                ctl.cas(CpuState::Parked, CpuState::Runnable);
            }
        }

        // The owner moves the relay's bytes and gives the guest the frames waiting for it.
        if owner {
            let relay_t0 = akuma_cpu::tsc::rdtsc();
            let mut dg = lock_dev(sh);
            let Devices { relay, net, scratch, plat, .. } = &mut *dg;
            if let (Some(r), Some((dev, wire))) = (relay.as_mut(), net.as_mut())
                && now_us >= next_relay_us
            {
                next_relay_us = now_us + 200;
                r.poll(wire, now_us);
                if now_us >= next_status_us {
                    next_status_us = now_us + 5_000_000;
                    r.status(wire);
                    if r.active() {
                        // Where the guest is while a relayed connection waits on it: if cpu0 sits
                        // halted with no timer due and frames queued, nothing is going to wake it.
                        p("[KVM] guest: cpu0=");
                        p(match sh.cpu[0].state() {
                            CpuState::Runnable => "run",
                            CpuState::Halted => "halted",
                            _ => "other",
                        });
                        p(" exits=");
                        dec(c.exits);
                        p(" next_event_in_us=");
                        match plat.next_event_us(0) {
                            Some(d) => dec(d.saturating_sub(now_us)),
                            None => p("none"),
                        }
                        p(" gw_queued=");
                        dec(wire.queued() as u64);
                        p(" gw_frames_from_guest=");
                        dec(wire.stats.frames);
                        p(" gw_lost=");
                        dec(wire.stats.lost);
                        p(" gw_ignored=");
                        dec(wire.stats.ignored);
                        p(" gw_resets=");
                        dec(wire.stats.tcp_resets);
                        p(" syn_out=");
                        dec(wire.stats.fwd_syn_out);
                        p(" syn_to_guest=");
                        dec(wire.stats.rx_syn_delivered);
                        p(" synack_in=");
                        dec(wire.stats.fwd_synack_in);
                        p(" rst_in=");
                        dec(wire.stats.fwd_rst_in);
                        p(" stale_in=");
                        dec(wire.stats.fwd_stale_in);
                        p(" rx_to_guest=");
                        dec(wire.stats.rx_delivered);
                        p(" rx_starved=");
                        dec(dev.rx_starved);
                        p("\n");
                    }
                }
            }
            // Frames the gateway has for the guest go into the buffers it has posted.
            if let Some((dev, wire)) = net.as_mut() {
                dev.deliver_rx(&sh.ram, wire, scratch.bytes_mut());
            }
            drop(dg);
            acct.relay += akuma_cpu::tsc::rdtsc().wrapping_sub(relay_t0);
        }

        // A halted processor wakes when something can be offered to it.
        {
            let mut dg = lock_dev(sh);
            for j in members() {
                if sh.cpu[j].state() == CpuState::Halted && dg.plat.offer(j, now_us).is_some() {
                    sh.cpu[j].cas(CpuState::Halted, CpuState::Runnable);
                }
            }
        }

        // The next runnable processor after the one that just ran.
        let Some(i) = next_runnable(sh, group, cur) else {
            // Nobody in this group can run.
            let ended = {
                let dg = lock_dev(sh);
                sh.nothing_can_run(&dg)
            };
            if ended {
                let dead = (0..sh.n_cpu).any(|j| sh.cpu[j].state() == CpuState::Dead);
                sh.request_stop(
                    if dead { "guest halted with interrupts off" } else { "guest halted waiting for an interrupt nothing can raise" },
                    false,
                );
                break 'run;
            }
            if service {
                // Sleep until the earliest of: a timer of ours, a kick, the host's next interrupt
                // (the NIC's, a keystroke). Interrupts are masked while the one-shot is armed,
                // and `poke` is re-read after masking: a kick that landed since the top of the
                // loop was consumed by its handler, not latched, and would otherwise be slept
                // through.
                akuma_cpu::daif::mask_irq();
                if members().any(|j| sh.cpu[j].poke.load(Ordering::Acquire)) || sh.stop.load(Ordering::Acquire) {
                    akuma_cpu::daif::unmask_irq();
                    continue 'run;
                }
                let sleep_t0 = akuma_cpu::tsc::rdtsc();
                let due = {
                    let dg = lock_dev(sh);
                    members().filter(|&j| sh.cpu[j].state() == CpuState::Halted).filter_map(|j| dg.plat.next_event_us(j)).min()
                };
                let mut wait = due.map(|d| d.saturating_sub(crate::net::uptime_us()));
                if owner {
                    // The relay's `accept` and the lease check run from this loop.
                    let cap = if lock_dev(sh).relay.as_ref().is_some_and(Relay::active) { OWNER_ACTIVE_CAP_US } else { OWNER_IDLE_CAP_US };
                    wait = Some(wait.map_or(cap, |w| w.min(cap)));
                }
                if let Some(w) = wait {
                    let _ = crate::lapic::arm_oneshot_in_us(w);
                }
                crate::sched::allow_tick();
                acct.sleep += akuma_cpu::tsc::rdtsc().wrapping_sub(sleep_t0);
                akuma_cpu::daif::unmask_irq();
            } else {
                core::hint::spin_loop();
            }
            continue 'run;
        };
        c.exits += 1;
        cur = i;

        // Bring the interrupt sources up to now (the PIT's channel 0 edge, the local APIC's
        // timer), then offer the best vector to the guest as a virtual INTR; the processor takes
        // it when the guest's IF allows (§15.21.4) and writes V_IRQ back cleared when it has.
        let runnable = members().filter(|&j| sh.cpu[j].state() == CpuState::Runnable).count();
        let (offered, mut due) = {
            let mut dg = lock_dev(sh);
            let offered = dg.plat.offer(i, now_us);
            let due = members()
                .filter(|&j| matches!(sh.cpu[j].state(), CpuState::Runnable | CpuState::Halted))
                .filter_map(|j| dg.plat.next_event_us(j))
                .min();
            (offered, due)
        };
        vmcb_of(&mut vmcbs, i).set_v_intr(match offered {
            Some(o) => ctrl::v_intr_extint(o.vector),
            None => ctrl::V_INTR_MASKING,
        });
        // While another processor *of this core* can run, a `PAUSE` (the guest's spin-wait hint)
        // gives it the core at once: a waiter would otherwise burn its whole slice on a lock
        // whose holder is the processor that is not running, and spin-count logic in the guest
        // (timeouts, backoff) would see stalls no real machine produces. On a core of its own a
        // processor spins undisturbed. A host interrupt at the guest's next deadline, or when
        // this processor's turn is up and another can run, ends the entry.
        vmcb_of(&mut vmcbs, i).set_intr0(if shares_core && runnable > 1 { INTR0_BASE | intr0::PAUSE } else { INTR0_BASE });
        if shares_core && runnable > 1 {
            let slice = now_us + QUANTUM_US;
            due = Some(due.map_or(slice, |d| d.min(slice)));
        }
        if let Some(due) = due {
            akuma_cpu::daif::mask_irq();
            let _ = crate::lapic::arm_oneshot_in_us(due.saturating_sub(crate::net::uptime_us()));
            akuma_cpu::daif::unmask_irq();
        }
        crumb(0x57);
        if service {
            save_host(host_pa);
        }
        // A service guest runs with the BKL released: on a many-core host the network and wifi
        // tasks (and every syscall) wait on it, and a guest entry can last a whole quantum. The
        // window is what the netpoll daemon uses for its own long BKL-free stretches; the host
        // interrupt that ends an entry leaves the lock released too. (Nested in `_unlocked`
        // above: this close does not take the lock back.)
        if service {
            akuma_bkl::bkl::dropped_window_open();
        }
        let guest_t0 = akuma_cpu::tsc::rdtsc();
        // SAFETY: the VMCB and the host save image are live frames this function owns; SVM is
        // enabled on this core (`enable_on_core`).
        unsafe { vmrun_regs(vmcb_of_pa(&vmcbs, i), host_pa, &mut regs_all[i]) };
        acct.guest += akuma_cpu::tsc::rdtsc().wrapping_sub(guest_t0);
        if service {
            akuma_bkl::bkl::dropped_window_close();
        }
        crumb(0x58);
        let regs = &mut regs_all[i];
        let mut v = vmcb_of(&mut vmcbs, i);
        // The first entry flushed; the rest need not.
        v.set_tlb_control(ctrl::TLB_DO_NOTHING);

        // Everything below touches the devices: one lock for the whole exit, dropped before the
        // next yield, `VMRUN` or sleep.
        let mut dg = lock_dev(sh);
        let Devices { plat, blk, net, scratch, log, .. } = &mut *dg;
        plat.cur = i;
        // V_IRQ comes back cleared once the guest has dispatched the offered vector: that is the
        // INTA cycle, and it happens before any exit this run can report, so the PIC is up to
        // date before a port write (the guest's EOI) is looked at.
        if let Some(o) = offered
            && v.v_intr() & ctrl::V_IRQ == 0
        {
            c.inj += 1;
            if !plat.accept(i, o) {
                p("\n[KVM] linux: PIC acknowledged a different vector than it offered\n");
            }
            if o.source == Source::Lapic {
                c.ticks += 1;
            }
            if c.inj <= LOG_FIRST.into() {
                p("\n[KVM] linux: irq vector ");
                hx(u64::from(o.vector), 2);
                p(" taken at rip=");
                hx(v.rip(), 16);
                p("\n");
            }
        }
        match ExitCode::from_u64(v.exitcode()) {
            ExitCode::Cpuid => {
                c.cpuid += 1;
                let (leaf, sub) = (v.rax() as u32, regs.rcx as u32);
                let h = __cpuid_count(leaf, sub);
                let [a, mut b, cc, mut d] = svm::cpuid::filter(leaf, [h.eax, h.ebx, h.ecx, h.edx]);
                // A guest with a local APIC sees it in CPUID and, being the only processor, has
                // initial APIC id 0 (the host core's id is no business of its).
                if leaf == 1 && guest.exposes_apic() {
                    d |= h.edx & (1 << 9);
                    // Initial APIC id: the processor's index, not the host core's.
                    b = (b & 0x00FF_FFFF) | ((i as u32) << 24);
                }
                // The hypervisor leaves (0x4000_0000..) are scanned 256 at a time; one line says
                // so, the rest would drown the log.
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
                    hx(u64::from(cc), 8);
                    p(" ");
                    hx(u64::from(d), 8);
                    p("\n");
                }
                v.set_rax(u64::from(a));
                regs.rbx = u64::from(b);
                regs.rcx = u64::from(cc);
                regs.rdx = u64::from(d);
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Msr => {
                c.msr += 1;
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
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Ioio => {
                c.io += 1;
                let io = exitinfo::Io::decode(v.exitinfo1());
                if io.string || io.size == 0 {
                    sh.request_stop("string port I/O (INS/OUTS) is not emulated", true);
                    p("\n[KVM] linux: string port I/O (INS/OUTS) is not emulated port=");
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
                v.complete_instruction(v.exitinfo2());
                if plat.reset_requested {
                    sh.request_stop("guest requested a reset (keyboard-controller pulse: its panic=-1 reboot)", false);
                    break 'run;
                }
            }
            ExitCode::Hlt => {
                c.hlt += 1;
                if v.rflags() & (1 << 9) == 0 {
                    // Nothing can wake it. The run goes on without it while another processor
                    // can, and stops when none can.
                    sh.cpu[i].cas(CpuState::Runnable, CpuState::Dead);
                } else {
                    let next = next_rip(&v, nrips, 1);
                    v.complete_instruction(next);
                    // Idle until an interrupt can reach it; the others run meanwhile. (A CAS: an
                    // INIT that parked it during this exit stays parked.)
                    sh.cpu[i].cas(CpuState::Runnable, CpuState::Halted);
                }
            }
            // A host interrupt or NMI arrived while the guest ran. The host's IF is set, so it
            // was taken at the `stgi` that ended the exit (the wake IPI of a kick among them);
            // there is nothing to do but resume (and count it).
            ExitCode::Other(0x60 | 0x61) => c.irq += 1,
            // Step over the `PAUSE` (`F3 90`); the next turn goes to the other processor.
            ExitCode::Pause => {
                c.pause += 1;
                let next = next_rip(&v, nrips, 2);
                v.complete_instruction(next);
            }
            ExitCode::Ud | ExitCode::Gp | ExitCode::Pf | ExitCode::Other(0x48) => {
                c.exc += 1;
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
                    dump_code(&sh.ram, &v, v.rip());
                }
                if during_delivery {
                    sh.request_stop("exception while delivering an event", true);
                    dump_state(&v);
                    break 'run;
                }
                // The intercept is tested before CR2 is written: the guest's CR2 is the VMM's to
                // set (§15.12.15).
                if vector == 14 {
                    let addr = v.exitinfo2();
                    v.set_save64(save::CR2, addr);
                }
                v.set_eventinj(eventinj::exception(vector, err));
            }
            ExitCode::Npf if plat.lapic_page(i).is_some_and(|b| (b..b + lapic::PAGE_LEN).contains(&v.exitinfo2())) => {
                c.lapic += 1;
                let off = v.exitinfo2() - (plat.apic_bases[i] & APIC_BASE_ADDR);
                let access = guest_code(&sh.ram, &v, v.rip()).and_then(|code| mmio::decode(&code));
                let Some(a) = access else {
                    sh.request_stop("guest touched the local APIC page with an instruction the MMIO decoder does not know", true);
                    p("\n[KVM] linux: guest touched the local APIC page with an instruction the MMIO decoder does not know rip=");
                    hx(v.rip(), 16);
                    p("\n");
                    dump_code(&sh.ram, &v, v.rip());
                    break 'run;
                };
                let now = lapic::bus_ticks_from_us(crate::net::uptime_us());
                mmio_emulate(&a, &mut LapicDev { lapic: &mut plat.lapics[i], off, now }, &mut v, regs);
            }
            ExitCode::Npf if net.is_some() && (NET_BASE..NET_BASE + NET_LEN).contains(&v.exitinfo2()) => {
                c.net += 1;
                let off = v.exitinfo2() - NET_BASE;
                let access = guest_code(&sh.ram, &v, v.rip()).and_then(|code| mmio::decode(&code));
                let Some(a) = access else {
                    sh.request_stop("guest touched the NIC window with an instruction the MMIO decoder does not know", true);
                    p("\n[KVM] linux: guest touched the NIC window with an instruction the MMIO decoder does not know rip=");
                    hx(v.rip(), 16);
                    p("\n");
                    dump_code(&sh.ram, &v, v.rip());
                    break 'run;
                };
                let (dev, wire) = net.as_mut().expect("checked by the guard");
                let mut window = NetDev { dev, wire, ram: &sh.ram, bounce: scratch.bytes_mut(), off };
                mmio_emulate(&a, &mut window, &mut v, regs);
            }
            ExitCode::Npf if blk.is_some() && (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_LEN).contains(&v.exitinfo2()) => {
                c.mmio += 1;
                let off = v.exitinfo2() - VIRTIO_BASE;
                let access = guest_code(&sh.ram, &v, v.rip()).and_then(|code| mmio::decode(&code));
                let Some(a) = access else {
                    sh.request_stop("guest touched the virtio window with an instruction the MMIO decoder does not know", true);
                    p("\n[KVM] linux: guest touched the virtio window with an instruction the MMIO decoder does not know rip=");
                    hx(v.rip(), 16);
                    p("\n");
                    dump_code(&sh.ram, &v, v.rip());
                    break 'run;
                };
                let (dev, hostdisk) = blk.as_mut().expect("checked by the guard");
                let mut window = VirtioDev { dev, disk: hostdisk, ram: &sh.ram, bounce: scratch.bytes_mut(), pic: &mut plat.pic, off };
                mmio_emulate(&a, &mut window, &mut v, regs);
            }
            ExitCode::Npf => {
                sh.request_stop("nested page fault (guest touched unmapped guest-physical memory)", true);
                p("\n[KVM] linux: nested page fault (guest touched unmapped guest-physical memory): gpa=");
                hx(v.exitinfo2(), 16);
                p(" info1=");
                hx(v.exitinfo1(), 16);
                p(" rip=");
                hx(v.rip(), 16);
                p("\n");
                dump_code(&sh.ram, &v, v.rip());
                break 'run;
            }
            ExitCode::Shutdown => {
                sh.request_stop("triple fault", true);
                p("\n[KVM] linux: triple fault: ");
                dump_exit(&v, regs.rcx, regs.rdx);
                dump_state(&v);
                dump_code(&sh.ram, &v, v.rip());
                break 'run;
            }
            _ => {
                sh.request_stop("unexpected exit", true);
                p("\n[KVM] linux: unexpected exit: ");
                dump_exit(&v, regs.rcx, regs.rdx);
                dump_state(&v);
                dump_code(&sh.ram, &v, v.rip());
                break 'run;
            }
        }
        // An ICR write that named other processors: route it now that this exit's borrow of the
        // VMCB is over. A STARTUP is only *posted* — the target's own task rewrites its VMCB.
        let mut kick = 0u32;
        if let Some(ipi) = plat.lapics[i].take_ipi() {
            c.ipi += 1;
            for j in 0..sh.n_cpu {
                if j == i || !ipi.reaches(plat.lapics[j].id()) {
                    continue;
                }
                let target = &sh.cpu[j];
                match ipi.mode {
                    // A processor held in INIT takes no interrupts.
                    IpiMode::Fixed if target.state() != CpuState::Parked => {
                        plat.lapics[j].raise(ipi.vector);
                        kick |= 1 << j;
                    }
                    IpiMode::Fixed => {}
                    IpiMode::Init => {
                        c.init += 1;
                        target.set(CpuState::Parked);
                        target.sipi.store(0, Ordering::Release);
                        target.init.store(true, Ordering::Release);
                        plat.lapics[j].init_reset();
                        kick |= 1 << j;
                    }
                    IpiMode::Startup if target.state() == CpuState::Parked => {
                        c.sipi += 1;
                        p("\n[KVM] linux: cpu ");
                        dec(j as u64);
                        p(" started at ");
                        hx(u64::from(ipi.vector) << 12, 8);
                        p("\n");
                        target.sipi.store(0x100 | u32::from(ipi.vector), Ordering::Release);
                        kick |= 1 << j;
                    }
                    IpiMode::Startup => {}
                }
            }
        }
        // The PIC and the PIT are processor 0's: a device that another processor's exit just
        // raised a line on (the disk's completion) has to reach 0 now, not at its next turn.
        if i != 0 && plat.pic.pending().is_some() {
            kick |= 1;
        }
        drop(dg);
        for j in 0..sh.n_cpu {
            if kick & (1 << j) != 0 {
                sh.poke(j);
            }
        }
    }
    crumb(0x59);
    for j in members() {
        if let Some(f) = vmcbs[j].as_mut() {
            sh.cpu[j].rip.store(Vmcb::new(f.bytes_mut()).rip(), Ordering::Relaxed);
        }
    }
    sh.totals.lock().add(&c);
}

/// A VMCB frame's physical address.
fn vmcb_of_pa(frames: &[Option<FreshFrame>; MAX_VCPUS], cpu: usize) -> u64 {
    frames[cpu].as_ref().expect("a VMCB is allocated for every virtual processor").pa() as u64
}

/// The `late()`/service entry: set a guest up, run it on its placed cores and report.
#[allow(clippy::too_many_lines)]
pub fn run(nrips: bool, vm: &Vm) {
    let guest = vm.guest;
    crumb(0x51);
    p("[KVM] linux: loading ");
    p(guest.name());
    p(" guest ");
    p(vm.path());
    p("\n");
    let ram_bytes = vm.ram_bytes();
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
    let loaded = match load_kernel(vm.path(), &ram, &mut scratch) {
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

    // The handoff: cmdline, memmap and start_info under the top of RAM, the memmap's one RAM
    // entry stopping short of them. With a disk, one more page under the handoff pages holds
    // the ACPI tables and the memory map reports it (type 3) instead of as RAM.
    let disk = vm.pick_disk();
    let _disk_lease = DiskLease(disk);
    // A spec that names a disk does not run without it: a refused RAM copy (the reserve is too
    // small) or a missing image would otherwise boot a rootless guest that halts and is
    // restarted forever (found on the ryzen 2026-10-22, a 3 GiB image against a 3 GiB reserve).
    if disk.is_none() && vm.wants_disk() {
        p("[KVM] linux: refused: the spec's disk is not available, not starting without it\n");
        return;
    }
    // Only Linux is told about its disk through ACPI; the Akuma guest finds it on the command
    // line. The SMP guest needs the MADT whatever its disk is.
    let acpi = (disk.is_some() && guest.is_linux()) || vm.vcpus() > 1;
    // Linux is handed the tables by `rsdp_paddr` and a memory-map entry under the top of RAM. The
    // Akuma kernel ignores that field and scans the BIOS window for the RSDP
    // (`akuma-ryzen-amd64`'s `find_rsdp`), so its page sits at the window's start, as
    // Firecracker's does, inside the RAM entry and with no entry of its own.
    let acpi_by_scan = guest.is_akuma();
    let acpi_gpa = if acpi_by_scan { BIOS_RSDP_GPA } else { ram_bytes - 0x4000 };
    let l = pvh::plan(ram_bytes, 0, if acpi && !acpi_by_scan { acpi_gpa } else { ram_bytes - 0x3000 });
    scratch.bytes_mut().fill(0);
    // The guest is told the host's TSC rate rather than left to calibrate it through trapped
    // port reads: a poll loop whose every iteration is a VM exit measures the exits, not the
    // PIT (`tsc_early_khz` is a documented kernel parameter). Built on the stack: no allocation.
    let mut cmd = [0u8; 400];
    let cmdline = match guest {
        Guest::Linux | Guest::LinuxApic => cmdline_with_tsc(
            &mut cmd,
            if guest == Guest::LinuxApic { LINUX_APIC_CMDLINE } else { LINUX_CMDLINE },
            crate::lapic::tsc_hz() / 1000,
            if disk.is_some() { ROOT_ARGS } else { NO_DISK_ARGS },
        ),
        // The Akuma guest calibrates its own TSC against the virtual PIT.
        Guest::Akuma => cmdline_with_tsc(&mut cmd, AKUMA_CMDLINE, 0, if disk.is_some() { AKUMA_DISK_ARGS } else { "" }),
        Guest::AkumaHello => cmdline_with_tsc(&mut cmd, AKUMA_CMDLINE, 0, if disk.is_some() { HELLO_DISK_ARGS } else { "" }),
        // No `nosmp`: the MADT says how many processors there are.
        Guest::AkumaSmp => cmdline_with_tsc(&mut cmd, "", 0, if disk.is_some() { AKUMA_DISK_ARGS } else { "" }),
        // The disk's window first, then the NIC's: slot order is device order.
        Guest::AkumaNet => cmdline_with_tsc(&mut cmd, AKUMA_CMDLINE, 0, if disk.is_some() { AKUMA_NET_ARGS } else { NET_ARGS }),
        // `nosmp` only for one processor: with more, the MADT says how many there are.
        Guest::AkumaSpec => match spec_cmdline(&mut cmd, vm, disk.is_some()) {
            Some(c) => c,
            None => {
                p("[KVM] linux: the spec's command line does not fit\n");
                return;
            }
        },
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
            p(vm.disk_hint());
            p(")\n");
        }
    }
    if !pvh::write_cmdline(scratch.bytes_mut(), cmdline) || !ram.write(l.cmdline_gpa, scratch.bytes_mut()) {
        p("[KVM] linux: cmdline handoff failed\n");
        return;
    }
    // The memory map and start_info are built together: with a disk the ACPI entry and
    // `rsdp_paddr` go into them, and the tables into their own page.
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
        let Some(rsdp) = svm::acpi::build_smp(acpi_page.bytes_mut(), acpi_gpa, &dev, vm.vcpus()) else {
            p("[KVM] linux: ACPI tables do not fit\n");
            return;
        };
        if !acpi_by_scan {
            pvh::add_acpi(start_page.bytes_mut(), scratch.bytes_mut(), rsdp, acpi_gpa, 0x1000);
        }
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

    // One VMCB per virtual processor, each with its own ASID (the TLB is tagged by it, and two
    // processors may share a core).
    let n_cpu = vm.vcpus();
    let service = guest.is_service();
    let here = crate::smp::cpu_index();
    // Where each processor runs: a service guest by its spec against the machine's `kvmcores=`,
    // everything else on the core it was started from, as it always was.
    let place = if service {
        let wanted = vm.spec.as_ref().map_or(0, |s| s.cores);
        akuma_vm_spec::cores::place(wanted, super::super::vm_cores(), |c| super::super::core_ready(c as usize), n_cpu)
    } else {
        Some([here as u8; akuma_vm_spec::cores::MAX_PLACED])
    };
    let Some(place) = place else {
        p("[KVM] linux: no core in kvmcores= has SVM enabled\n");
        return;
    };
    let ctl: [VcpuCtl; MAX_VCPUS] = [const { VcpuCtl::new() }; MAX_VCPUS];
    for (i, c) in ctl.iter().enumerate().take(n_cpu) {
        let Some(mut f) = FreshFrame::alloc_zeroed() else {
            p("[KVM] linux: no frame for a VMCB\n");
            return;
        };
        init_vmcb(f.bytes_mut(), guest, npt.pml4.pa() as u64, ASID + i as u32);
        if i == 0 {
            // The BSP starts the way PVH says: 32-bit protected mode, paging off.
            let mut v = Vmcb::new(f.bytes_mut());
            set_pvh_state(&mut v, loaded.entry);
            if let Err(e) = svm::validate_guest_state(&v) {
                p("[KVM] linux: guest state rejected: ");
                p(e.name());
                p("\n");
                return;
            }
        }
        *c.vmcb.lock() = Some(f);
    }
    ctl[0].set(CpuState::Runnable);

    // The devices. The gateway's queue and TCP rings are lent to it for good, so it is a pooled
    // `'static` value handed from guest to guest.
    // Only a service guest with `vm_outbound` gets a route out; everything else has the ports forwarded in.
    let outbound = service && vm.spec.as_ref().is_some_and(|s| s.outbound);
    if outbound {
        p("[KVM] linux: outbound networking on (the guest's TCP and DNS leave through the host)\n");
    }
    let blk = disk.map(|d| (MmioBlk::new(d.sectors()), d));
    let net = if guest.has_net() {
        let Some(mut gw) = take_gateway() else {
            p("[KVM] linux: no memory for the gateway\n");
            return;
        };
        gw.set_outbound(outbound);
        Some((MmioNet::new(svm::gateway::GUEST_MAC), gw))
    } else {
        None
    };
    let relay = service.then(|| Relay::new(vm.forwards(), outbound));
    if crate::lapic::tsc_uptime_us().is_none() {
        p("[KVM] linux: warning: host uptime is 10 ms ticks; PIT timing will be coarse\n");
    }
    let started_us = crate::net::uptime_us();
    let mut plat = Platform::new(guest, n_cpu);
    plat.last_tx_us = started_us;
    let sh = Arc::new(Shared {
        guest,
        nrips,
        service,
        n_cpu,
        ram,
        _npt: npt,
        dev: Spinlock::new(Devices { plat, blk, net, relay, scratch, log: ExitLog { cpuid: 0, msr: 0, exc: 0 } }),
        cpu: ctl,
        place: {
            let mut a = [0u8; MAX_VCPUS];
            a[..akuma_vm_spec::cores::MAX_PLACED].copy_from_slice(&place);
            a
        },
        stop: AtomicBool::new(false),
        why: Spinlock::new(Why { set: false, fatal: false, reason: "exit budget spent" }),
        started_us,
        totals: Spinlock::new(Counters::default()),
        start_gpa: l.start_gpa,
    });

    // Groups: one per distinct host core, the owner's (processor 0's) first.
    let mut group_of_core = [0u32; crate::smp::MAX_CPUS];
    for i in 0..n_cpu {
        group_of_core[usize::from(sh.place[i])] |= 1 << i;
    }
    // A core the host did not start at boot (`smp=` cut it off) is started now, for this VM.
    for core in 0..crate::smp::MAX_CPUS {
        if group_of_core[core] != 0 && !crate::smp::ensure_started(core) {
            p("[KVM] linux: could not start core ");
            dec(core as u64);
            p("\n");
            return;
        }
    }
    let owner_core = usize::from(sh.place[0]);
    // The coordinator runs the owner group, so it has to be on that core: its state (SVME, the
    // host save area) is per core. A boot-time rehearsal is already there.
    let mut started = true;
    if owner_core != here {
        started = crate::sched::repin_current(owner_core);
        if !started {
            p("[KVM] linux: could not move the VM task to its core\n");
            let _ = crate::sched::repin_current(here);
        }
    }
    let mut workers = 0usize;
    if started {
        for core in 0..crate::smp::MAX_CPUS {
            if group_of_core[core] == 0 || core == owner_core {
                continue;
            }
            if !ensure_worker(core) {
                sh.request_stop("no task slot for a virtual processor's core", true);
                break;
            }
            *JOBS[core].lock() = Some(Job { sh: Arc::clone(&sh), group: group_of_core[core] });
            workers += 1;
        }
    } else {
        sh.request_stop("the VM task could not be moved to its core", true);
    }
    // Keep the guest's cores to its own tasks while it runs (never core 0).
    let mut reserve = 0u64;
    if service {
        for i in 0..n_cpu {
            reserve |= 1 << sh.place[i];
        }
        crate::sched::reserve_cores(reserve | super::super::boot_reserved());
    }

    crumb(0x55);
    p("[KVM] linux: running (start=");
    hx(l.start_gpa, 8);
    p(", ram=");
    dec(ram_bytes >> 20);
    p(" MiB, vcpus=");
    dec(n_cpu as u64);
    p(", cores=");
    for i in 0..n_cpu {
        if i > 0 {
            p(",");
        }
        dec(u64::from(sh.place[i]));
    }
    p(")\n");

    if started {
        serve(&sh, group_of_core[owner_core], true);
    }
    // The owner group is done (it asked for the stop, or the stop reached it): every other task
    // must be too before anything they share is torn down. A worker that never started still
    // holds its job; take those back.
    sh.request_stop("run ended", false);
    for core in 0..crate::smp::MAX_CPUS {
        if group_of_core[core] != 0 && core != owner_core {
            if let Some(job) = JOBS[core].lock().take() {
                drop(job);
                workers = workers.saturating_sub(1);
            }
        }
    }
    let _ = workers;
    let mut waited = 0u32;
    while Arc::strong_count(&sh) > 1 {
        akuma_cpu::daif::mask_irq();
        let _ = crate::lapic::arm_oneshot_in_us(500);
        crate::sched::allow_tick();
        akuma_cpu::daif::unmask_irq();
        crate::sched::yield_now();
        waited += 1;
        if waited % 4000 == 0 {
            p("[KVM] linux: still waiting for a virtual processor's task\n");
        }
    }
    if service {
        crate::sched::reserve_cores(super::super::boot_reserved());
    }
    if owner_core != here && started {
        let _ = crate::sched::repin_current(here);
    }
    report(&sh, n_cpu);
}

/// The end-of-run summary, in the shape the rehearsal scripts and the ryzen logs read.
fn report(sh: &Shared, n_cpu: usize) {
    let guest = sh.guest;
    crumb(0x59);
    let (stopped, fatal) = {
        let w = sh.why.lock();
        (w.reason, w.fatal)
    };
    let c = *sh.totals.lock();
    let mut dg = lock_dev(sh);
    let Devices { plat, net, relay, .. } = &mut *dg;
    if n_cpu > 1 {
        for j in 0..n_cpu {
            p("\n[KVM] linux: cpu ");
            dec(j as u64);
            p(match sh.cpu[j].state() {
                CpuState::Parked => " parked",
                CpuState::Runnable => " runnable",
                CpuState::Halted => " halted",
                CpuState::Dead => " halted with interrupts off",
            });
            p(" rip=");
            hx(sh.cpu[j].rip.load(Ordering::Relaxed), 16);
        }
    }
    // Give the host's ports back: a restarted guest binds the same ones.
    if let (Some(r), Some((_, wire))) = (relay.as_mut(), net.as_mut()) {
        r.close(wire);
    }
    p("\n[KVM] linux: stopped: ");
    p(stopped);
    p("\n[KVM] linux: exits=");
    dec(c.exits);
    p(" cpuid=");
    dec(c.cpuid);
    p(" msr=");
    dec(c.msr);
    p(" io=");
    dec(c.io);
    p(" irq=");
    dec(c.irq);
    p(" exc=");
    dec(c.exc);
    p(" injected=");
    dec(c.inj);
    p(" hlt=");
    dec(c.hlt);
    p(" mmio=");
    dec(c.mmio);
    p(" lapic=");
    dec(c.lapic);
    p(" lapic_irq=");
    dec(c.ticks);
    if n_cpu > 1 {
        p(" vcpus=");
        dec(n_cpu as u64);
        p(" ipi=");
        dec(c.ipi);
        p(" init=");
        dec(c.init);
        p(" sipi=");
        dec(c.sipi);
        p(" pause=");
        dec(c.pause);
    }
    if let Some((_, wire)) = net.as_ref() {
        let s = wire.stats;
        p(" net_mmio=");
        dec(c.net);
        p(" net_frames=");
        dec(s.frames);
        p(" arp=");
        dec(s.arp_replies);
        p(" dhcp_offer=");
        dec(s.dhcp_offers);
        p(" dhcp_ack=");
        dec(s.dhcp_acks);
        p(" icmp=");
        dec(s.icmp_echoes);
        p(" udp_echo=");
        dec(s.udp_echoes);
        p(" net_ignored=");
        dec(s.ignored);
        p(" net_lost=");
        dec(s.lost);
    }
    p(" console_bytes=");
    dec(plat.tx_bytes);
    p(" rip=");
    hx(sh.cpu[0].rip.load(Ordering::Relaxed), 16);
    p("\n");
    // A Linux run passes on its banner; the Akuma guest must also have run its init to the end
    // (the marker the script on its disk prints). The SMP guest must also have started every
    // processor. The NIC guest must also have been leased its address.
    let net_ok = net.as_ref().is_none_or(|(_, wire)| wire.stats.dhcp_acks > 0);
    let pass = plat.banner.seen && !fatal && (guest.is_linux() || plat.done.seen) && (n_cpu == 1 || plat.smp_up.seen) && net_ok;
    p("[KVM] ");
    p(guest.name());
    p(" guest: ");
    p(if pass { "PASS" } else { "FAIL" });
    p(" (banner=");
    p(if plat.banner.seen { "yes" } else { "no" });
    if guest.is_akuma() {
        p(", init=");
        p(if plat.done.seen { "yes" } else { "no" });
        if n_cpu > 1 {
            p(", smp=");
            p(if plat.smp_up.seen { "yes" } else { "no" });
        }
        if guest.has_net() {
            p(", net=");
            p(if net_ok { "yes" } else { "no" });
        }
    }
    p(")\n");
    // The gateway goes back to the pool for the next guest.
    if let Some((_, gw)) = net.take() {
        *GATEWAY.lock() = Some(gw);
    }
}
