# amd64: Claude Code wedged the ryzen whole — `mmap_regions` had no shootdown assist (2026-10-17)

**Status: FIXED and verified** in the ryzen Firecracker guest on a copy of the
metal root (3/3 clean on the fix, 2/2 wedged without it). The sibling of
[`AMD64_TLB_STUCK_AS_LOCK.md`](AMD64_TLB_STUCK_AS_LOCK.md): same shape, the
*other* per-process lock the `munmap` sender holds.

## Symptom

On the ryzen (metal, `smp=8`), starting Claude Code (`claude`, `claude-kimi`)
killed the whole box within a second or two: herdr froze on the console, no
ssh, no ping, the watchdog never fired. It looked like a regression of the
newest kernel, but the previous one did it too. What made it look new was the
trigger: it needs a **multi-threaded process mapping and unmapping while one
of its threads reads `/proc/<pid>/maps`**, under enough load (a second claude,
or herdr + claude, ~65+ live threads) for the race to land. klogd starved
before the box went silent, so the metal logs stopped at
`[threads] new high-water: 66 live user threads`.

## Reproduction (no metal needed)

`fc-p3.sh` on ryzen boots Firecracker on a dm-snapshot of the metal root (p3),
so the guest has the real claude and config while p3 stays untouched. In it:
60 `sleep` processes (to ~83 threads), then `claude` from
`/src/github.com/netoneko/akuma` under an ssh PTY. Unfixed: 2/2 wedged, 13
bytes of TUI and then nothing.

## Evidence

The guest's serial log: `[TLB] stuck: 1 peer(s) unacked … sender=1
missing=0x8` beside a `[BKL] stuck … owner=2 … tag=28` storm. Core 3 is
missing from the shootdown and is **not** a BKL waiter, which is the
"IRQ-masked wait outside the BKL" case `wait_for_acks` names.

Host-side `kvm_exit` RIPs (the procedure in
[`CONNECT_TIMEOUT_LOCK_INVERSION.md`](CONNECT_TIMEOUT_LOCK_INVERSION.md) §10),
symbolized against the booted ELF:

| vCPU | RIP | where |
|---|---|---|
| 1 | `…802f9781` | `shootdown::wait_for_acks` — the sender, holding the BKL |
| 3 | `…80317300` `pause` | `SlotTable::with_active_mut` → `fd::pid_map_rows` — spinning on `Process::mmap_regions` (`lock cmpxchg` on `0x298(%r12)`) |
| others | `…802b4b19` | `KernelLock::acquire` |

## Cause

`munmap`/`mprotect` hold `Process::mmap_regions` across the TLB flush they send
and so across `wait_for_acks`. The `/proc/<pid>/maps` read (`pid_map_rows`)
runs in the BKL-free VFS window, IRQ-masked, and took `mmap_regions` with a
plain `spinning_top::Spinlock::lock()`: no IPI can land, nothing services the
mailbox, and the sender waits for an acknowledgement from the core that waits
for the sender's lock. Every other core then queues on the BKL the sender holds.

`ProcAddressSpace::lock` got the fix for exactly this on 2026-10-01
(`masked_spin_assist` in its spin). `mmap_regions` is the second lock the same
sender holds and was left out.

## Fix

`akuma_exec::process::AssistedSpinlock<T>`: a `Spinlock` whose `lock()` is a
`try_lock` loop calling `akuma_bkl::sync::masked_spin_assist()` (amd64:
`shootdown::bkl_spin_assist`) on every failed attempt, the same body as
`ProcAddressSpace::lock_inner`. `Process::mmap_regions` is now one. It has the
same `.lock()` surface and guard type, so none of the 22 call sites changed. A
no-op off x86 bare metal.

## Still true

The BKL is not the outermost lock on amd64. Any lock a shootdown sender can
hold, taken anywhere IRQ-masked, needs the assist. The two known per-process
ones are now covered; a new one will wedge the same way and print the same
`missing=` with the missing core outside the `waiter=` set.

## Background

- [`AMD64_TLB_STUCK_AS_LOCK.md`](AMD64_TLB_STUCK_AS_LOCK.md) — the address-space-lock sibling
- [`CONNECT_TIMEOUT_LOCK_INVERSION.md`](CONNECT_TIMEOUT_LOCK_INVERSION.md) §10 — `kvm_exit` RIP capture
- [`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`](AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md) — a dead core behind `[TLB] stuck`
