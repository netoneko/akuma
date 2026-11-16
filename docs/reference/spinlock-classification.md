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
| **`KEYS`** (amd64 xhci) | `keyboard_getb`, `keyboard_has_byte` (reader side) vs `kbd_event` (producer) | **the one to look at**: if `kbd_event` can run from an interrupt, the unmasked readers can self-deadlock on one core. amd64 devices are polled (`docs/reference/…` build notes), so probably not today; it is the only mixed lock whose unmasked sites are not setup, test or comment |

## Never masked: three checked for interrupt reach

The 49 include several locks that sound interrupt-adjacent. Read: `ALLOCATION_REGISTRY` is behind `ENABLE_ALLOCATION_REGISTRY:
bool = false` (compiled out); `FRAME_TRACKER` is gated the same way; `KLOG` (amd64 `serial.rs`) uses `try_lock` on both sides by
design ("never blocks"). The rest (`MOUNT_TABLE`, the rump proxies, the kvm and driver state, the VFS tables, `CONSOLE` in
`fbcon`) are taken from syscall or driver-poll context. A real classification of these needs each interrupt handler's call
graph; none is claimed here.

## Recommendation

1. **Do not mass-migrate the 28 always-masked locks yet.** `IrqSpinlock::lock()` masks and re-saves `DAIF` (and issues an `isb`
   on aarch64) even when the caller already masked, so wrapping `with_irqs_disabled(|| X.lock())` in the new type pays the mask
   twice. The correct migration removes the outer `with_irqs_disabled` at every site of that lock in the same change, which is a
   per-lock edit (about 150 sites) and deserves a run of both boot suites per batch. **`POOL` was migrated without doing that**
   (its sites still wrap), so the scheduler pays a second mask; worth measuring, and worth removing those wrappers.
2. **Read `KEYS`** (amd64 xhci): decide whether `kbd_event` is interrupt-driven. If it is, mask the readers (or make it an
   `IrqSpinlock`).
3. **Leave the 49** unless an interrupt handler is shown to reach one; add them to a list of "taken only from task context" so
   the next audit starts from a decision, not a re-scan.
4. A true classification needs a call graph from the registered handlers (`IRQ_HANDLERS` entries, the timer tick, the amd64
   vector stubs) to each lock; the text scan here is the cheap first cut.
