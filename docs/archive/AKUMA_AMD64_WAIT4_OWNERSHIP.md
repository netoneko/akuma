# amd64: `wait4` answered for every process, not the caller's children

**Date:** 2026-09-08
**Status:** fixed. Five rigs green, verified on the metal with the exact command
that had required a power cycle.
**Found by:** the ring-3 workload check while landing 5b slice 1
(`docs/archive/AKUMA_AMD64_STEP5B_SLICE1_REGISTRATION.md`), filed the same day
as issue 5 in `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, and **pre-existing** —
`057ed0d3` reproduced it identically, so neither 5b nor the `cpuid` repair
caused it.

---

## The symptom, and why it looked like something else

```
$ ssh akuma '( ls /bin; ls /bin ) | wc -l'
<hangs forever>
$ ssh akuma 'echo still-alive'
still-alive
<prints, then hangs forever>
```

Every session after the first hang printed its output in full and then never
tore down. On the bare-metal box the machine became unreachable and needed a
power cycle. That shape — output arrives, teardown never happens — is exactly
the (closed) issue 3, which had been diagnosed as a lost pipe wake or a spinning
`wait4`, so the obvious reading was "issue 3 is back".

It was not. The ladder says so:

```
echo hi                                       rc=0
ls /bin | wc -l                               rc=0    # plain pipe, 2 processes
( echo a ); echo done                         rc=0    # subshell, builtin only
( ls /bin >/dev/null ); echo done             rc=0    # subshell, ONE exec
( ls /bin >/dev/null; true ); echo done       HANGS   # subshell, exec NOT last
( ls /bin >/dev/null; ls /bin >/dev/null )    HANGS   # subshell, TWO execs
```

`( ls )` and `( ls; true )` differ by nothing a kernel should care about — until
you remember that **ash execs the last command of a subshell in place** and
forks for anything earlier. `( true; ls )` passes and `( ls; true )` hangs,
which nails it: the hang needs a forked process that *waits*.

## What it actually was

`STRACE=1` on the console shell (no `sshd` in the picture — the repro is
`INIT=/bin/busybox INITARGS=sh` with the command on stdin) ends like this:

```
[sc>] cpu=3 task=7 nr=231 a1=0x0        # the `ls` grandchild calls exit_group
[sc]  cpu=3 task=7 nr=231 -> 0x0
[sc]  cpu=3 task=5 nr=61  -> 0x20       # the subshell's wait4 returns pid 32
[sc>] cpu=3 task=5 nr=61  a1=0xffffffffffffffff   # wait4(-1) again — never returns
```

`sys_waitpid` scanned the **global** spawn table with no parent filter:

```rust
let exists = table.iter().any(|e| e.as_ref().is_some_and(|s| any || s.pid == want));
if !exists { return errno::ESRCH; }
```

Every live process on this target has a row in that table, not just the caller's
descendants. So for `wait4(-1)` the question being answered was *"does any
process exist?"* rather than *"do I have any children?"*. The subshell had just
reaped its only child; its **own** row — owned by the shell above it — was still
there, so `exists` was true, no row had `exit` set, and `sys_waitpid` returned
0 = "a child exists, none has exited". The `Wait4` arm parks on 0. The process
was waiting for itself, and nothing could ever wake it.

The cascade follows: the shell above waits on the subshell, `sshd`'s session
thread waits on the shell, and the session never closes. Every later session
still *ran* its command — sshd's accept path was fine — and then joined the same
queue.

Two things had to be true for this to survive as long as it did:

- **The wait has to happen in a forked process.** Only a process that is itself
  a table row can see itself, so init never trips it. That is why the boot suite
  never caught it and why `INIT=/probes/grandfork` running a plain fork/wait
  ladder passes on the *broken* kernel.
- **The shell has to wait twice.** One `wait4` finds the real child and reaps
  it; the second one is the one that hangs. `( ls )` never waits at all.

## The fix

Two halves, and each is independently necessary (proved below):

1. **Filter by parent.** `Spawn` already carried `ppid`, set from
   `current_pid()` by both of its two constructors (`sys_fork`, `sys_spawn`) and
   by nothing else — so it was safe to start reading it for a *refusal*, which
   is the check `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` exists to
   demand. Every writer was enumerated before the field was read this way.
2. **Return `ECHILD`, not `ESRCH`.** POSIX gives "you have no such child" its
   own errno and a shell tests for exactly it. `ESRCH` happened to end ash's
   loop, but anything that checks — the `wait` builtin, `system()`, make's
   jobserver — reads a wrong answer from it.

### The half nobody asked for: orphan reparenting

A `ppid` filter creates a leak the global scan did not have. Before it, any
process could reap any row, so a child whose parent died was collected by
accident. After it, an orphan has nobody who may reap it and its row and pipes
sit there until the slots run out.

So `spawn_record_exit` — the single site where an exit becomes visible — now
reparents the dying process's children onto pid 1, the way Linux does. Init is
not itself a table row (`current_pid()` answers 1 below `SPAWN_SLOT_BASE`), and
on this target init is the console shell or `sshd`, both of which reap.

**A bug fix that makes a lookup stricter should be read as two changes**: what
the strictness now refuses, and what used to be collected only *because* it was
loose.

## Verification

### The regression pins, and what each actually pins

`wait4_ownership_test` (4 boot checks, registered after `spawn_test`) pins the
errno half hard. It **cannot** pin the ownership half, and the reason is the bug
itself: the suite runs as init, and init has no row above it to trip on. That is
stated in the test rather than left for the next person to discover.

The ownership half is pinned by **rung 5 of `userspace/forktest/c_stress/
grandfork.c`**, which does the wait inside a forked child. The probe announces
every step through `write(2)` before running it, because the failure mode is a
hang: there is no exit status to read and a buffered line is a line you never
see, so the last thing printed names the operation that did not return.

Both were A/B'd against deliberately broken builds rather than assumed:

| build | boot test | probe rung 5 |
|---|---|---|
| fixed | 4/4 pass | pass |
| no filter, no ECHILD (`057ed0d3` shape) | 2/4 **fail** | **fail** |
| filter kept, `ESRCH` restored | — | **fail** |
| `ECHILD` kept, filter removed | — | **fail** |

The last two rows are the point: each half of the fix is independently
necessary, and a probe that passed with either one missing would pin nothing.

`grandfork` is statically linked musl, so the same binary was run on **real
x86_64 Linux** (the box's Ubuntu side): all five rungs pass there, which is what
says the probe describes POSIX rather than describing Akuma.

To run it: build with `x86_64-linux-musl-gcc -O1 -static`, write it into an ext2
image with `debugfs` the way `amd64_mem_trials.py::inject_local` does, and boot
`INIT=/probes/grandfork`. Running it as `init` is deliberate — no `sshd`, no
shell, so a hang cannot be confused with a session-teardown problem, which is
the confusion that made this bug look like issue 3 for half a day.

### Rigs

| rig | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 516 / 0 | **520 / 0** |
| QEMU/TCG `SMP=1` | 507 / 0 | **511 / 0** |
| Firecracker (the box, KVM) | 503 / 0 | **507 / 0** |
| OVMF/GRUB (the box, KVM) | 507 / 0 | **511 / 0** |
| bare metal `SMP=4` | 507 / 0 | **511 / 0** |
| host tests | 1360 / 0 | **1360 / 0** |

Exactly +4 on every rig: the new checks, nothing else moved. amd64 clippy clean,
`cargo clippy --release` (the AArch64 kernel) clean.

One measurement trap met on the way: running the local and remote arms of
`amd64_trials.py` **together** gave `494 passed, 1 failed` — a *lower* check
count than the 520 the same arm gives alone, plus a timing-sensitive mmap check
failing. Two VM workloads on one laptop starve each other. A tally that drops
should be read as "this run did not complete" before it is read as a
regression; the count moving in the wrong direction is the tell.

On the metal, the command that had cost a power cycle, plus 40 ssh sessions each
running it:

```
( ls /bin; ls /bin ) | wc -l          -> 34
40/40 sessions, free used 1051212 -> 1049205, free unmoved
ps | wc -l -> 5                       # no orphan or zombie accumulation
```

### A harness trap worth knowing

`amd64_mem_trials.py`'s **"firecracker" arm is remote** — it runs on the box
through `hpbox.ubuntu`. With the box booted into Akuma it produces no probe
output at all and every probe scores `NOT REACHED`, which reads exactly like a
catastrophic kernel regression. Three runs were lost to this before the cause
was checked. The box must be on **Ubuntu** for that arm; `--local-only` is the
arm that does not care.

`cowstale` was sampled rather than argued about: three isolated runs on the
remote arm after this change gave **1 fail, 2 pass**, and it also failed once on
the local arm in a run made *before* this change. That is the known pre-existing
no-TLB-shootdown race under `CLONE_VM` threads, which the step-6 hand-off says
to score against its own rate rather than against 8/10 — not a regression, and
not something to read a verdict into from a single run.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` issue 5 — the report this closes,
  and issue 3, whose closed diagnosis this one should be read against.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE1_REGISTRATION.md` — the slice whose
  ring-3 check surfaced it.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — enumerate every writer
  before reading a record to refuse something.
