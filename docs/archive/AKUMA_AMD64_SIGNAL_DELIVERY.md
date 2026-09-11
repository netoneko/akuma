# amd64: signal delivery

**Date:** 2026-09-11
**Scope:** items 1 and 2 of `AKUMA_SELF_HOSTING_AMD64.md`'s YOU-ARE-HERE — a
signal that is pended now gets *looked at*, and `^C` on the serial console
raises `SIGINT` on the foreground process group instead of arriving as a byte.
**Status:** done and verified; the deferred parts are §8.

## 1. What was actually missing

Every *half* of signals except one already worked on this target.
`akuma-threading` held the per-thread pending set, the blocked mask, the
sigaltstack and the wakers. `akuma-syscalls-glue` had `rt_sigaction`,
`rt_sigprocmask`, `sigaltstack`, `kill`, `tkill`, `tgkill`, `rt_sigsuspend` and
`rt_sigtimedwait`. `akuma_exec::process::deliver_signal` pended on a whole
thread group and set the interrupt flag `should_interrupt_blocking_syscall`
reads. `write_to_process_stdin` held the ISIG branch that turns `^C` into a
`SIGINT` broadcast.

What no amd64 code did was **look at the pending set**. So:

- `kill(2)` was not dispatched at all, and `rt_sigaction` was a literal `=> 0`.
- Had they been, nothing would have run the handler — the pending bit would sit
  there for the life of the process.
- `deliver_signal` returns `true` whether or not it reached a thread, so
  `sys_kill` would have reported **success** either way.

On AArch64 the looking happens inside the EL0 sync handler against a
`UserTrapFrame` (`akuma_exceptions::try_deliver_signal`, ~350 lines welded to
that architecture's `sigcontext`). This is the x86_64 counterpart, against
`UserCtx`, in `amd64/src/signal.rs`. It is a **separate implementation on
purpose**: the register file, the frame layout, the return instruction and the
restorer convention are all different, and the only thing the two could share is
a dispatch shape neither would be shorter for.

## 2. The return path: one, not two

Redirecting a `sysret` needs a second exit from `syscall_entry`, for exactly the
reason `execve` needed one (`AKUMA_AMD64_EXECVE_RETURNS.md`): the ordinary path
takes `rip`/`rflags` from the `rcx`/`r11` the `syscall` instruction delivered,
off the kernel stack, and a signal changes both.

So `UserCtx` gains three fields and the assembly gains one label:

| field | offset | who writes it |
|---|---|---|
| `user_rflags` | 176 | `syscall_entry` (`mov [rax+176], r11`) |
| `sig_return` | 184 | `signal::deliver_pending`, `signal::sys_rt_sigreturn` |
| `sig_rax` | 192 | the same two |

`.Lsig_return` abandons the pushed frame, takes `rsp`/`rip`/`rflags`/`rax` and
all twelve saved registers out of the `UserCtx`, and `sysret`s.

**One path serves both directions, and that is the design rather than a
shortcut**: entering a handler and returning from one are the same operation —
install a register file and `sysret` into it — differing only in who filled the
file. `deliver_pending` fills it with the handler's ABI (`%rdi` = signo, and
with `SA_SIGINFO` `%rsi`/`%rdx` = the frame's `siginfo`/`ucontext`);
`sys_rt_sigreturn` fills it from the frame on the user stack.

The hand-indexed-offset assertion block grew from six entries to nine. It earns
itself the same way it did for `execve`: a reordered field compiles, boots,
passes the suite, and sends a signal handler to whatever moved into 32.

## 3. Three pinned divergences from Linux

1. **Delivery happens at a `syscall` return only** — not on the LAPIC tick's
   `iretq`, and not out of a `#PF`. A program that never syscalls never takes a
   signal. This is the same gap `thread::should_leave_now` already documents for
   `exit_group`, and closing one closes both.
2. **`uc_mcontext.fpstate` is `NULL` and no FP state is saved.** Linux always
   attaches an `xsave` area. Nothing in this tree's userspace dereferences it
   (musl does not; Go would), and this kernel is both writer and reader —
   `sys_rt_sigreturn` never looks at the field — so the pair is self-consistent.
3. **`SA_RESTORER` is required at delivery.** So is it on real Linux/x86_64,
   where `sigaction` returns `EINVAL` without one, because the architecture has
   no kernel trampoline page. Here the check is at *delivery* rather than at
   registration, because glue's `sys_rt_sigaction` is shared with AArch64 where
   `sa_restorer` does not exist. A missing restorer declines delivery, which
   falls through to the default action.

`rflags` is filtered on the way back (`sanitize_rflags`): `sysret` loads `%r11`
straight into `RFLAGS`, so an `rt_sigreturn` frame is a ring-3 program choosing
its own flags. `IF` is forced on — a ring-3 thread with interrupts masked cannot
be preempted, which on this target is a hang — and `IOPL`/`NT` are stripped.
`rip` is range-checked for the same class of reason: `sysretq` **faults in ring
0** on a non-canonical `%rcx`, so an unchecked frame is ring 3 choosing where
the kernel takes a `#GP`.

## 4. Termination goes through this target's own exit

A fatal `SIG_DFL` does **not** call `akuma_syscalls_glue::sys_exit_group`. It
sets `EXIT_STATUS` and `UserCtx::leave` — the same three lines the
`Syscall::ExitGroup` arm runs — so the task returns into `run_process`'s
epilogue, which is what drains sibling threads, closes the fd table, stamps the
`SPAWN` row and removes the channel. Glue's version does most but not all of
that and then parks the thread in a `yield_now` loop; a child killed through it
would never report a status to its parent's `sys_waitpid`.

That is also why `tkill`/`tgkill` are **local arms** rather than `to_glue`:
glue's `sys_tkill` decides fatality inline by calling `sys_exit_group`. Here
every signal is pended and every fatality decision belongs to
`deliver_pending`, which is the one place that knows how this kernel leaves ring
3. The dispositions the local arm does reproduce are glue's, because they are
POSIX: `SIG_IGN` drops, `SIGKILL` is unconditional, a blocked signal pends.

**The exit status travels in `%rax`, not in `EXIT_STATUS`.** `run_process` reads
it off `enter_user`'s return value and stamps it into the child's exit channel,
which is what `wait4` decodes — so `deliver_pending` must return `-(sig)` rather
than the interrupted syscall's own result. Getting that wrong is how rung 7 of
the probe first failed: a `SIGTERM` death reported as whatever the interrupted
`read` had answered.

## 5. Four bugs found on the way, all pre-existing

### 5a. `set_tid_address` returned a literal `1`

musl's `__init_tp` seeds `pthread_self()->tid` from this syscall's return, and
`raise(3)` is `block-all` / `tgkill(getpid(), pthread_self()->tid, sig)` /
`restore`. So **every self-signal in every program on this target addressed
thread slot 1**, which is not the caller. Folded to glue, which answers
`current_thread_id()` and records `clear_child_tid` on the way — the other half
this arm dropped.

### 5b. `gettid` answered the pid for a main thread

`crate::thread::current_tid()` fell back to `usermode::current_pid()` when the
task was not a `clone` child, so `gettid()` and the tid `clone` hands a child
came from **two different namespaces on one target**. The slot is the namespace
every per-thread array in `akuma-threading` is indexed by, which `clone_thread`'s
own comment says at length. Folded to glue; the fold deleted `current_tid` and
the `Thread::tid` field behind it, which held `task as u32` — the same value
glue answers. The two namespaces met only in the main-thread fallback, and that
is where the divergence was.

### 5c. `Process::thread_id` was `None` for everything `register_exec_process` built

`deliver_signal` collects its target tids from exactly that field, so `all_tids`
was empty and nothing was pended. The old note said leaving it `None` kept
`unregister_process`'s thread-termination arm out of this target's scheduler;
two facts retire that. Since A1 the task slot **is** the `akuma-threading`
thread id here (`current_thread_id` reads `X86ArchHooks::current_slot`, which is
`sched::current_task`), so there is no second numbering. And the field was never
consistently `None` anyway: a `fork` child and a `clone` thread both go through
the shared `spawn_child_thread_and_publish`, which has always written
`Some(tid)`. What was `None` was precisely the half that could not be
signalled — `init`, everything `sshd` spawns, and every `execve`d image.

### 5d. `interrupt_thread` writes the channel nobody reads — and the obvious fix is a worse bug

`is_current_interrupted` reads `Process::channel` **first** and only falls back
to the per-thread registry; `interrupt_thread` writes the registry alone. On
AArch64 those are the same `Arc` and the difference never shows. On amd64 they
are two objects — every process registers an *exit* channel under its task slot
(that is what makes `wait4` shared code), and a console-attached process also
carries the serial line's own channel in `Process::channel`. So `kill` sets
`interrupted` on the exit channel and `should_interrupt_blocking_syscall` reads
the console one.

**This was "fixed" by having `interrupt_thread` write both, and the fix was
reverted.** One line up the file from the field, `Process::inherit_from` does
`channel: parent.channel.clone()` — so a `Process::channel` is shared by an
entire **process tree**, and on amd64 with a console-attached `init` (which is
every rig: `init=/bin/sshd`) that is every process on the machine. A
per-process interrupt flag cannot live in an object the whole machine shares.

**So the gap stands**, and it is narrower than it looks: the per-thread `EINTR`
path (`current_thread_has_pending_interrupt`, reading the pending set) is
unaffected and is what every "a `kill` interrupts a blocking syscall" case
actually uses — rung 6 passes without the flag. Closing it properly means a
per-**thread** interrupt flag that is not a shared `Arc`, found without paying a
`get_channel` map lookup on every syscall.

**A false trail is recorded here on purpose**, because it cost the most time in
this whole piece and the shape of the mistake is reusable. A `sigprobe` rc=130
appeared, the sharing hazard above was a perfect-looking explanation, and
reverting `interrupt_thread` **did not fix it** — 3 runs of 3 still failed. The
sharing hazard is real by inspection and the revert stands on its own; it was
simply not this failure. What found the real one was refusing to stop at a
plausible cause: one `safe_print!` in the prologue's interrupted arm, naming the
pid and syscall number (`pid=77 tid=4 nr=173` — `getppid`, rung 11's), and then
an A/B with the tick delivery compiled out, which still failed and cleared the
other suspect. See §5f.

### 5e. `deliver_signal` signalled recycled thread slots

Found while chasing 5d, and kept. `Process::thread_id` is a *recorded* slot
number and slots are recycled, so a signal to a process whose thread has already
exited — a zombie, which `lookup_process_shared` finds perfectly well — names a
slot that may be running something else. `kill_process` and
`kill_process_with_signal` both guard this, at length and after being bitten;
`deliver_signal` did not. `sigprobe`'s `reap_with_signal` creates the window
deliberately: it re-sends every 50 ms while polling `waitpid`, and the send
after the child dies but before the reap is exactly it.

Guarded now with the same `slot_still_owned_by` the neighbours use, applied
after collection so the `for_each_process` callback — which runs IRQ-masked and
must not lock — stays as it was.

### 5f. A `kill` marked its target a zombie that had exited 130 — while it ran on

The one rung 11 found, and the one the false trail in §5d was hiding.

`akuma-syscalls-glue`'s dispatch prologue read `is_current_interrupted()` on
every syscall and, when set, stamped the caller `exited = true`,
`exit_code = 130`, `state = Zombie(130)` — **marking a process dead while it is
running** — before returning `EINTR`. The theory was that the flag means Ctrl-C
and Ctrl-C means death. Neither half holds. `deliver_signal` raises the flag for
**every** signal, not only `SIGINT`, so `kill(getpid(), SIGUSR1)` stamped the
caller; and the killing is the *signal's* job, and has been since 2026-08-24
(`CTRL_C_SIGINT_DELIVERY.md`) — the same `kill_process_group` that raises the
flag pends `SIGINT`, whose default action terminates at the next return to
userspace. The stamp was belt-and-braces from before delivery worked, and what
it did instead was overwrite the truth with a guess.

Measured: `sigprobe` printed every remaining rung and `_exit(0)`, and `ssh`
reported **130**. Deterministic, 3 runs of 3. The prologue returns `EINTR` and
nothing else now — which is the flag's actual job.

Together with 5e, that is **two** shared-crate behaviour changes;
`akuma-syscalls-abi`'s seven new rows are additive.

## 6. The console: `^C` becomes `SIGINT`

`console::pump_once` wrote keystrokes straight into the channel's stdin FIFO, so
`^C` reached the program as a `0x03` byte. It now calls
`akuma_exec::process::write_to_process_stdin` — the tty's front door on both
kernels — which strips the INTR character and raises `SIGINT` on
`TerminalState::foreground_pgid` when `ISIG` is set.

That needs a pid, which `console::ATTACHED_PID` supplies: set by
`register_exec_process` for a process whose fd 0 is a `FileDescriptor::Stdin`,
i.e. `init` on the serial line. *Which* console-attached pid it holds does not
matter — every one of them carries the same channel and the same
`TerminalState` `Arc` — and in practice it is `init`, registered first and
outliving what it spawns. The fallback for pid 0 (before `init`) and for a
reaped pid is the old direct write, which has no ISIG handling and correctly so:
with nothing attached there is no foreground group to signal.

`foreground_pgid` needs no new plumbing. It defaults to 1, `init` is pid 1 with
pgid 1, `fork` propagates `pgid`, and `kill_process_group` deliberately excludes
the group **leader** — so `^C` reaches the shell's children and not the shell.
A job-control shell that calls `TIOCSPGRP` moves it through glue's `term.rs` arm.

## 7. One trap, and it cost a boot

The first version of the local `sys_tkill` called
`akuma_exec::process::interrupt_thread` alongside the pend. That flag is the
**Ctrl-C sledgehammer, not a signal**: glue's dispatch prologue reads it on
*every* syscall, marks the process a `Zombie(130)` and returns `EINTR` —
`SA_RESTART`-blind and disposition-blind by design, because Ctrl-C's job is to
end the foreground job.

It made `raise(3)` fail in the most confusing possible way. musl's `raise` is
block-all / `tgkill` / restore, and the `EINTR` landed on the **restore**:

```
[SIGDBG] procmask how=0 ...  0x0->0xfffffffc7ffbfeff     <- block-all
[SIGDBG] tkill tid=5 sig=10 me=5
[SIGDBG] epilogue tid=5 pend=0x200 mask=0xfffffffc7ffbfeff
[SIGDBG] procmask how=2 ... r=0xfffffffffffffffc  0xfff...->0xfff...   <- EINTR
```

so SIGUSR1 stayed blocked forever and the handler never ran. Glue's own
`sys_tkill` does not set the flag either; the per-thread `EINTR` decision
belongs to `current_thread_has_pending_interrupt`, which reads the pending set
and honours `SA_RESTART`.

## 8. What is still open

- **Delivery on the timer tick's `iretq`** — §3 item 1. A compute-bound program
  with no syscalls is unreachable by `^C`.

  The other half of that item — **delivery out of a `#PF`/`#GP`** — is **done**,
  later the same day: `AKUMA_AMD64_FAULT_SIGNALS.md`. A fault is a catchable
  `SIGSEGV` now, `amd64_mem_trials.py`'s `EXPECTED_FAIL` table is empty, and the
  memory probes are 10/10.
- **`akuma-net`'s `is_current_interrupted` hook is still `false`**, so a socket
  read is not interruptible even though glue's blocking arms are. The module
  header's reason used to be "no signals"; it is narrower now and the hook is
  read from inside `smoltcp`'s poll loop, so wiring it is a measurement, not a
  one-liner.
- **`rt_sigsuspend` / `rt_sigtimedwait` / `pause` are not dispatched.** They
  exist in glue and would fold; `rt_sigsuspend` in particular interacts with the
  epilogue (it arms a restore-mask that AArch64's frame builder consumes), so it
  wants its own probe rung before it lands.
- ~~**`getpid` still answers a literal `1`.**~~ **Fixed later the same day** —
  folded to glue with `getppid`, and `sigprobe` gained rung 11 for it.

  **The reason first written here was wrong and is worth correcting rather than
  deleting**, because it is the plausible-sounding one: musl spells *both*
  `raise(3)` and `pthread_kill(3)` with **`tkill`**, which takes a thread id and
  never consults `getpid`. (glibc spells them with `tgkill(getpid(), …)` and
  *would* have been broken by the constant — the day something
  dynamically-linked and non-musl runs here.) What the literal 1 actually broke
  is `kill(getpid(), sig)`, which `sys_kill` refuses outright (`pid <= 1` →
  `EPERM`), and every identity use of a pid: `$$` was 1 in every shell on the
  machine at once — measured on the metal — so a pid-named temp file, lock or
  log line collided with everything else, and no `ps` row could be correlated
  with it.

  `getpgid`/`getsid` still answer `1`, deliberately: `foreground_pgid` defaults
  to 1 and `kill_process_group` excludes the leader, which is exactly what makes
  `^C` reach `init`'s children and not `init`. A shell reading a real `getpgid`
  and `TIOCSPGRP`ing it would move that target, and nothing here has been tested
  against job control.
- **An `ssh` session's `^C`** still does not raise `SIGINT`: the child's stdin is
  a pipe with no channel to run a line discipline on. That is item 3 of the
  walk's YOU-ARE-HERE, unchanged.
- **`kill(2)` is more than a signal in this tree, on both kernels.**
  `deliver_signal` sets the Ctrl-C `interrupted` flag alongside the pend, and
  glue's dispatch prologue reads that flag on *every* syscall: it marks the
  process `Zombie(130)` and returns `EINTR`. So `kill(pid, SIGUSR1)` — a signal
  whose default action is to be *ignored* — still makes the target's next
  syscall fail, and marks it a zombie while it runs on. Pre-existing and shared
  with AArch64, so not changed here; found because the probe's first version
  raced its `kill` against the child's own `sigaction` and got `EINTR` back
  **from the `sigaction`** (exit `60 + EINTR`), 22 runs in 25 at `SMP=4` under
  TCG. The probe handshakes now (§9); the divergence stands.

  Worth a measurement before touching it: the flag is what makes Ctrl-C able to
  end a job blocked in a syscall with a `SA_RESTART` handler installed, which is
  the case `current_thread_has_pending_interrupt` deliberately declines.

## 9. Verification

`userspace/forktest/c_stress/sigprobe.c` — eight rungs when this piece landed,
twelve by the end of the day (`AKUMA_AMD64_FAULT_SIGNALS.md` adds 9-12) — wired
into
`scripts/utils/amd64_ring3_check.py` beside `grandfork`. It is a **musl
program's** view — `sigaction` recording a handler, a frame the handler's `ret`
returns through, `rt_sigreturn` restoring a register file the kernel did not
author — which the boot suite structurally cannot check: it runs inside the
kernel, on init's task. Rungs 6 and 7 re-send the signal while polling
`waitpid`, so a kernel with no delivery reports a rung number instead of hanging
the run.

| rung | what it isolates |
|---|---|
| 1 raise | a handler runs at all, and execution resumes after it |
| 2 resume | the interrupted code's locals survived the excursion |
| 3 siginfo | `SA_SIGINFO`'s second and third arguments are real |
| 4 mask | a blocked signal pends and fires on unblock, in that order |
| 5 nested | a second signal during a handler reaches a second handler |
| 6 eintr | a signal breaks a blocking `read` (no `SA_RESTART`) |
| 7 fatal | `SIG_DFL` `SIGTERM` to a child is `WIFSIGNALED`, not ignored |
| 8 abort | `abort()` reaches `SIGABRT` through musl's block/`tkill`/unblock |
| 9-12 | added later the same day: a catchable `SIGSEGV` two ways, `kill(getpid())`, and delivery to a pure compute loop — `AKUMA_AMD64_FAULT_SIGNALS.md` §6 and §7 |

Statically linked musl, so **the same binary was run on real Linux** (the
trashcan's Ubuntu personality) and all eight rungs pass there — the A/B that
says the probe is right before the kernel is judged by it
(`LINUX_AB_PROBE_TECHNIQUE.md`).

**It took two races to get right, and Linux only showed the first.** On the
first Linux run rung 6's single `kill` after a fixed sleep raced the child into
its blocking `read`, so the read returned EOF instead of `EINTR`; that is what
`reap_with_signal` exists for. The second race Linux never lost: the `kill` also
raced the child's *`sigaction`*, and on Akuma at `SMP=4` it won 22 times in 25 —
the signal arrived before the handler was armed, and the Ctrl-C flag it also
sets made the racing `sigaction` itself return `EINTR`. A `sleep` is not a
handshake; the child writes a ready byte now, and the retry loop still covers
the last gap (between that byte and entering the `read`). A probe that passes on
Linux is not yet a probe that is right — it is a probe whose races Linux happens
to win.

The boot suite gets `signal::smoke_test` — 20 checks for exactly the code whose
failure would be *silent* in the probe too: the `sigcontext`/`ucontext`/frame
sizes and offsets, the twelve-register shuffle **both ways** with distinct
values (a transposition is invisible to the compiler and a probe only notices it
with live data in both registers), the `rflags` filter, the `sysret` `rip`
guard, and delivery declining rather than jumping into nothing.

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 641/0 | **661/0** (+20, `signal:`) |
| Firecracker/KVM `SMP=4` | 619/0 | **639/0** (+20) |
| bare metal `SMP=4` | 634/3 | **654/3** (+20; the same three `xhci:`) |
| `amd64_ring3_check --smp 1 -n 40` | grandfork only | **OK**, + sigprobe 8/8 |
| `amd64_ring3_check --smp 4` | grandfork only | **OK**, + sigprobe 8/8 |
| `sigprobe` x25 in one `SMP=4` guest | — | **25/25** (22 failed before the handshake) |
| `sigprobe` on real Linux x86_64, x10 | — | **10/10** |
| `^C` on the serial console, `busybox sh` init | byte `0x03` | **`cat` dies, shell survives** |
| `kill` / `kill -9` from ash, **on the metal** | `ENOSYS` | **143 / 137** |
| host tests | 1375/0 | **1375/0** |
| clippy, amd64 ±`no-tests` | clean | **clean** |
| AArch64 boot suite (HVF, `MEMORY=2048M`) | 307/0 | **307/0** |

**The bare-metal `3` is the USB disk, not this change**, and it is the same
three failures and the same count the previous session measured on *two*
different kernels (`AKUMA_AMD64_EXECVE_RETURNS.md`): `xhci: read the MBR at
LBA 0`, `xhci: read the sda1 ext2 superblock`, `xhci: WRITE(10) to a scratch
LBA in sda2`, with `fs: ext2 mounted on module` — the RAM fallback. The drive
has stalled and needs a power cycle, which is a hand on the machine. The
arithmetic is exact: 634 + 20 = 654, so nothing else moved.

Because the persistent root is not mounted, `sigprobe` could not be staged onto
the metal (there is no `base64` in the RAM image's busybox and `chmod` is
`ENOSYS` there). What ran instead is the end-to-end path through `ash`:

```
sh -c 'sleep 30 & p=$!; sleep 1; kill    $p; wait $p; echo rc=$?'   -> Terminated / rc=143
sh -c 'sleep 30 & p=$!; sleep 1; kill -9 $p; wait $p; echo rc9=$?'  -> rc9=137
```

`kill(2)` → `deliver_signal` → pend → default action → the parent's `wait`
decoding `WIFSIGNALED`, on real silicon with real musl. Before this change
`kill` was not dispatched at all.

**AArch64 is not untouched this time** — `akuma-exec`'s `interrupt_thread` is
the one shared behaviour change (§5d) and `akuma-syscalls-abi` gained seven
additive rows — so the AArch64 kernel was built and booted rather than compared
by section: 307 passed, 0 failed, which is the same tally as the A1/A2 baseline
on the same accelerator.

The console check is the delayed-feed rig `AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md`
§6 describes — QEMU's serial is `mon:stdio`, so a timed `python3 -c` feed is
real keyboard input:

```
/ # cat
hello
hello
[signal] pid=47 killed by signal 2 (default action)

/ # echo ALIVE-AFTER-CTRLC
ALIVE-AFTER-CTRLC
```

`hello` twice is the line discipline's echo plus `cat`'s output; the third line
is the default action; and the shell running the next command is what says the
broadcast excluded the group leader.

## Background

`AKUMA_SELF_HOSTING_AMD64.md` (the walk; YOU-ARE-HERE items 1 and 2),
`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` (the producer this builds the signal
route on top of, and §7's two prerequisites — both confirmed real, 5c and 5d
above), `AKUMA_AMD64_EXECVE_RETURNS.md` (the first second return path),
`CTRL_C_SIGINT_DELIVERY.md` (the shared ISIG route),
`SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md` (why the mask is per-thread),
`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md` (the delivered-set half of
`current_thread_has_pending_interrupt`).
