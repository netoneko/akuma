# amd64: a fault becomes a catchable `SIGSEGV`

**Date:** 2026-09-11
**Scope:** all of `AKUMA_AMD64_SIGNAL_DELIVERY.md` §8 item 1 — delivery out of a
`#PF`/`#GP` (§1-§6) and, with it, delivery on the **LAPIC tick** (§7). A signal
is now looked at from all three places a program can be interrupted, so there is
no longer a shape of program a signal cannot reach.
**Status:** done and verified. **amd64's `c_stress` memory probes are 10/10**,
and `amd64_mem_trials.py`'s `EXPECTED_FAIL` table is empty for the first time.

## 1. Two bugs, one line apart

Signals arrived on this target earlier the same day, but only at a `syscall`
return. A CPU fault went somewhere else entirely: `idt.rs`'s dispatchers fell
through to `user_fault`, which printed and called
`usermode::kill_current_from_fault(128 + 11)`. Two separate things were wrong
with that, and the second one hides behind the first.

### 1a. The status was not a signalled status

`128 + SIGSEGV` = 139 is what a **shell** prints, computed from `WTERMSIG` *by
the shell*. What `waitpid` reports is a *signalled* status, and this tree's
encoding for that is a **negative** exit code: `encode_wait_status` turns `-11`
into `WIFSIGNALED`/`WTERMSIG == 11`, and turns `139` into `WIFEXITED` with code
139.

So every segfault on this target was reported to its parent as a **clean exit**.
That is not cosmetic — `waitpid`-based supervision cannot tell a crash from a
program that chose to exit 139 — and it is why
`userspace/forktest/c_stress/eager_mprotect_probe.c`, whose entire job is to
assert that an `mprotect` downgrade produces a `SIGSEGV`, could never pass.

One constant. `SIGSEGV_STATUS = -(SIGSEGV as i64) as u64`.

### 1b. There was no handler to run

The bigger one. A program that installs a `SIGSEGV` handler — every
`sigsetjmp`-guarded probe, Rust's stack-overflow reporter, Go's `sigpanic`, any
JIT with a guard page — got killed anyway. `mprotectlb` is exactly that shape,
and it was the other `EXPECTED_FAIL` row.

## 2. What it took: the stub had to save more

`signal::deliver_fault_signal` builds the same `rt_sigframe`
`deliver_pending` does, from the interrupted register file. On a syscall that
file is in `UserCtx`, because `syscall_entry` put it there. On a fault it is
wherever the exception stub left it — and the stub saved **ten** registers: the
caller-saved nine plus `rbp`.

Ten is exactly right for a *serviced* fault. The dispatcher is `extern "C"`, so
it preserves `rbx`/`r12`-`r15` itself, and the interrupted instruction
re-executes with everything intact.

**Delivering a signal needs to read them, not merely preserve them.** A handler
is handed `uc_mcontext` — the interrupted register file — and `rt_sigreturn`
puts that file back, so a register the kernel never wrote down is one a handler
returns into garbage. And *preserved-in-the-register* is not
*readable-from-Rust*: by the time the dispatcher decides to deliver, its own
Rust frames have used `rbx` and `r12`-`r15` for their own purposes.

So the stub now pushes all fifteen, as `TrapRegs`, passed as the dispatcher's
second argument. The sixteenth push is padding and is load-bearing: fifteen is
120 bytes, and `rsp` must be 16-aligned at the `call`. Pushing the pad *first*
rather than last is what puts the block at `[rsp + 0]`, so the second argument
is a plain `lea rsi, [rsp]` and the `PageFaultFrame` moves from `[rsp + 80]` to
`[rsp + 128]`.

Cost: six extra pushes and pops on every `#PF` — including demand paging, this
target's hottest exception — which is noise beside a page fault's own work.

## 3. The redirect is the frame, not a flag

The syscall path needed a second `sysret` exit (`.Lsig_return`) because the
ordinary one takes `rip`/`rflags` off the kernel stack. The fault path needs
nothing of the kind: **rewriting the pushed frame in place *is* the redirect**,
and the stub's existing `iretq` does the rest. `page_fault_dispatch` already
rewrote `pf.frame.rip` for the user-copy fixup; this writes `rip`, `rsp`,
`rflags` and three registers instead of one field.

Three further differences from `deliver_pending`, each of them a reason the two
are separate functions rather than one with a flag:

- **Only three registers are written back.** Linux's `setup_rt_frame` sets
  `di`/`si`/`dx` (and `ip`/`sp`) and leaves the rest of the file alone, so a
  handler that looks at `%rbx` sees what the faulting instruction had. The
  syscall path zeroes more because a `syscall` has already clobbered `rcx`/`r11`
  and the ABI makes the argument registers dead.
- **`rflags` is forced, not filtered.** `iretq` restores what the frame says, so
  `IF` set is not optional — a ring-3 thread with interrupts masked stops being
  preemptible — and `DF` clear is what the ABI promises the C function about to
  run. `cs`/`ss` are left alone; they are already the ring-3 selectors.
- **`si_code` and `si_addr` are real.** `SEGV_MAPERR` for an address with no
  translation, `SEGV_ACCERR` for one whose translation refuses the access —
  which is exactly the present bit, and exactly what an `mprotect` downgrade
  produces. A `#GP` reports `SI_KERNEL` and address 0, because the CPU rejected
  the operand before translation and there is no faulting address to report;
  Linux says the same.

## 4. Where it sits in the dispatcher, and why

After every servicing arm and after the user-copy fixup. That order is the only
one that works: a demand-paged page or a CoW break is not a fault the program
should hear about, and a fault inside `copy_to_user` belongs to the **kernel's**
access, not to ring 3.

Under the BKL, for the reason the servicing arms above it state: writing a
440-byte frame to the user stack can itself demand-page or break a CoW page, and
a CoW break broadcasts a shootdown IPI whose acknowledgement wait assumes every
sender holds the lock. The nested `#PF` would take it anyway — the dispatcher is
reentrant by owner core — so this is belt and braces, and the belt is cheap.
`kill_current_from_fault` takes it on the other side of the same decision.

## 5. What it refuses, and why each refusal is still correct

No process, no `UserFn` disposition, the signal **blocked**, or a frame that
cannot be written. Linux force-unblocks a synchronous fault signal and then
applies the default action if the handler cannot run; declining here reaches the
same place by the caller's route, which is `user_fault` → kill. Declining is
always safe: a killed process is what happened before this function existed.

The blocked case deserves its own sentence. A blocked synchronous fault cannot
be *deferred* — the faulting instruction would simply re-execute and fault
again, forever. Killing is the only terminating answer.

**Recursion is bounded at one level.** If the frame write faults (a stack that
has run out, most plausibly), the nested `#PF` services it if it can and
otherwise takes the same path, finds `build_frame` failing or the address
outside every region, and kills the process — which ends the recursion. A
`sigaltstack` is the proper answer for stack-overflow reporting and is honoured
when the action asks for it (`SA_ONSTACK`), which is what a runtime that cares
already does.

## 6. Verification

Two new rungs in `userspace/forktest/c_stress/sigprobe.c`, and they test
different halves:

| rung | what it isolates |
|---|---|
| 9 `segv` | a fault becomes a catchable `SIGSEGV`, escaped with `siglongjmp` — the `mprotectlb` shape, which never returns through `rt_sigreturn`. Checks `si_addr` and `si_code == SEGV_ACCERR` |
| 10 `segvret` | the handler **repairs** the mapping (`mprotect` from inside the handler) and **returns**, so the faulting store re-executes and succeeds — the whole register-file round trip through `rt_sigreturn`, which rung 9 never exercises |

Rung 10 carries a `volatile long witness` the compiler is free to keep in a
callee-saved register across the faulting store: the store resuming proves
`rip`/`rsp` came back, and the witness proves the rest of the file did. Its
handler is bounded at four entries — a kernel whose `mprotect` does not take
effect, or whose `rt_sigreturn` restores the wrong `rip`, would fault there
forever, and a probe that hangs has no rung number.

Both pass on **real Linux** first (`LINUX_AB_PROBE_TECHNIQUE.md`).

The boot suite gets four more checks, all of them about the thing that is
*silent* when wrong: `TrapRegs` is the **third** place the register order is
spelled — `syscall_entry`'s store order and `to_sigcontext` are the other two —
and the only one whose source is assembly in another file. `Regs::from_trap` is
checked field by field against a `TrapRegs` with fifteen distinct values, so a
transposition cannot cancel out.

The gates in the table below cover §1-§6 and §7 together, with the `sigprobe`
rungs at 12.

| gate | before | after |
|---|---|---|
| `c_stress` memory probes, amd64 | 8/10, two `EXPECTED_FAIL` | **10/10, `EXPECTED_FAIL` empty** |
| `mprotectlb` | never ran (no handler) | **0 divergence(s) from Linux** |
| `eager_mprotect_probe` | `WIFSIGNALED` never true | **both phases PASS** |
| QEMU/TCG `SMP=4` | 661/0 | **665/0** (+4) |
| Firecracker/KVM `SMP=4` | 639/0 | **643/0** (+4) |
| bare metal `SMP=4` | 654/3 | **658/3** (+4; the same three `xhci:`), `$$` = a real pid, `kill -USR1 $$` survives |
| `amd64_ring3_check --smp 1 -n 20` | 8 rungs | **OK**, 12 rungs |
| `sigprobe` on real Linux x86_64 | 8/8 | **12/12** |
| the §5f repro (10 ssh sessions, then the probe) | 3/3 `rc=130` | **3/3 `rc=0`** |
| `^C` on the serial console, `busybox sh` init | works | **works** (re-checked after §5f) |
| host tests | 1375/0 | **1375/0** |
| AArch64 boot suite | 307/0 | **307/0** |
| clippy, amd64 ±`no-tests` and aarch64 | clean | **clean** |

The bare-metal `3` is the same stalled USB disk the previous piece measured,
unchanged and unrelated: `xhci: read the MBR at LBA 0`, `xhci: read the sda1
ext2 superblock`, `xhci: WRITE(10) to a scratch LBA in sda2`, with `fs: ext2
mounted on module` — the RAM fallback. 654 + 4 = 658 exactly, so nothing else
moved. The drive needs a power cycle, which is a hand on the machine.

**Two shared crates changed**, both recorded in
`AKUMA_AMD64_SIGNAL_DELIVERY.md` §5e and §5f — `deliver_signal`'s stale-slot
guard and the prologue that stopped stamping a live process as a zombie. So the
AArch64 kernel was booted rather than argued about: **307/0**, its baseline.

## 7. The third delivery point: the LAPIC tick

Done in the same session, because it is the same shape and the same stub
treatment, and because it is what makes "a signal reaches this program"
unconditional rather than "…if it syscalls or faults".

`timer_dispatch` now takes the `TrapRegs` block too, and calls
`signal::deliver_pending_on_tick` for a tick that interrupted **ring 3**, before
the preemption decision. Four things are worth writing down:

- **The alignment arithmetic is the other one.** The exception stubs pad,
  because the CPU's error code makes their entry `rsp` 16-aligned and fifteen
  pushes is `≡ 8 (mod 16)`. The timer has no error code, so its entry `rsp` is
  already `≡ 8`, and the same fifteen pushes cancel it exactly — **no padding
  push, and adding one would break it.**
- **`from_user` is the gate, and it is load-bearing rather than an
  optimisation.** The interrupted code is then provably not holding the BKL, so
  taking it around the frame write cannot deadlock against the very code it
  interrupted. (A ring-0 tick has no register file worth redirecting either.)
- **Before `preempt_if_needed`.** The switch saves and restores this kernel
  stack and the `iretq` at the end of the stub is still this task's, so the
  redirect is in place whether or not the tick also takes the task off the CPU.
- **A fatal default has no syscall to hand a status to**, so it leaves through
  `kill_current_from_fault` — the same unwind a `SIGSEGV` kill takes — and
  `deliver_pending_on_tick` therefore **does not kill**: it returns
  `TickOutcome::Fatal(sig)` and the dispatcher kills *after* releasing the BKL.
  Killing from inside would have left the lock one level deep for the rest of
  the boot (`kill_current_from_fault` takes it again and never returns, and the
  task unwinds into `run_process`, which expects to hold it exactly once). Found
  by reading the finished code rather than by running it — the path needs a
  fatal-default signal pending on a compute-bound ring-3 task at a tick, which
  no gate here produces.

The decision itself is now one function, `next_delivery`, shared by all three
entry points; they differ only in where the register file comes from and how the
process leaves ring 3 if the answer is fatal. That factoring is not tidiness:
two copies of a signal-mask update is exactly the shape that lets one path forget
`sa_mask` and re-enter a handler that asked not to be.

Gate: `sigprobe` rung 12, a child spinning in a **pure compute loop** — a
`volatile` accumulate and a `volatile` flag read, no syscalls, no faults — that a
kernel with only the other two delivery points cannot reach.

## Background

`AKUMA_AMD64_SIGNAL_DELIVERY.md` (the syscall-return half, the same day, and §8
item 1 is this),
`AKUMA_AMD64_MEMORY_CLOSEOUT.md` (the six memory gaps, which left these two as
"the ceiling"),
`AKUMA_AMD64_MMAP_REGIONS.md` (the region table `mprotect` splits),
`AKUMA_AMD64_COW.md` (why the CoW arm sits between demand paging and this).
