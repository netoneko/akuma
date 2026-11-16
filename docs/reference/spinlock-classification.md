# Static spinlocks: which ones an interrupt can reach

2026-11-17. `TYPE_SYSTEM_SAFETY_REPORT.md` §9 asked for the ~100 `static` spinlocks to be classified before more of them
become [`IrqSpinlock`](../../crates/akuma-irq-lock/src/lib.rs). `scripts/spinlock_audit.py` reads them (text, not a call graph),
so the numbers are a map for a reader, not a proof. Reproduce with `scripts/spinlock_audit.py [--list|--unmasked]`.

A **site** is a `NAME.lock()` / `NAME.try_lock()` outside tests. It is **masked** when it sits inside `with_irqs_disabled(..)`,
or in a function that also creates an `IrqGuard`/`PreemptGuard` or calls `irq_save_mask`.

| Class | Locks | Meaning |
|---|---|---|
| already `IrqSpinlock` | 4 | `IRQ_HANDLERS`, `POOL`, `DMESG`, `CONSOLE_LOCK` |
| **all sites masked** | 28 | every production site already masks; the type would make that unforgettable |
| **mixed** | 13 | some sites mask and some do not; the only class where a site can be wrong |
| **never masked** | 49 | no production site masks |
| no production site found | 3 | `TERMINAL_STATES`, `NODENAME`, `DOMAINNAME` |

## Mixed: read, and almost all are benign

Each unmasked site of the 13 was read (`--unmasked`). None is a demonstrated deadlock.

| Lock | Unmasked sites | Verdict |
|---|---|---|
| `PMM`, `COW_EVER`, `TALC`, `CLAIMED_SPANS`, `NETWORK`, `PAD` | one to four, all in `init`/`build`/`add_ram`/`reserve_range`/`register_claimed_span`, and a doc comment | boot-time or setup paths that run before interrupts are live; harmless |
| `AIO_CONTEXTS`, `THREAD_PID_MAP`, `XHCI` | comments that name the lock; the code sites are masked or guarded | false matches of the text scan |
| `VFORK_WAITERS` | two `*_for_test` helpers | test-only |
| `TIMERFD_TABLE` | 4 of 5 sites unmasked | syscall context only: nothing in an interrupt takes it; inconsistent, not wrong |
| `ALARM_QUEUE` | `on_timer_interrupt` | already in the timer interrupt, masked by being there; the other two sites are guarded |
| **`KEYS`** (amd64 xhci) | `keyboard_getb`, `keyboard_has_byte` (readers, task context, IRQs on) vs `kbd_event` (producer, under the IRQ-masked controller lock) | **a real hazard, fixed 2026-11-17**: on one core a tick landing while a reader held `KEYS` could switch to the pump, which spins on `KEYS` with IRQs masked while the holder cannot run again. It is an `IrqSpinlock` now. Builds on amd64; not exercised at runtime (QEMU's microvm has no xHCI keyboard) |

## Never masked: three checked for interrupt reach

The 49 include several locks that sound interrupt-adjacent. Read: `ALLOCATION_REGISTRY` is behind `ENABLE_ALLOCATION_REGISTRY:
bool = false` (compiled out); `FRAME_TRACKER` is gated the same way; `KLOG` (amd64 `serial.rs`) uses `try_lock` on both sides by
design ("never blocks"). The rest (`MOUNT_TABLE`, the rump proxies, the kvm and driver state, the VFS tables, `CONSOLE` in
`fbcon`) are taken from syscall or driver-poll context. A real classification of these needs each interrupt handler's call
graph; none is claimed here.

## Recommendation

1. **Migrate the 28 always-masked locks one at a time, and never mask twice.** `IrqSpinlock::lock()` masks, so wrapping it in
   `with_irqs_disabled(|| X.lock())` pays for two masks (and two `isb`s on aarch64). Two correct shapes, both used now:
   - the wrapper only protects the lock: delete the wrapper (`POOL`: nine sites, done 2026-11-17);
   - the wrapper protects more than the lock (the console's UART byte sequence): hold an `IrqGuard` yourself and take the lock
     with `IrqSpinlock::lock_masked(&guard)` / `try_lock_masked`, which borrows the guard as proof that IRQs are masked, so
     there is no second mask and the lock guard cannot outlive the masked section (`DMESG`, `CONSOLE_LOCK`).
   Per-lock edit (about 150 sites for the 28), with both boot suites run per batch.
2. **`KEYS`** is done (above).
3. **Leave the 49** unless an interrupt handler is shown to reach one; add them to a list of "taken only from task context" so
   the next audit starts from a decision, not a re-scan.
4. A true classification needs a call graph from the registered handlers (`IRQ_HANDLERS` entries, the timer tick, the amd64
   vector stubs) to each lock; the text scan here is the cheap first cut.
