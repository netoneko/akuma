# Akuma/amd64: the ssh/apk wedge — two cores on one kernel stack

**Grade: B** (root-caused and fixed 2026-09-12, verified on all three machines;
the structural fragility behind it is still open — see "The fix" below).
Supersedes the
"self-reset" and "BKL storm" theories in `AKUMA_AMD64_USB_XHCI.md` for this
symptom, and the "context-switch page fault" theory this document itself opened
with: all three were consequences.

> **CORRECTION 2026-09-12 (later the same day).** The title of this document
> used to say *page fault*, and §1 below described a `#PF: not-present write
> from ring 0` at `akuma_threading_x86_switch_context+0xb` (the
> `mov [rdi], rsp`). **That is not what happens.** Two things were wrong:
>
> * **The vector.** Every vector but 0/6/8/13/14 was installed as one
>   `unhandled` handler that printed the same four words for all of them, so
>   the dump could not say which exception fired. It is **`#DB`**, vector 1.
>   Per-vector stubs (`idt.rs`, `exception_stubs!`) name it now, and the first
>   boot with them said so.
> * **The rip.** The `+0xb` reading came from symbolizing against a build that
>   was not the one that crashed. Symbolized against the binary that produced
>   it, the faulting rip is `x86_yield_now+0x215`, which `objdump` shows is the
>   instruction *immediately after* `callq …x86_switch_context`:
>
>   ```
>   …01b0: e8 c3 fd ff ff   callq  akuma_threading_x86_switch_context
>   …01b5: b0 01            movb   $0x1, %al          <- faulting rip
>   ```
>
> That is the signature of the switch's closing `popfq; ret` restoring a saved
> flags word with **`TF` set**: `popfq` arms single-step, the `ret` retires, and
> the `#DB` lands on the first instruction of the resumed thread. There is no
> `#DB` handler, so `fatal()` halts the core — and stages 2-4 of the chain below
> (halted core, TLB acks that never complete, BKL storm, ssh death) all still
> hold. Only stage 1 is a different exception than this document first said.
>
> Three captures, three flags words, all impossible for this kernel:
> `0x202312`, `0x243392`, `0x204312` — each carries `TF`, and between them
> `IOPL=2`, `IOPL=3`, `NT` and `AC`. The kernel sets none of those, and `AC` in
> particular cannot have arrived through `syscall` (`IA32_FMASK` clears it), so
> the value came off a saved frame rather than out of a `syscall` entry.
>
> **The evidence is destroyed by the crash itself**, which is why it took three
> captures to get this far: the `#DB` frame the CPU pushes lands exactly on the
> 64 bytes the switch just popped. A `[rsp-64..rsp)` dump added to read the
> restored frame shows the exception frame instead — `rip`, `cs`, `rflags`,
> `rsp`, `ss` in order, verbatim. The frame has to be read **before** the
> `popfq`; `x86_check_incoming_frame` in `akuma-threading` does that now.

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

## The cause: a context switch taken with no kernel lock held

`x86_yield_now` states its own cross-core safety argument:

> `ON_CPU` is what keeps two cores off one stack. It is cleared for the outgoing
> thread **before** the switch, but no other core can observe that until this
> core releases the kernel lock, which the incoming thread does only after the
> switch has completed.

That is an argument only while the lock is in fact held. It is not, on one path:

- `akuma_bkl::sync::lock_bounded` backs a contended spinlock off by calling the
  registered **yield hook** — a voluntary handoff, so the holder runs at once
  instead of at the next tick — and it does so **holding nothing**, which is the
  whole point of `lock_bounded`.
- `akuma_exec::init` registers `threading::yield_now` as that hook. On this
  target `yield_now` is a context switch.
- `amd64/src/console.rs` reaches `lock_bounded` on the terminal state, which
  under an ssh session is contended constantly.

*(That path is real in the shared crates but **not** what fires here: amd64
never calls `akuma_exec::init`, so the hook is unregistered and
`akuma_bkl::yield_now` degrades to a spin hint. It is recorded because the next
person to wire `akuma_exec::init` on this target will reintroduce it.)*

**What actually fires is `sched::yield_now` itself.** It opens a BKL drop
window, consumes the tick's reschedule request, and switches:

```rust
pub fn yield_now() {
    smp::bkl_drop_window();          // a NO-OP when the caller holds no lock
    if smp::take_need_resched() { … }
    threading::yield_now();          // the switch
}
```

`bkl_drop_window` releases, spins and re-acquires — **for a caller that holds
the lock**. For one that does not it returns immediately, and the switch below
runs unlocked. Several of its ~20 callers are exactly that: kernel wait loops
that hold nothing (`net.rs`'s `blocking_relax` and its poll loops,
`console.rs`, `dns.rs`, `fd.rs`, the `smp.rs` drive loops).

`ON_CPU[cur]` is then cleared before the stack moves and is visible to peers
**immediately** — a peer's `x86_pick_next` takes the outgoing slot while this
core is still executing on its stack, loads its *stale* saved `rsp`, and starts
running there. Two cores, one stack.

Every symptom follows from that, and each looked like a different bug:

| observed | what it is |
|---|---|
| `popfq` restores `rflags` with `TF`, `IOPL`, `NT`, `AC` | the saved flags word rewritten by the other core's pushes |
| `iretq` frame whose `CS` is `0x0000013c_00000008` | likewise — hardware pushes `CS` zero-extended, so a small integer in the upper half is a memory write, not a CPU push |
| a single-step trap at `fork_process+0x9` on a *different* core | every `IrqGuard` ends in a `popfq`; any restored flags word will do, not just the switch's |
| a frame word read at `+0x30` turning up at `+0x0` a few microseconds later | the other core running on the stack, caught between two reads |

**Measured**: `[SWITCH NO-BKL] from=8 to=5 core=2 via=yield_now`, four times in
one `apk update` boot, always on a secondary — from an assertion placed where
the claim is *used* rather than where it is written, plus a per-core tag naming
which of this file's four entries into the scheduler the core came through.

Two instrumentation dead ends, recorded so they cost nobody a second afternoon:

- **Stack walking does not work here.** A release build has no frame pointers,
  the frame at the switch is two deep, and everything above it is stale data
  from when that stack was used more deeply — it produced three plausible-looking
  symbols (`parse_directory`, `socket_egress`, `RawVecInner::finish_grow`)
  belonging to no live frame. A one-relaxed-store-per-yield tag answered in one
  boot what four rounds of stack dumps did not.
- **Do not build the report in a buffer on the stack you are reading.** A
  512-byte `StackWriter` local sits in the frame being walked, so words 16..80
  read back as the report's own zeroed scratch. Snapshot into a `static` first.

### The fix

`sched::yield_now` takes the BKL for the switch when its caller has none, and
puts it back exactly as it found it. One place, correct for every caller.

Holding the lock across a switch is this kernel's normal state, not a new idea:
the BKL belongs to the *core*, every kernel task is born holding it, and
`idle_loop` is entered with it at depth 1.

**Not** "refuse to switch when unlocked" — those callers are wait loops with no
other way to make progress, and a kernel thread spinning in ring 0 is only
*asked* to reschedule by the tick, never forced, so refusing would livelock
them (the same shape as the livelock `bkl_drop_window`'s own comment records
from 2026-09-05).

`switches_without_bkl()` stays as the standing guard and the boot suite asserts
it is zero; `yields_that_took_bkl()` counts the guard *working* and is printed,
not asserted.

**The deeper fragility is not fixed and should be.** `ON_CPU[cur]` being cleared
before the stack moves is only ever safe by an argument about a *different*
lock. The structural fix is Linux's: hand the outgoing slot to the resumed
thread through a per-core `prev`, and clear `ON_CPU[prev]` after the switch
returns — including on the fresh-thread trampoline path, which never returns to
a call site. Until then, `switches_without_bkl()` is the guard, and the boot
suite asserts it is zero.

## What else is fixed, and what is not

| | |
|---|---|
| **`#DB` has a handler** (`idt.rs`, `debug_entry`/`debug_dispatch`) | Ring 3 gets a `SIGTRAP` — a program single-stepping itself was killing the *kernel*. Ring 0 is disarmed: `TF` cleared in the frame `iretq` restores, counted, first eight reported as `[DB]`. Continuing is not a guess: the crash's own evidence shows the restored **return address was correct** and only the flags word was wrong, so the resumed thread is otherwise intact. Boot suite asserts `ring0_debug_traps() == 0`. |
| **`IA32_FMASK` masks `TF`, `IOPL` and `NT`** (`usermode.rs`) | `syscall` is not an interrupt gate and does not clear `TF`, and `signal::sanitize_rflags` deliberately *permits* `TF` (as Linux does, for debuggers). That pair was a ring-3 kill switch for the machine: set `TF` through a signal frame, issue any syscall, and the kernel single-steps into a `#DB` it could not handle. Linux masks these in `MSR_SYSCALL_MASK` for exactly this reason. **This is not the crash below** — the captured words carry `AC`, which the old mask already cleared, so they did not arrive through `syscall`. |
| **The switch checks the frame it is about to restore** (`akuma-threading`, `x86_check_incoming_frame`) | Reads `[rsp+48]` before the `popfq` and prints the whole frame if the flags cannot be kernel flags. It has to be *before*, because the `#DB` frame the CPU pushes overwrites the popped one exactly. |
| **`wait_for_acks` stops waiting for a dead peer** (`shootdown.rs`) | Stage 3 above. A peer that took a fatal exception halts with interrupts off and never acknowledges, so the sender spun forever **holding the BKL** — a silent wedge. It halts instead, quietly, leaving the dump as the last thing on the console. |
| **The dump is legible** (`idt.rs`, `console.rs`) | Peer cores' `[BKL] stuck`/`[TLB] stuck` no longer interleave with it (`claim_console_exclusive`); every vector has a name instead of one `unhandled` for all of them; `cr3`, `freed=`, `core`, `task_slot` and `slot_root` are printed; and the 64-word stack dump's `>= KERNEL_VMA` guard is now `>= PHYSMAP_BASE`, which is where amd64 thread stacks actually live — it had never printed for this crash. |
| **Kernel stacks have canaries** (`sched.rs`, `paint_canary`/`check_canaries`) | These are `vec![0u8; 32K].leak()` — no guard page, and `akuma-threading`'s canary machinery only paints the stacks *it* allocates from the PMM, not the two this target supplies per task. Four words at each base, checked on both sides of every switch, asserted zero by the boot suite. It was the other candidate for "a word rewritten while its owner was not looking", and ruling it out is what left the scheduler. |
| **A fatal dump owns the UART** (`serial.rs`, `begin_fatal`/`fatal_*`) | `serial::LOCK` is best-effort by design — a core that cannot take it inside the budget prints anyway, so that a lock held by a core that died mid-line cannot silence the next report. The cost is that `put_hex` emits sixteen digits under one acquire and a peer's byte lands **inside a number**: captures read `ffffyfff80296506` and `cs=0x0000]00000000008`. Now the first core into `fatal` silences every other writer for the rest of the boot, and a second core that gets there stops instead of printing into the first dump. |
| **The free gate's saved-context arm exists on x86** (`akuma-threading`, `sched.rs`) | `any_saved_ctx_on_l0` answered a hardcoded `None` under "nothing on this target saves a `ttbr0`-shaped value anywhere". Expired: `Machine::space_root` is one per task slot and `hook_switch_to` writes it to `CR3`. Now a registered probe over the task table. **Also not this crash** — `freed=0` on every capture and the tripwire never fired — but the gate's own documentation claimed a protection that was not there. |

## What the captures said, and what they ruled out

*(Written while the cause was still unknown; kept because the eliminations are
what left one candidate standing.)*

Four captures (three `#DB`, one ring-3 `#GP` the kernel survived):

| | rip | flags restored | slot |
|---|---|---|---|
| 1 | `x86_yield_now+0x215` | `0x202312` | 8, user root |
| 2 | `x86_yield_now+0x215` | `0x243392` | 8, user root |
| 3 | `x86_yield_now+0x215` | `0x243392` | 6, **kernel root** (the console pump daemon) |
| 4 | `x86_yield_now+0x215` | `0x204312` | 8, user root |

- **Not a page fault and not a lock bug** — see the correction at the top.
- **Not the address-space free gate** — `freed=0` every time, no `[SWITCH FREED-CR3]`.
- **Not `syscall` entry leaking user flags** — two of the words carry `AC`, which
  the pre-fix `IA32_FMASK` already cleared.
- **Not a shifted frame** — the `ret` landed at the correct return address every
  time (that is how the `rip` is always the same instruction), so `[rsp+56]` was
  intact while `[rsp+48]` was not. Whatever wrote it wrote *one word*.
- **Not user-settable flags** — `IOPL` and `NT` appear in the words, and
  `sanitize_rflags` strips both; ring 3 cannot set them either.
- **Not confined to user threads** — capture 3 is a kernel daemon in the kernel
  root.

Each value is `0x200000` (`ID`) plus a small remainder, and all four sit just
above 2 MiB when read as integers. That is either a coincidence of flag bits or
a hint that the word is a size or offset; the evidence does not settle it.

## The reproduction is load-shaped, and the harness can lie

Four crashes in five *booted* runs before the tripwire, none in eleven after —
but the eleven ran **two to four VMs at once**, which under TCG serialises each
guest and would suppress an SMP race on its own. Two harness traps cost real
time here, both worth knowing:

- **QEMU takes a write lock on the drive.** Two trials sharing
  `target/x86_64-unknown-none/release/amd64-root.img` means the second never
  boots at all — and a harness that greps its log for `EXCEPTION` scores that as
  a **pass**. Two of the nine "results" above were VMs that did not exist. Give
  each trial its own image copy, and check the log has more than ten lines.
- **Bare metal wedged with every diagnostic in the table above in place** and
  none of them was the fix (staged and rebooted 2026-09-12; `apk update` over
  ssh stopped answering on both ports and the box needed a power cycle). It is
  the gate, and it is the one that says the diagnostics were not the fix. After
  the `yield_now` change it runs: `apk update` twice plus shell churn,
  `OK: 28641 distinct packages available`, box still answering, zero detector
  lines in `dmesg`.

## Verified

| | before | after |
|---|---|---|
| local QEMU/TCG `SMP=4`, solo, `apk update` on first connection | **6 crashes in 13** booted trials | **0 in 16**, every detector silent |
| box Firecracker (KVM) `SMP=4` boot suite | 653 passed, 0 failed | **656 passed, 0 failed** (three new assertions) |
| bare metal `SMP=4`, `apk update` over ssh | **wedged**, needed a power cycle | **6/6 rounds** of shell churn + `apk update`, `OK: 28641 distinct packages available` every time, 13-15 s each, box still answering, no detector lines |

### `[BKL] stuck` on the metal is not this bug

A post-fix metal run shows a few hundred `[BKL] stuck` lines, and they are
**load-driven lock contention, not the wedge chain**. What separates them:

- the fatal chain needs a **dead core** — `EXCEPTION`, then `[TLB] stuck`
  because the halted peer never acknowledges. Neither appears. Only `[BKL]
  stuck` does, and the box stays fully responsive with `apk` completing in a
  steady 13-15 s.
- holders are `tag=501` (irq/sched), `tag=1` (`write`) and `tag=231`
  (`exit_group`) — the ordinary contenders, the same story
  `AKUMA_AMD64_USB_XHCI.md` § 2026-09-12 tells.
- **attributed by load type rather than assumed**: 60 rounds of pure fork/exec
  shell churn with no network at all produced 12 lines, and one `apk update`
  produced 12. If `yield_now`'s new `bkl_enter` were the driver it would skew
  to the network side, since its unlocked callers are predominantly the network
  and console wait loops. It does not.

**Not A/B'd against a pre-fix kernel on the metal**, deliberately: that kernel
wedges on this exact workload, so the measurement costs a power cycle and
returns nothing. Local QEMU cannot substitute — TCG is serialised enough that
`[BKL] stuck` never fires there at all (0 lines in all 16 solo trials, before
and after). Quantifying the fix's added contention needs
`yields_that_took_bkl()` readable at runtime; today it is reported only by the
boot suite, where it is legitimately 0 because the suite runs before any network
load.

Three assertions now stand in the boot suite, and each is the tripwire for one
of the things ruled out along the way:

```
debug: no ring-0 #DB was taken
debug: no kernel stack canary was overwritten
debug: every context switch held the kernel lock
```

**A harness trap that cost two of the nine early "results".** QEMU takes a write
lock on the drive, so two trials sharing
`target/x86_64-unknown-none/release/amd64-root.img` means the second never boots
— and a harness that greps its log for `EXCEPTION` scores that as a **pass**.
Give each trial its own image copy and check the log has more than ten lines.
And **run trials one at a time**: four concurrent TCG VMs serialise each guest
enough to hide this race completely (0 crashes in 11, against ~46% solo).

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
