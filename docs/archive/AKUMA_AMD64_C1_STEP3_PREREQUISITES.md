# amd64 C1 step 3: what had to exist before one syscall could cross the seam

**Date:** 2026-09-07
**Scope:** the four things `akuma-syscalls-glue` needed on this target before its
first arm (`uname`) could serve a real ring-3 caller. None of them was on the C1
hand-off prompt's list of prerequisites
(`proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`), and each is larger than all
three that were.
**Status:** all four landed and verified. `uname` is served by glue on QEMU,
Firecracker and bare metal.

## The shape of the problem

C1 folds `amd64/src/usermode.rs` into `akuma-syscalls-glue`, one arm at a time
(`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, trunk C). The gate for starting —
`cargo check -p akuma-syscalls-glue --target x86_64-unknown-none` — went green
with B3 (`docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md`), and the C1 dispatch
vocabulary landed after it (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`).
So the first arm was expected to be a one-line change: point `Syscall::Uname` at
`to_glue` and delete the local body.

It was four bugs, and **the boot self-test suite could not have caught any of
them**, for one shared reason:

> The suite runs inside a `BypassValidationGuard` and hands the syscall bodies
> **kernel-stack buffers**. Every check below that involves a user pointer is
> therefore vacuous under the suite: validation is off and the pointer is not a
> user pointer.

Each defect needed a real ring-3 caller to appear, and three of the four were
silent — a hang, a wrong-but-plausible `EFAULT`, and a panic on one boot
protocol only.

## 1. `akuma_exec::runtime::register` — the `ExecConfig` panic

Glue's user-copy helpers are `akuma_exec::process::user_access::*`, and the
first thing they touch is `akuma_exec::runtime::config()`, a
`akuma_primitives::Registered` cell:

```text
[PANIC] crates/akuma-not-even-once/src/lib.rs:208
        akuma-exec: ExecConfig not registered — call akuma_exec::init() first
```

`amd64/src/exec_runtime.rs` is the answer: this target's `ExecRuntime` +
`ExecConfig`, with every field labelled as one of three kinds — real, a no-op
that provably cannot fire, or a loud `not_wired!` stub that panics naming
itself and the step that will wire it. The module header argues category 3 at
length; the short version is that this tree's history is a list of what silent
plausible defaults cost.

**A hook firing there is not a bug in that file.** It is the next syscall family
asking for a subsystem C2 or C1 step 5 has not folded yet, and the message names
which one.

### The panic message was itself missing

The amd64 panic handler printed a location and **discarded the message**, so
every one of these arrived as a bare `lib.rs:208`. `Registered` exists to carry
a string; that string was being thrown away at the last step. Fixed in
`amd64/src/main.rs` the same day, with `safe_print!` and a fixed stack buffer —
a panic handler is the console-survives-the-allocator path by definition.

## 2. `stac`/`clac` in the shared copy loop — the hang

`akuma-user-access`'s x86 `copy_from_user_safe` was **the only x86 user copy in
the tree without SMAP brackets**. `amd64/src/uaccess.rs`'s own
`read_bytes`/`write_bytes` have had them since SMAP was turned on; the shared
crate — the one every glue-served syscall copies through — did not.

It survived because nothing on this target had ever called it with a real ring-3
pointer. Under the suite it gets kernel-stack buffers, which SMAP does not
police.

What made it expensive is how it presented: **a hang, not an error.** The `#PF`
lands inside `rep movsb`, the fixup turns it into `EFAULT`, and the caller
retries.

The old smoke test asserted the bug. It read `copy_from_user_safe` as "the
unbracketed copy" and checked that SMAP refused it — a true observation of a
defect, written down as an invariant. It now asserts the opposite (the shared
loop reads a user page and is byte-exact), and SMAP's own behaviour is still
observed rather than assumed, by turning the brackets off at
`akuma_user_access::set_smap_active` and watching the same read be refused.
That version tests one thing the old one could not: that the flag is actually
what drives the brackets.

## 3. `akuma_mmu::get_current_ttbr0` had no x86 arm — every folded syscall `EFAULT`

The function had an AArch64 arm (`ttbr0_el1()`) and a fallback returning `0`,
and x86_64 got the fallback. `is_current_user_range_mapped` reads it, finds a
root of `0`, and answers `false` — so **every pointer on this target failed
validation and every folded syscall returned `EFAULT` to a real caller.**

The x86 arm is `CR3`. The name stays the AArch64 one because the callers are
shared and what they want is "the root of the current user address space".

The walk it feeds (`x86_user_page_ok`) accumulates `U/S` **and `R/W` across
every level**, because on x86 both are ANDed down the walk: a PML4E with `U/S`
clear makes the whole 512 GiB region supervisor-only however the leaf is marked.
Testing only the leaf would report a kernel page as user-accessible — the same
hole the AArch64 AP test was added to close (`docs/archive/USER_COPY_FOLD.md` §7).

## 4. Both boot paths must register it — and only one did

`exec_runtime::init()` went into `kmain` (PVH) and not into `kmain_mb2`
(multiboot2). **QEMU and Firecracker enter via PVH; GRUB enters via
multiboot2**, so both VMM rigs were green and the bare-metal box died at the
first folded syscall with §1's panic. A boot-protocol-shaped failure wearing a
memory-shaped message.

Reproduced without a reboot on the box's own OVMF/GRUB rig (`/root/ovmf5.sh`),
which is the multiboot2 path under KVM: the pre-fix arm panicked at 10 s.

Fixed by making the console hook and the runtime registration **one shared
function** — `boot::install_shared_sinks`, called from both entries. This is the
second time the two `kmain`s have been merged at a step that drifted;
`boot::early_init` was the first, and it found the multiboot2 path had never run
seven of the PVH path's tests (`docs/archive/AKUMA_AMD64_BLOCKING.md`).
`set_print_hook` being duplicated in both entries was the invitation, and the
rule is now in `docs/runbooks/amd64-bare-metal-loop.md`: a change that touches
boot order gets an `ovmf5.sh` run, because the fast lane is PVH-only and
structurally cannot see this class.

### A pre-existing hang behind it

With the panic gone, the multiboot2 boot did not fail — it **hung**, never
printing a tally. `net::netpoll_spawn_selftest` bounds its wait with
`uptime_us()`, which is `lapic::ticks()`, and `boot::self_tests` calls
`lapic::stop_timer()` before that test runs. So when the netpoll daemon also
fails to lap, the deadline is a promise nothing keeps. Both conditions hold on
exactly one rig — OVMF/GRUB q35, whose e1000 this kernel does not drive, so
there is nothing to poll.

That is the *hang* version of the failure the loop's own comment was written to
prevent: on the multiboot2 path a failed suite used to withhold `init`, so a
slow daemon meant no sshd on a headless box. A yield cap (`MAX_YIELDS`) restores
the bound. It costs a healthy boot nothing — the lap condition breaks out in
microseconds — and turns an unbounded hang into a failed check, which is a line
in the log and a boot that carries on.

## Verification

| rig | entry | result |
|---|---|---|
| local QEMU/TCG, `SMP=4` | PVH | 425 passed, 0 failed |
| box OVMF/GRUB, `SMP=4` | multiboot2 | 415 passed, 1 failed — that rig's undrivable NIC (`netpoll laps 0`) |
| HP 500-502nj, bare metal | multiboot2 | **416 passed, 0 failed** (`netpoll laps 101` in 1408 yields) |

`ssh akuma "uname -a"` on the metal answers `Akuma akuma 0.1.0 de169eed-release
x86_64 GNU/Linux` — the folded glue arm, on the machine that could not boot
before. Host tests green (30 suites).

Two `uname` fields deliberately changed what they print: `release` and `version`
now come from glue's build identity rather than `banner::RELEASE`/`VERSION_DESC`.
That is the fold working — one answer, not two — and it is a gain: `uname -a`
now names the commit it is running. `banner::print()` keeps the local strings
for the boot banner, which is where a target's own name belongs.

## What this predicts for steps 4–6

Every defect above was invisible to the suite for the same reason, so the
remaining arms should be assumed to have the same blind spot. Two consequences:

- **A folded arm is not verified by the boot suite passing.** It is verified by
  a real ring-3 caller — busybox over ssh — exercising it.
- **Run the multiboot2 rig for anything that touches boot order**, not only the
  fast lane.

The next arms are the `FastPath::Leaf` set (`akuma_syscalls::fast_path`):
`getuid`/`geteuid`/`getgid`/`getegid` return a literal `0` in both kernels and
consult no `Process`. Deliberately *not* next: `getpid`/`gettid`/`getppid`/
`getpgid`/`getsid`/`getcwd`, which **are** identity and read a process table
this target does not populate — glue would answer confidently and wrongly. They
wait for C1 step 5 (`PROCS` folds into `akuma-exec`), which is also what
`futex_wake`'s `not_wired!` stub names.

## Batch 2 (same day): the leaf tier, and what the ring-3 check found

`getuid`/`getgid`/`geteuid`/`getegid` folded, with `setuid`/`setgid` alongside —
glue takes those with `capset`/`setres[ug]id`/`setgroups` under one stated
stance: "success" means *not implemented*, not "privileges dropped". No
divergence to pin; the answer is identical on both sides and only the number of
places it is written down changed.

**The tests are about the number hop, not the value.** Every one of these was
`=> 0` locally and is `=> 0` in glue, so a value check would pass against a
dispatcher that had lost the arms entirely. x86_64 102-108 — the whole
credential block — lands in asm-generic's timer/module block, where this build
answers one number for real: `nr::SETITIMER` is 103, and `sys_setitimer` reads
two `struct itimerval` pointers out of `args[1]`/`args[2]`. A `getuid` arriving
there hands it whatever its unset argument registers held. **A missed hop is a
different arm, not a missing one** — the `symlink`-was-`utimensat` shape again.

### `getgroups`, found by the ring-3 check rather than by reading

Verifying the batch the way this document says to — a real ring-3 caller —
`busybox id` on the metal printed `uid=0 gid=0` (the folded arms answering
correctly) and then `id: can't get groups`, exit 1.

`akuma-syscalls-glue` has had `proc::sys_getgroups` all along: no supplementary
groups exist, so the answer is the count `0`, and `size == 0` is the probe form
every caller actually uses. What was missing was the *vocabulary* —
`akuma-syscalls-abi` had no `Getgroups` row, so x86_64 115 decoded to nothing.
One row and one arm; `id` exits 0 on the metal now.

That row is also the pair that makes the two-number shape earn itself:
asm-generic 158 is `getgroups`, **x86_64 158 is `arch_prctl`**, and this kernel
answers both — one through `Syscall`, one from the x86-only legacy list, which
`dispatch_smoke_test` already asserts are disjoint.

### The netpoll self-test, twice reshaped

§4's yield cap was not the end of it. On the metal the same binary reported
`netpoll laps 101` on one boot and `laps 0` the next, with networking healthy
on both — so what varied was the *measurement*. The cause is the same stopped
clock: `boot::self_tests` was asking "does the netpoll daemon get scheduled"
under a condition the daemon never runs in. Fixed by giving the test its own
`start_timer`/`stop_timer` bracket, the one the SNTP sync three lines above
already had.

Then the backstop itself misfired. A plain `yields >= 200_000` fired at roughly
0.2 s on real hardware — well inside the 2 s budget — so it pre-empted the bound
it exists to protect. It is now a **stalled-clock detector**: it trips only when
`uptime_us()` has not moved *at all* across that many yields. If the clock is
advancing, the deadline governs and the branch is unreachable; if it is frozen,
no number of yields will ever reach the deadline and this is the only way out.
When it does trip it fails a check of its own, because "the clock was stopped"
and "the daemon is starved" are different facts and the tally should not
conflate them.

The general lesson, which is not about netpoll: **a timeout expressed in a clock
the caller can stop is not a timeout.** Two of the three bugs in this section
are that sentence.

#### And with the test finally honest, it reports a real failure — OPEN

Bare metal, `SMP=4`, the clock running and the full 2 s budget spent:
**410534 yields, `netpoll laps 0`**, no stalled-clock check. That is not the
measurement artefact any more; the daemon genuinely is not scheduled during the
suite window.

It is **not** a networking outage, and one grep settles that: `mem_watch_tick`
runs inside the daemon's own loop and writes a `mem: heap …` line to the
`dmesg` ring every 10 s. On the same boot, `ssh akuma "dmesg" | grep -c "mem:"`
went 1 → 6 over the following minute, one per period. **The daemon starts
lapping the moment the suite ends**, ssh answered throughout, and a TCP connect
to a closed port came back refused rather than hanging.

So the fault is bounded to "during `boot::self_tests`, on real hardware, the
netpoll daemon does not get picked" — and it is boot-thread-relative rather than
absolute, since 410534 yields from the boot task produced none of them. QEMU/TCG
does the opposite: 101 laps in 101 yields, the daemon running on *every* yield.

Not investigated further in this pass. It gates nothing — the suite starts
`init` regardless since A1 — and it wants the scheduler picker instrumented and
several reboot cycles, which is its own pass. What has changed is that it is now
a `[FAIL]` with a number beside it on every boot instead of a hang on one rig
and a coin-flip on another.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; C1 is trunk C.
- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — steps 1 and 2, the two
  syscall-number vocabularies and the `symlink`-was-`utimensat` bug.
- `docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md` — the gate this step stands on.
- `docs/archive/AKUMA_AMD64_BLOCKING.md` — A1/A2, and the first merge of the two
  boot paths.
- `docs/runbooks/amd64-bare-metal-loop.md` — the rigs, and why `ovmf5.sh` is the
  one that sees a multiboot2 bug.
