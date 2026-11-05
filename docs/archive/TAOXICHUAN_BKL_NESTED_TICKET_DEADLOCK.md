# The tokio kernels' big lock: a tick that lands during a wait deadlocks every core (2026-11-05)

**Status: fixed in `akuma_tokio_kernel_core::bkl::enter`; gated on amd64 under load. Provenance not settled** (see "Open").

## Symptom

`sh shanghai/mw_gate.sh`, the `middleware` kernel on amd64 (SMP=4, QEMU TCG), at the start of the probe's 4-thread autolock contention
case: the console goes quiet after `[threads] new high-water: 10 live user threads` and prints `[BKL] cpu N stuck: owner cpu -1`. No
`MWPROBE RESULT` line. About 1 boot in 8 in a plain loop, 1 in 9 to 16 under CPU contention; 0 of 100 after the fix. aarch64 did not
show it (the same lock code, a different interrupt behaviour; not proven impossible there).

## How it was caught

The tcg rig takes `TX_QEMU_EXTRA='-monitor unix:<short path>,server,nowait'` (macOS caps a unix socket path at 104 bytes; a run
directory under `$TMPDIR` was too long and QEMU simply did not start, which looked like a hang). A capture script boots until the
console shows `[BKL] ... stuck`, then reads the guest through the monitor: `info registers -a`, the lock words by `nm` symbol, and each
core's stack. Reading a wedged guest this way takes seconds; sampling for it takes hours.

What the first dump showed: all four cores at one `rip`, inside `bkl::enter`'s wait loop; `OWNER = -1`, `DEPTH = 0`; **`NEXT = 0x988`,
`SERVING = 0x983`**. Five tickets outstanding for four spinners. Each spinner's own ticket is in `RBP` (0x984, 0x985, 0x986, 0x987)
and every core had read `SERVING = 0x983` into `RAX`: **the ticket being served was held by no running core.**

A throwaway kernel (not committed) logged, per ticket, the core that took it and the site, and who released it. Second wedge:
ticket `0x928` (served, never released) and ticket `0x929` were **both taken by cpu 0**.

## Cause

`enter` takes a ticket and spins on `SERVING == ticket`. It can be reached with interrupts **on** (the wait after `unlocked(f)` in
`park_halt`'s `sti; hlt`, or a syscall's own-tier path calling `restore`). If the timer tick lands during that wait, the handler calls
`enter` too. The re-entrancy test is `OWNER == this core`, which is false (the outer wait has not acquired), so the handler takes a *new*
ticket, one **behind** the suspended outer one. A ticket lock serves in order: the outer ticket comes up first and cannot run until the
handler returns, and the handler is waiting for a ticket that comes after the outer one. Every other core ends up queued behind them.
One suspended wait is enough to stop the machine.

The first dump's stacks matched: cores 0 and 1 sat inside `timer_trap` (interrupting `idle_main` and `bkl::unlocked<park_halt>`).

## Fix

`enter` masks interrupts before taking the ticket and restores them after the lock is owned and `DEPTH` is set. `BklCpu` gained
`irq_save`/`irq_restore` (default no-ops); both machines implement them with `akuma_cpu::daif::{save_and_mask_irq, restore}`. A tick that
arrives during the wait is taken once the core owns the lock, where its `enter` nests. The wait still services polled work
(`while_waiting`: the TLB shootdown), which is why that hook exists: a masked core cannot take the IPI.

## Verification

- **Host test** `bkl::tests::a_tick_that_lands_while_a_core_waits_does_not_queue_behind_the_wait`: a fake core whose tick lands on the
  first lap of a wait and asks for the lock. **It fails on the unfixed `enter`** (the assertion "a handler that waited behind its own
  suspended wait", bounded so it fails instead of hanging) **and passes with the fix.**
- **Under load, amd64, `middleware` kernel, SMP=4:** the unfixed tree wedged on boot 9 and boot 16 of two separate runs (16 busy loops
  on 12 host cores); the fixed kernel passed **100 of 100** of the same.
- Gate, both machines at SMP=4 after the fix: `MW GATE PASS`, 206 PASS lines (`shanghai/MIDDLEWARE_COMPOSITION.md`).

## Open

- **Provenance.** The pre-ETXTBSY baseline kernel passed 15 of 15 plain boots and was not run under the contention that reproduces the
  wedge, so this document does **not** claim ETXTBSY introduced it or that it was latent. The mechanism needs no ETXTBSY code; it needs a
  tick to land in a wait, and the new exec/exit work may only have made that window easier to hit. Running the baseline under the same
  stress would settle it.
- **`akuma-bkl` (the main kernels' lock) was not examined.** It is a different implementation. If it takes a ticket with interrupts
  enabled it has the same shape; its own `[BKL] stuck: owner=` storms in `docs/archive/` may be this.
- aarch64 got the same fix but never showed the wedge.

## Background

`crates/akuma-tokio-kernel-core/src/bkl.rs`; `scripts/kvm/taoxichuan/tcg_rig.py` (`TX_QEMU_EXTRA`); related: `FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md`
(another ticket-lock wedge, a different cause: read it before assuming this is the same one). **Not yet in
`TAOXICHUAN_BUG_FIX_LIST.md`:** `docs/runbooks/update-bug-fix-list.md` needs its branch-wide sweep and counted statistics, which was not
done here.
