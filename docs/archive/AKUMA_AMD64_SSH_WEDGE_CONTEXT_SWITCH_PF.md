# Akuma/amd64: the ssh/apk wedge — a context-switch page fault, not a lock bug

**Grade: C** (active investigation, 2026-09-12; the crash is reproduced in
QEMU with a symbolized faulting rip, the culprit thread race is not yet fixed).
Supersedes the "self-reset" and "BKL storm" theories in
`AKUMA_AMD64_USB_XHCI.md` for this symptom: those were consequences.

## The symptom chain, and what each stage was

Under ring-3 churn — an ssh login burst, or `apk`'s fork/exec/write storm —
at SMP=4, the box wedges in four stages that each look like a different bug:

1. **A ring-0 page fault inside the context switch.** `#PF: not-present
   write from ring 0`, faulting rip = `akuma_threading_x86_switch_context`
   (`0xffffffff8039ee23`, +0xb into the asm), on a sane kernel stack. The
   switch writes the outgoing thread's rsp through `mov [rdi], rsp` — a
   write to task bookkeeping whose page is *not present*. QEMU repro:
   `logs/trial-1.log` (fresh SMP=4 boot, first connection runs `apk
   update`). The metal photograph of the same day shows the same shape
   (`#PF … from ring 0`, then a second fault with `rip=0x86`) — two cores
   faulting back to back.
2. **The faulting core halts** — `fatal()` prints and `halt()`s with
   interrupts off. That core is now gone but still counted as online.
3. **The next TLB shootdown never completes.** A peer that halted never
   acknowledges, so the next `AllCores` flush spins forever in
   `wait_for_acks` — `[TLB] stuck: 2 peer(s) unacked` — *holding the BKL*
   (the flush call sites run BKL-held; `shootdown.rs`'s deadlock argument
   assumes every peer can ack, and a halted peer is the case it does not
   cover).
4. **Everything else queues on the BKL** — `[BKL] stuck` lines from every
   waiting core, netpoll starves, ssh dies, the box looks hung. Under the
   stall storm the xHCI transport also degrades (reads off the disk fail
   with E-IO), which then presents as `sshd: failed to spawn '/bin/sh'`
   (exec cannot read the image) — the same signature as the USB doc's
   `lib`/`var`/`public` mystery, arrived at from the top.

So: the BKL storm is not the disease, the TLB stuck is not the disease —
both are downstream of one dead core.

## What the tagged attribution added (and what it named)

The BKL-hold profiler is now wired on amd64: syscall entry tags the holder
with the x86_64 syscall number, faults stamp `fault`, the timer stamps
`irq/sched`, idle stamps `idle`, netpoll stamps `netpoll`
(`usermode.rs`/`idt.rs`/`sched.rs`/`net.rs`; amd64 previously installed no
tags, so every line read `tag=511`). One boot paid for it:

- `tag=1` (`write`) storms — console drain under sshd output, and file
  writes during the apk burst;
- `tag=57` (`fork`) — process creation holds the BKL past the stuck
  threshold under shell-spawn churn;
- `tag=11` (`munmap`) — the wedged holder in the QEMU repro, waiting for
  the TLB acks;
- `tag=500` — fault service, firing during the crash itself.

These are real holds worth carving out *after* the crash is fixed — a
kernel that dies inside its scheduler cannot be A/B'd.

## What apk proved (and did not)

- **apk works on the ramdisk**: SMP=1 QEMU, `apk update` → `OK: 28641
  distinct packages available`. The tool, the network stack, and DNS are
  fine.
- `RING-3 CHECK: OK` at SMP=4 (40 sessions) passed on some boots — the
  crash is intermittent, needs the fork/exec/exit churn to hit the bad
  interleaving.
- The crash is NOT apk-specific: the first QEMU repro fired during ssh
  *auth* before any apk command ran.

## Diagnostics added along the way (all uncommitted on the trunk)

- **`fatal()` dumps 64 stack words** (`idt.rs`), not 5 — there are no frame
  pointers in a default build, so the raw stack is the only backtrace;
  symbolize with `nm target/x86_64-unknown-none/release/akuma-amd64`.
  On the metal `fatal` halts, so the dump stays on the television —
  photographable, which is how the metal evidence was captured.
- **GDB=1 on `amd64/run.sh`** — `-gdb tcp::1234 -S`, the x86 counterpart of
  the aarch64 runner, for catching the fault live.
- **BKL tags** (above) and the **xHCI transition tracking / device model**
  described in `AKUMA_AMD64_USB_XHCI.md` § 2026-09-12 — same boot, same
  tree.
- Known tear: with several cores printing concurrently, exception dumps
  interleave with `[BKL] stuck` lines (the metal photograph shows register
  lines shredded mid-print). The metal's *first* dumps of this crash were
  unusable for this reason. Atomic/fenced fatal printing is still owed.

## The suspect, for whoever fixes it

`akuma_threading_x86_switch_context` writes through `&mut Context` (the
`mov [rdi], rsp`) of the **outgoing** thread, then switches to the incoming
thread's stack. A not-present write there means a thread was scheduled
whose `Context`/stack page is unmapped — the obvious race is exit teardown
(stack freed / `Context` page reclaimed) versus the thread still being in
the run queue, or its per-slot statics being unmapped while `current_tid`
still points at the slot. The fork/exit churn of apk (and the ring-3
check's grandchild shape) is exactly the load that makes the window wide.
The fix should name the unmap: instrument `reclaim`/stack-free to poison
the `Context` magic (the fault would then be a *readable* signature, not a
write fault), or take the slot off the run queue before the first unmap.

## Reproducing

```sh
SMP=4 HTTP_PORT=8084 SSH_PORT=2258 INIT=/bin/sshd sh amd64/run.sh   # boot
# then hammer: ssh sessions of `( ls /bin; ls /bin ); echo r$$`, or
python3 scripts/utils/amd64_ring3_check.py --smp 4 --ssh-port 2258 --http-port 8084
```

Intermittent — the trial that caught it was a fresh boot whose *first*
connection ran `apk update`. Watch the log for `not-present write from
ring 0`.

## Background

- `docs/archive/AKUMA_AMD64_USB_XHCI.md` § 2026-09-12 — the decode fix, the
  device model, the BKL `tag=511` story this extends.
- `docs/runbooks/amd64-bare-metal-loop.md` — the trash box loop, the ring-3
  check.
