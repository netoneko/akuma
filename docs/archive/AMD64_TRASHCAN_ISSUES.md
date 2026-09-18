# amd64 bare metal ("the trashcan") — open issues

**Status: OPEN.** Live defects on the HP 500-502nj bare-metal target and its
Firecracker/QEMU stand-ins. **Stability: C** — these are active, and at least one
has had its "root cause" overturned twice.

This is the **investigation log**. It was carved out of
[`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) on 2026-09-18: that document is
the *aspirational manual* for building the system on the metal, and a goal that
has not been reached yet should not be buried under three passes of debugging
narrative. The manual keeps the one-line statement of what must work; the
evidence lives here.

The runbook for driving the machine is
[`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md),
and its first rule applies to everything below: **grep `docs/archive/` before
forming a theory.** This target is a port, so most of what breaks here already
broke once on AArch64 — and when the archive says "fixed", the next question is
**"fixed where?"**, because a fix that landed in AArch64-only code leaves the
bug live here.

---

## 1. `git clone` over HTTPS hangs

**Status: FIXED 2026-09-19** — `execve` did not close `FD_CLOEXEC`
descriptors on this target. Four distinct causes wore this one signature; the
signature discriminates none of them, which is why the history below is kept in
full. **The fix is §1.5**; §§1.1–1.4 are how it was found.

**`git clone` over HTTPS from inside Akuma.** `git` 2.54.0 is installed on the
metal and works locally.

### Pass 1 — hung, blamed on DNS

**First attempt, 2026-09-18: hung — no DNS.** `git clone --depth=1
https://github.com/netoneko/akuma.git` sat with three processes
(`git clone`, `git remote-https`, `git-remote-https`) at **0:00 CPU across
12 s**. That signature is worth knowing because it is *blocked on I/O*, not
the SMP wedge — which also shows 0:00 but with `cargo`/`rustc` and after real
work. A hung resolver looks exactly like a stuck clone.

The bare-metal root needs the same two things the `box` rootfs needed for DNS
and HTTPS, and they are separate failures:

| missing | symptom |
|---|---|
| `/etc/resolv.conf` | clone **hangs** at 0:00 CPU, no error |
| CA bundle (`ca-certificates.crt`) | clone **fails with a TLS error** — git verifies github's certificate in its own stack |

Stage both on sdb1 before concluding anything about git's TLS support.

### Pass 2 — root-caused: connected-UDP syscalls, FIXED

**It was not TLS — it was the kernel.** `/etc/resolv.conf` was present and correct the whole time, and
`nslookup github.com` resolved fine. `curl` and `git` use **c-ares**, which
`connect()`s its UDP socket where musl's resolver does not, and three
syscalls on that connected path were wrong: `send()` answered `EBADF`
(a null `sendto` destination fell into the TCP path) and
`getsockname`/`getpeername` answered `ENOSYS`. Full account:
[`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md).

So this step's real lesson is the diagnostic, not the fallback list:
**`nslookup` working proves nothing about whether `git` can resolve.**
The CA-bundle row above is still untested — it simply never got reached.

### Pass 3 — the symptom returns: a third cause

**2026-09-18, later: the symptom came back, and it is a THIRD cause.**
Same three processes, same 0:00 CPU. The DNS fix is not regressed — it is
verified working on the kernel that shows this: `/root/probes/resprobe2`
passes every connected-UDP step (errno=0 throughout), and `git`'s **own
helper**, driven by hand, does the entire job in under 8 s —
`printf 'capabilities\nlist\n' | GIT_CURL_VERBOSE=1
/usr/libexec/git-core/git-remote-https origin <url>` resolves github.com,
connects to 20.217.135.5:443, completes a TLS 1.3 handshake and returns
response headers.

What hangs is `git` ↔ helper. Under `GIT_TRACE=1` the trace stops dead at
`start_command: git-remote-https` and `GIT_CURL_VERBOSE=1` yields **zero**
curl lines, with no outbound socket for the whole hang — the helper is
started and never receives its command. Both sides sit `State: R` at 0:00,
i.e. runnable-and-idle, not `D`.

Ruled out with numbers rather than reasoning: pipe exhaustion
(`[PIPES] live=4 high=16 refused=0 cap=256` in the idle report) and a
general fork/exec wedge (`( ls; ls )` grandchildren and `git --version`
both fine). Candidate to test first: the lost-wakeup mechanism the runbook's
OPEN "ssh needs one extra event" entry lists — a pipe reader never woken by
the first write deadlocks exactly like this.

**So this row's lesson has a second half: the 0:00-across-three-processes
signature now has three distinct causes** (a hung resolver, the SMP wedge,
and this), and it discriminates none of them. Drive the helper by hand
before blaming the network — it separates all three in one command.
Caveat on the evidence: the box had ~10 stuck git processes that `kill -9`
would not clear when this was narrowed, so re-run it on a fresh boot.
Why those processes never went away:
[`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) §3.1.

### Pass 4 — reproduced on Firecracker; lost-wakeup RULED OUT

**Reproduced on a fresh Firecracker boot, and the lost-wakeup candidate is
RULED OUT.** Same signature (`git ls-remote`, 25 s,
`GIT_TRACE=1` stops dead after `start_command: git-remote-https`), on a
kernel built that day, with the network proven good in the same session:
the helper driven by hand completed a TLS 1.3 handshake and got
`HTTP/2 200` with `content-type: application/x-git-upload-pack-advertisement`
in **0.9 s**.

The new evidence is the **slot census**, which the earlier pass did not
read. All four processes park at the *same* site — `fs.rs:740`, the
`pipe_check_set_reader` / `park_indefinitely` arm of the blocking pipe read
in `akuma-syscalls-glue` — with `sc=0` (x86_64 `read`):

```
[SLOT]  8 ... sc=0 scn=578 park=fs.rs:740 pid=Some(78)   git ls-remote / clone
[SLOT] 10 ... sc=0 scn=153 park=fs.rs:740 pid=Some(80)   git remote-https
[SLOT] 11 ... sc=0 scn=341 park=fs.rs:740 pid=Some(81)   git-remote-https
```

**`pick=` advances between consecutive censuses** (e.g. pid 78: 451 -> 481,
pid 81: 453 -> 483). A lost wakeup leaves a thread parked and never picked;
these are being scheduled, re-testing their condition, finding the pipe
empty and parking again. So the wake path is working and the ~500 lines of
sticky-`WOKEN_STATES` reasoning in `amd64/src/sched.rs` § "There is no
`prepare_block`" are not implicated. **The data is simply not in the pipe.**

That moves the question from "who failed to wake whom" to "where did the
first helper command go". `git` is parked *reading* the helper's stdout,
which means it believes it already wrote `capabilities\n` to the helper's
stdin; the helper is parked reading that same stdin having never seen it.
Note `git remote-https` (pid 80) is parked in `read`, not `wait4` — git's
`execv_dashed_external` runs the helper via `run_command` and should be
waiting on it, so that park site is itself unexplained and is the next
thread to pull.

Next probe, and it is cheap: a parent that writes to a pipe and *then*
forks+execs twice, with the reader at the second level — the shape git uses
and the shape `printf | git-remote-https` (which works) does not.

### Where it stands

Network, DNS and TLS are **proven good** on the kernel that shows the hang. The
wake path is **proven live**. What is unexplained is where git's first helper
command goes, and why `git remote-https` parks in `read` rather than `wait4`.

## Background

- [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) — the goal this blocks; §3
  keeps the one-line statement of the requirement.
- [`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md) — pass 2's fix.
- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) §3.1 — why
  the stuck git processes never went away, which contaminated pass 3's evidence.
- [`GIT_CLONE_STALE_ITIMER_SIGALRM.md`](GIT_CLONE_STALE_ITIMER_SIGALRM.md),
  [`GIT_MISSING_SYSCALLS.md`](GIT_MISSING_SYSCALLS.md) — the AArch64 git bugs.
  Both fixed **there**; `src/syscall/time.rs` is not code this target runs, so
  "fixed" needs checking against `amd64/` before it counts here.
- [`AKUMA_AMD64_STREAM_END_STALL.md`](AKUMA_AMD64_STREAM_END_STALL.md) — the
  other open "an edge that should have woken a reader did not" on this target.
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md) —
  how to drive the machine, and the rigs that need no reboot.

### Pass 5 — ROOT CAUSE: `execve` never closed `FD_CLOEXEC` fds

**Fixed 2026-09-19.** Not a pipe bug, not a wakeup bug, not the network.

The kernel's own `strace` (boot flag, with `init=/usr/bin/git` on the networked
Firecracker rig) ends the parent's trace at:

```
[sc>] task=2 nr=3 a1=0x8      close(8)
[sc>] task=2 nr=0 a1=0x7      read(7)      <- never returns
```

fds 7/8 are the **notify pipe** git's `start_command` creates. The contract is
pure POSIX: the child's copy of the write end is `FD_CLOEXEC`, the parent closes
its own copy and blocks reading the read end, and a **successful exec closes the
child's copy**, draining the last writer so the read returns EOF. That EOF *is*
how the parent learns the exec worked. (A failed exec instead writes `errno` to
it.)

This target never closed those fds, so the write end survived into the exec'd
image, the pipe kept a writer forever, and the read never returned. git
therefore never sent `capabilities` to the helper it had just started — which is
why every observation looked like a pipe problem and none of them was:

* `[PIPE-DUMP]` showed `bytes=0 writers>0` on **every** live pipe — "the kernel
  is behaving, nobody wrote", which was exactly true.
* `pick=` advanced across censuses, so no thread was stranded.
* Driving the helper by hand worked in 0.9 s: no notify pipe is involved.
* `printf | git-remote-https` and `printf | git remote-https` both worked, and a
  two-level `sh -c 'sh -c cat'` pipeline worked — none of them exec a child
  whose exec-success the parent learns by EOF.

**Why it was here and not on AArch64.** `Process::close_cloexec_fds` is shared
code and had exactly one caller: `akuma-syscalls-glue`'s `sys_execve`, the
AArch64 path. `amd64/src/usermode.rs`'s `sys_execve` had no sweep at all. This
is the runbook's "when the archive says fixed, ask **fixed where?**" in its
purest form — nothing was regressed, the call site simply never existed here.

**The fix**, at the POSIX point of no return (after `install_image` commits,
never before — a failed `execve` must leave the fd table untouched):

* `akuma-exec`: `release_fd_entry` factored out of `SharedFdTable::close_all`,
  and `Process::close_cloexec_fds_releasing()` built on it. `close_cloexec_fds`
  alone only removes the *names*; the references still have to be dropped, and a
  second inline copy of that list is how the two kernels drifted apart.
* `amd64/src/usermode.rs::sys_execve`: calls it.

Also landed on the way, because the bug was invisible without it:
`amd64/src/pipe.rs::report` now calls `glue::pipe_dump()`. The per-pipe
breakdown existed but its only callers were in `akuma-kernel-glue` — AArch64
again — so the one diagnostic that separates a lost wakeup from an unwritten
pipe was unreachable on this target.

**Verified on the box's Firecracker guest, kernel built from the fix:**

| | before | after |
|---|---|---|
| `git ls-remote <small repo>` | hang (>50 s, killed) | **1.2 s**, real refs |
| `git clone <small repo>` | hang | **6.4 s**, `README` checked out |
| `git clone --depth=1` Akuma's own repo | never attempted | **~21 s**, 1617 files, `status` clean, HEAD `e30fda9` (tag v0.0.8) |

That last row is §3.1 of [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) — the
gate the whole document waits on.
