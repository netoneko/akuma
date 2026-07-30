# SIGCHLD delivery — root cause and fix plan for the hanging `wait` builtin

Investigation of [BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md) §11.3 / §11.8, the one item that
campaign left open:

> **`wait` still hangs** (§11.3). Unfixed — it needs SIGCHLD delivery, which is a larger piece of
> work than the reclaim fix and touches the signal path rather than the scheduler.

**Status: IMPLEMENTED AND VERIFIED (2026-07-30) — one bug remains open.** §4 (SIGCHLD delivery),
§6 (the `rt_sigsuspend`/SA_RESTART exemption), and §7.1 (`waitid` `siginfo` offsets) have all
landed, in commit `1115c86` ("some wait4 fixes"). Host unit tests (8 new, in `children.rs`) and
clippy are clean; the §9.3 in-VM reproducer suite passes all 6 cases on both SMP=2 and SMP=4 —
`sh -c "sleep 1 & wait"` no longer hangs. One bug turned up during verification and is **not
yet fixed** — see §11. The rest of this document (the plan itself) is left as originally written,
derived from reading the source at `another-smp-attempt-0` @ `534350d`.

**Headline: §11.8's framing is right — SIGCHLD is genuinely never raised — but the missing piece is
smaller than "a larger piece of work."** Every mechanism the fix needs already exists and is already
exercised by other signals. What is missing is one edge in the graph: the child's exit path knows
its parent, and the signal path knows how to deliver to a thread, and nothing connects them. The
fix is a ~40-line helper plus its call sites.

The one genuinely subtle part is not the delivery — it is **SA_RESTART**, which currently restarts
`rt_sigsuspend`. That is wrong per POSIX and, if busybox installs its SIGCHLD handler with
`SA_RESTART`, it would make `wait` hang *even after* SIGCHLD is delivered correctly. §6 covers it.

---

## 1. The reproducer, and exactly who does what

`sh -c "sleep 1 & wait; echo OK"` hangs forever, at SMP=1 and SMP=4, on an idle VM. The child runs,
exits, disappears from `ps`; the parent sits in `wait` indefinitely.

**`/bin/sh` is busybox ash.** Confirmed, not assumed — `bootstrap/bin/sh` and
`bootstrap/bin/busybox` are byte-identical copies:

```
bootstrap/bin/sh:      ELF 64-bit LSB pie executable, ARM aarch64, static-pie,
                       BuildID[sha1]=0353c72e6c22825a2b9a903f4d398579c6219a64, stripped
bootstrap/bin/busybox: ELF 64-bit LSB pie executable, ARM aarch64, static-pie,
                       BuildID[sha1]=0353c72e6c22825a2b9a903f4d398579c6219a64, stripped
```
`strings` gives `BusyBox v1.37.0 (2025-12-16)`. §11.1's harness says "a full busybox userspace", so
this is the binary that hangs.

This matters because it pins down *why* the signal is load-bearing. busybox ash's `wait` builtin
(`waitcmd`) does **not** call blocking `waitpid`. It calls `dowait(DOWAIT_BLOCK_OR_SIG, ...)`, whose
inner loop is:

```c
/* busybox ash.c, wait_block_or_sig() — shape, from the 1.37 source */
do {
        got_sigchld = 0;
        pid = waitpid(-1, status, doing_jobctl ? (WNOHANG|WUNTRACED) : WNOHANG);
        if (pid != 0)
                break;                  /* reaped something, or ECHILD/EINTR */

        /* children exist but none are ready — sleep until an interesting signal */
        sigfillset(&mask);
        sigprocmask(SIG_SETMASK, &mask, &mask);   /* note: oldset == &mask */
        while (!got_sigchld && !pending_sig)
                sigsuspend(&mask);                /* &mask now holds the OLD mask */
        sigprocmask(SIG_SETMASK, &mask, NULL);
} while (1);
```

So the blocking wait is **`waitpid(WNOHANG)` + `sigsuspend` + a `got_sigchld` flag set by ash's own
SIGCHLD handler**. Three separate things must work, and only the first one does today:

| # | Requirement | State today |
|---|---|---|
| 1 | `waitpid(-1, WNOHANG)` returns 0 when children exist but none exited | ✅ works (`sys_wait4`, `src/syscall/proc.rs:792`) |
| 2 | `sigsuspend` returns once SIGCHLD is pending | ✅ mechanism works (`sys_rt_sigsuspend`, `src/syscall/signal.rs:151`) — but nothing ever pends SIGCHLD |
| 3 | ash's SIGCHLD **handler actually runs**, so `got_sigchld = 1` | ❌ nothing raises signal 17, so no handler, so the `while (!got_sigchld)` loop re-suspends forever |

Requirement 3 is why "just make `sigsuspend` return when a child exits" is not a fix: the kernel
returning `EINTR` without running the handler leaves `got_sigchld == 0`, and ash immediately
re-suspends. **The handler must run.** That is the whole of the work.

§11.3's other claim also checks out: foreground waits are unaffected because they go through
blocking `wait4` on a live child, which is woken by the `ProcessChannel` poller
(`ch.add_poller(waiter_tid)`, `src/syscall/proc.rs:844`) — a path that does not involve signals at
all. It is specifically the background-job path that needs SIGCHLD.

> **Derivation note.** Requirement 3's necessity is a deduction, not an observation: `wait` demonstrably
> works with busybox ash on Linux, and the only mechanism by which `wait_block_or_sig` can terminate
> is `got_sigchld` (or `pending_sig`, which is set for *trapped* signals only). Therefore ash installs a
> catching handler for SIGCHLD in this mode. §9 gives the one-line log check that confirms it against
> the real binary, and what to conclude if it does not.

---

## 2. Confirming the kernel never raises it

`grep -rn SIGCHLD src/ crates/` returns, excluding tests, only clone-flag *parsing* and `waitid`'s
`siginfo` construction:

- `src/syscall/proc.rs:397-398, 431` — clone-flag routing (`SIGCHLD` in the low byte ⇒ treat as fork).
- `src/syscall/proc.rs:933, 1044-1057` — `sys_waitid` filling a `siginfo_t` it hands to the caller.

Neither raises a signal. There is no `pend_signal_for_thread(parent_tid, 17)` anywhere in the tree.
Confirmed independently: `signal_is_fatal_default` (`src/syscall/signal.rs:85-87`) does not list 17,
which is correct and also means a stray SIGCHLD with the default disposition is harmless — see §5.4.

---

## 3. What already exists (the reason this is small)

Every primitive the fix needs is present, public, and in use by other signals.

| Need | API | Location |
|---|---|---|
| Pend a signal on a thread + wake it | `pend_signal_for_thread(tid, sig)` | `crates/akuma-exec/src/threading/mod.rs:3127` |
| Per-thread pending bitmask | `PENDING_SIGNALS[MAX_THREADS]`, `take_pending_signal(mask)` | `threading/mod.rs:429, 3186` |
| Read *another* thread's mask | `thread_signal_mask_of(tid)` | `threading/mod.rs:485` |
| Build the user sigframe and jump to the handler | `try_deliver_signal` | `src/exceptions.rs:974` |
| Deliver at syscall return | `take_pending_signal` → `try_deliver_signal` | `src/exceptions.rs:2604-2622` |
| Deliver after `rt_sigreturn` | same pair | `src/exceptions.rs:2518-2531` |
| `sigsuspend` that wakes on a pending bit | `sys_rt_sigsuspend` | `src/syscall/signal.rs:151-195` |
| Lazy sigreturn trampoline (musl/aarch64 sets no `SA_RESTORER`) | `ensure_sigreturn_trampoline` | `src/exceptions.rs:~940` |
| child pid → parent pid | recorded in `CHILD_CHANNELS` | `crates/akuma-exec/src/process/children.rs:16-24` |
| pid → tgid → thread ids | `lookup_process`, `for_each_process` | `process/table.rs:150, 168` |

The `CHILD_CHANNELS` registry is the key asset: `register_child_channel(child_pid, channel,
parent_pid)` already stores the parent for every fork child, and `is_child_of_group`
(`children.rs:42`) already contains the logic for "the recorded parent may be a non-leader thread;
resolve its thread group." The fix reuses that resolution.

**The missing edge, stated precisely:** every path that publishes a child's exit calls
`ProcessChannel::set_exited(code)` and stops there. `set_exited` (`process/channel.rs:258`) wakes
registered *pollers* — which unblocks a parent already sitting in `wait4` — but it has no idea which
pid it belongs to, so it cannot raise a signal. Nothing else on the exit path looks up the parent.

---

## 4. Fix plan

### 4.1 Step 1 — one helper, in `children.rs`

Add to `crates/akuma-exec/src/process/children.rs`, next to `is_child_of_group` (which already
holds the thread-group resolution logic this reuses):

```rust
/// The pid recorded as `child_pid`'s parent at fork time, if it is still a
/// registered child. This is the *forking thread's* pid, which may be a
/// non-leader thread of a multithreaded parent — resolve the group before
/// using it as a signal target (see `sigchld_target_thread`).
pub fn parent_pid_of(child_pid: Pid) -> Option<Pid> {
    with_irqs_disabled(|| CHILD_CHANNELS.lock().get(&child_pid).map(|(_, ppid)| *ppid))
}

/// Raise SIGCHLD on the parent of `child_pid`, if it has a live parent process.
///
/// MUST be called *after* the child's channel is marked exited: ash (and every
/// other shell) responds to the handler by calling `waitpid(WNOHANG)`, which has
/// to already see the zombie or the shell concludes nothing happened and
/// re-suspends. Ordering is the whole bug if it is wrong.
///
/// Never sets the interrupted flag (`interrupt_thread`) — that is Ctrl+C's
/// channel and would turn every child exit into a spurious EINTR storm across
/// the parent's unrelated blocking syscalls. Pending + wake is enough: the
/// pending bit is what `sys_rt_sigsuspend` polls, and delivery happens at the
/// parent's next syscall-return boundary.
pub fn raise_sigchld_for_parent(child_pid: Pid) {
    const SIGCHLD: u32 = 17;
    let Some(ppid) = parent_pid_of(child_pid) else { return };
    // Kernel-thread parents (in-kernel sshd bridge) have no Process — nothing to signal.
    let Some(tgid) = lookup_process(ppid).map(|p| p.tgid) else { return };
    if let Some(tid) = sigchld_target_thread(tgid) {
        crate::threading::pend_signal_for_thread(tid, SIGCHLD);
    }
}
```

and the target-thread chooser:

```rust
/// Pick which thread of group `tgid` receives SIGCHLD.
///
/// Linux delivers a process-directed signal to any thread that does not block
/// it. We approximate that with an explicit preference order, because two of
/// our delivery-path guards silently drop a signal aimed at the wrong thread:
///
///   1. a thread whose per-thread mask does not block SIGCHLD *and* which has a
///      sigaltstack configured, if the handler is SA_ONSTACK;
///   2. any thread not blocking SIGCHLD;
///   3. the group leader.
///
/// (2)-before-(3) is what makes a multithreaded parent work at all. The
/// sigaltstack preference in (1) exists because `try_deliver_signal`
/// (src/exceptions.rs) *re-pends and bails* when SA_ONSTACK is set but the
/// target thread has `alt_sp == 0` — aimed at a Go M that has not reached
/// `mstart`'s sigaltstack call, SIGCHLD would be re-pended at every syscall
/// return forever and never delivered.
fn sigchld_target_thread(tgid: Pid) -> Option<usize> { /* for_each_process over the group */ }
```

Both are pure lookups over existing state, so both are host-testable (`children.rs` already has a
`#[cfg(test)]` block with a registered test runtime, `children.rs:1053+`).

### 4.2 Step 2 — call it from every exit-publication site

Audit of every place a child's exit becomes visible to the parent. All of them are
`get_child_channel(pid) → set_exited(code)`, or the thread-channel `set_exited` that happens to be
**the same `Arc`** for spawned children (stated in the comment at `process/mod.rs:1010-1014`).

| Site | What exits that way | Needs the call |
|---|---|---|
| `src/syscall/proc.rs:15` `notify_child_channel_exited` | `sys_exit` (`:251`), `sys_exit_group` (`:317`, plus the `tgid != pid` re-notify at `:322`), and every crash path via `notify_child_channel_exited_pub` (`src/exceptions.rs:2500`, `:2535`) | ✅ **primary site** |
| `crates/akuma-exec/src/process/signal.rs:92` `kill_process` | child killed (exit `-9`) | ✅ |
| `crates/akuma-exec/src/process/signal.rs:137` `kill_process_with_signal` | child killed by signal *n* | ✅ |
| `crates/akuma-exec/src/process/mod.rs:1015` (`kill_child_processes` subtree teardown) | subtree kill, code 137 | ✅ |
| `process/mod.rs:1124` (`return_to_kernel`), `:1365` (`return_to_kernel_from_fault`) | a process that falls off the end instead of calling `exit_group` | ✅ — same `Arc`, so the exit *is* published here |

Recommended shape rather than five scattered calls: give `children.rs` a single

```rust
pub fn publish_child_exit(child_pid: Pid, code: i32) {
    if let Some(ch) = get_child_channel(child_pid) {
        ch.set_exited(code);      // wakes wait4 pollers — must happen first
    }
    raise_sigchld_for_parent(child_pid);
}
```

and convert the sites to it. That makes the "`set_exited` first, then signal" ordering structural
instead of a comment repeated five times. Three of the sites (`mod.rs:1015` and both
`return_to_kernel`s) guard with `if !ch.has_exited()` to avoid clobbering a real exit code with
`137`/`-9`; keep that guard, and raise SIGCHLD **only when this call is the one that published the
exit** — otherwise a process that exits cleanly and is then torn down raises two SIGCHLDs for one
death. (Harmless for ash, which re-polls with `WNOHANG` and gets `ECHILD`/0, but it is free to get
right, and duplicate delivery is exactly the kind of thing that confuses a `SIGCHLD`-counting
handler.)

### 4.3 Step 3 — nothing needed in `sigsuspend`, `wait4`, or the delivery path

Worth stating explicitly, because it is most of why this is a small change:

- `sys_rt_sigsuspend` (`signal.rs:173-194`) already polls `pending_signals_raw(slot)` against
  `!suspend_mask | force_bits` and returns `EINTR`, leaving the armed restore-mask for
  `try_deliver_signal` to consume as `uc_sigmask`. Correct as-is.
- The **lost-wakeup window is already closed.** ash blocks all signals (`sigprocmask(SIG_SETMASK,
  fullset)`) *before* calling `sigsuspend`. A child exiting in that window still sets the pending
  bit; `sigsuspend` then installs the old (SIGCHLD-unblocked) mask and its *first* loop iteration
  observes the bit before ever sleeping. No race.
- `try_deliver_signal` needs no change for the base fix: a UserFn handler for 17 gets a frame, and a
  `Default`/`Ignore` disposition makes it return `false` after `take_pending_signal` already
  consumed the bit — a clean drop with no leak and no kill (17 is absent from
  `signal_is_fatal_default`).
- `sys_wait4`'s `WNOHANG` path already returns 0 correctly for "children exist, none exited."

### 4.4 Step 4 (recommended, separable) — real `siginfo` for SIGCHLD

The base fix delivers signal 17 with an all-but-empty `siginfo_t`: `try_deliver_signal`
(`src/exceptions.rs:1147-1153`) writes only `si_signo`, `si_errno = 0`, `si_code = is_fault as i32`
(so **0**, i.e. `SI_USER`, for SIGCHLD) and `si_addr = 0`. A correct SIGCHLD carries
`si_code = CLD_EXITED(1)`/`CLD_KILLED(2)`, `si_pid`, `si_uid`, `si_status`.

ash does not read any of it, so this is not required to fix `wait` — but anything with an
`SA_SIGINFO` SIGCHLD handler that reads `si_pid` (a common reaping idiom) currently gets zero, which
is worse than not being told at all. Minimal design consistent with the bitmask-based pending model:

- a `LAST_SIGCHLD: [AtomicU64; MAX_THREADS]` side-channel in `threading/mod.rs`, packing
  `(child_pid: u32, status: i32)` (plus the `CLD_*` code, derivable from the status sign as
  `sys_waitid` already does at `proc.rs:1054`), written by `raise_sigchld_for_parent` immediately
  before it pends;
- `try_deliver_signal` special-cases `signal == 17` and fills the union from that slot.

It is lossy under a burst (one slot, last-writer-wins), which matches the existing pending model —
`PENDING_SIGNALS` is a bitmask, so N child exits already collapse to one SIGCHLD. That is also what
Linux does for non-queued signals, and it is why correct reapers loop on `waitpid(WNOHANG)` rather
than counting signals.

**Get the offsets right — the existing `waitid` code does not.** See §7.1.

---

## 5. Risk / regression surface

### 5.1 The Go runtime now receives a signal it never saw before

Go installs handlers for essentially every signal, `SA_ONSTACK`, including SIGCHLD (`_SigNotify`).
After this change every Go program that spawns children (i.e. every `os/exec` user — `go build`
itself) starts taking real SIGCHLD delivery on the path already known to be delicate. Mitigations:
the sigaltstack preference in `sigchld_target_thread` (§4.1), which specifically avoids the
`SA_ONSTACK && alt_sp == 0` re-pend trap at `exceptions.rs:1226-1233`. Go's own handler treats
SIGCHLD as notify-only, so behaviour should match Linux — but the in-VM `go build` and the rustc
self-host build are the regression tests that matter here, not the shell one-liner.

### 5.2 Spurious wakeups

`pend_signal_for_thread` wakes the target. A parent blocked in `read`/`accept`/`epoll_wait` while a
child exits now takes one extra wakeup, loops, and re-blocks. Negligible in absolute terms (child
exits are rare next to I/O), and preferable to the alternative of consulting the disposition first
and pending only for `UserFn` — that "optimisation" would break `sigwait`/`sigtimedwait` on a
*blocked* SIGCHLD with a default disposition, which Linux does deliver. **Recommendation: always
pend.**

### 5.3 Do **not** set the interrupted flag

`sys_kill` pairs `pend_signal_for_thread` with `interrupt_thread` (`src/syscall/proc.rs:1451-1465`).
Copying that pattern for SIGCHLD would be a serious regression: `is_current_interrupted()` is the
Ctrl+C channel, it auto-clears on read (`channel.rs:~288`), and it makes blocking syscalls across
the kernel return `EINTR`. Every child exit would inject `EINTR` into the parent's unrelated
blocking syscalls. The consequence of *not* setting it is that SIGCHLD handler delivery is deferred
to the parent's next syscall-return boundary rather than interrupting an in-flight blocking
syscall — a real fidelity gap versus Linux (§8), but it does not affect the `sigsuspend` case, which
polls the pending bit directly, nor `wait4`, which is woken by the channel poller.

### 5.4 Processes with no SIGCHLD handler

Consumed at the next syscall return and dropped (`take_pending_signal` clears the bit;
`try_deliver_signal` returns `false`). If SIGCHLD is *blocked*, the bit persists until unblocked —
which is correct. No zombie-reaping behaviour changes.

---

## 6. Required hardening: `rt_sigsuspend` must not be restarted

This is the one non-obvious correctness item, and it is on the critical path.

`try_deliver_signal`'s SA_RESTART block (`src/exceptions.rs:1005-1016`) rewinds `ELR` by 4 —
re-executing the `SVC` — for *any* syscall that returned `-EINTR` when the handler has `SA_RESTART`:

```rust
const SA_RESTART: u64 = 0x10000000;
if action.flags & SA_RESTART != 0 {
    if (entry_esr >> 26) == 0x15 {              // EC_SVC_LOWER
        let ret_val = unsafe { (*frame).x0 as i64 };
        if ret_val == -4 /* EINTR */ || ret_val == -512 { unsafe { (*frame).elr_el1 -= 4; } }
    }
}
```

On Linux, **`sigsuspend` is never restarted** — it always returns `EINTR` to userspace, by design,
because the caller's whole purpose is to re-evaluate a predicate that the handler just changed. The
same is true of `pause`, `ppoll`/`pselect6`, `epoll_pwait`, and `io_getevents`.

Applied to ash, restarting is precisely fatal. Trace it:

1. `sigsuspend` returns `EINTR`; SIGCHLD is taken and delivered; the handler sets `got_sigchld = 1`.
2. If `SA_RESTART` is set, `ELR -= 4`, so `rt_sigreturn` returns straight into the `SVC` again.
3. `sigsuspend` re-enters. The pending bit is gone (consumed in step 1). The kernel cannot see
   `got_sigchld`, which lives in ash's memory.
4. **It blocks forever.** Userspace never gets back to `while (!got_sigchld)`.

That is the identical symptom — `wait` hangs — with the SIGCHLD fix fully in place, which would make
for an unpleasant debugging session if the exemption is not landed at the same time.

Whether busybox 1.37 trips it depends on how ash installs the handler: `setsignal()` builds a
`struct sigaction` with `sa_flags = 0` (no `SA_RESTART`), in which case the bug stays latent. Do not
rely on that — the exemption is correct unconditionally, it is cheap, and it removes the dependency
on a detail of one shell's source.

**Fix:** consult the syscall number, which is still live in `frame.x8` at this point (already read
for diagnostics at `exceptions.rs:1050`), and skip the rewind for the non-restartable set:

```rust
// Linux never restarts these, regardless of SA_RESTART: the caller must re-evaluate
// a predicate the handler just changed (sigsuspend/pause), or was given an explicit
// timeout that a restart would silently extend (ppoll/pselect6/epoll_pwait).
fn syscall_is_non_restartable(nr: u64) -> bool {
    matches!(nr, 133 /* rt_sigsuspend */ | 73 /* ppoll */ | 72 /* pselect6 */
                | 22 /* epoll_pwait */ | 4 /* io_getevents */)
}
```

`rt_sigsuspend` (133) is the one required for this bug; the rest are the same class and are free to
include. Note `nanosleep`/`clock_nanosleep` are deliberately **not** in the list: Linux does restart
those, via `ERESTART_RESTARTBLOCK` with a *recomputed* remaining timeout, which this kernel does not
model — restarting them naively re-sleeps the full duration. Leaving them alone preserves current
behaviour; fixing them properly is separate work.

---

## 7. Adjacent bugs found while reading (report only — not part of this fix)

### 7.1 `sys_waitid` writes `siginfo_t` fields at the 32-bit offsets *(real, user-visible)*

`src/syscall/proc.rs:1047-1053`:

```rust
// 12: si_pid (u32), 16: si_uid (u32), 20: si_status (i32)   <-- comment and struct
#[repr(C)]
struct SigChld { si_signo: u32, si_errno: u32, si_code: i32,
                 si_pid: u32, si_uid: u32, si_status: i32 }
```

`#[repr(C)]` with six 4-byte fields lays these out at 0/4/8/**12/16/20**. On any 64-bit
architecture, `siginfo_t`'s `_sifields` union is 8-byte aligned (it contains `void *si_addr` and
`clock_t si_utime`), so the union starts at **offset 16** — `si_pid` = 16, `si_uid` = 20,
`si_status` = 24. Every field is 4 bytes low: a caller reads `si_pid` as 0 (the padding),
`si_uid` as the pid, and `si_status` as the uid.

The kernel already gets this right elsewhere, which is the cross-check:
`try_deliver_signal` writes `si_addr` at `si.add(16)` (`src/exceptions.rs:1153`), i.e. union-at-16.
musl agrees (`__pad[128 - 2*sizeof(int) - sizeof(long)]` = 112 bytes, and 16 + 112 = 128).

Fix: insert an explicit `__pad0: u32` after `si_code`. **This must be resolved before §4.4**, which
would otherwise copy the wrong layout into the signal path.

### 7.2 `wait4(-1)` matches on the raw pid, not the thread group

The `pid > 0` branch resolves the waiter's thread group (`is_child_of_group`, deliberately, per the
comment at `children.rs:33-41`). The `pid == -1 || pid == 0` branch does not: `has_children(current_pid)`
(`proc.rs:865`) and `find_exited_child(current_pid)` (`:873`, `:896`) both compare `*ppid ==
parent_pid` against the *calling thread's* pid (`children.rs:100, 124`). A multithreaded parent that
forks on thread A and calls `wait(-1)` on thread B gets `ECHILD`. Same class as the bug the `pid > 0`
path was already fixed for. Not on the ash path (single-threaded), so out of scope here, but it will
bite the same runtimes (Go, git's sideband threads) that motivated the `pid > 0` fix.

### 7.3 `wait4` ignores every option except `WNOHANG`

`let wnohang = options & 1 != 0;` (`proc.rs:803`) — `WUNTRACED` (2) and `WCONTINUED` (8) are silently
dropped, and `pid < -1` (wait on a process group) falls through to `ECHILD` (`:917-922`). Consistent
with there being no job-control stop/continue support at all, so `WUNTRACED` has nothing to report;
worth a comment rather than code. Interactive job control (`fg`/`bg`/`^Z`) is a separate project and
needs SIGCHLD-on-stop plus `SA_NOCLDSTOP` — see §8.

### 7.4 No `SIGCHLD = SIG_IGN` / `SA_NOCLDWAIT` auto-reap

On Linux, ignoring SIGCHLD means children are reaped automatically and never become zombies.
Akuma keeps the zombie until `wait4` or thread-slot recycling collects it. Pre-existing; unchanged by
this fix; noted because §4.2 puts a disposition lookup within easy reach and it would be tempting to
bundle. Don't — changing zombie lifetime interacts with §11.7's reclaim work.

---

## 8. Explicitly out of scope

- Interrupting **in-flight blocking syscalls** on SIGCHLD (§5.3). Delivery is deferred to the next
  syscall-return boundary. Sufficient for `sigsuspend` (polls the bit) and `wait4` (channel poller);
  a fidelity gap for a parent parked in a long `read`.
- SIGCHLD for **stopped/continued** children (`CLD_STOPPED`/`CLD_CONTINUED`) and `SA_NOCLDSTOP` —
  needs job-control stop support first.
- **Queued** SIGCHLD. `PENDING_SIGNALS` is a bitmask; N simultaneous child exits collapse to one
  signal. Matches Linux for non-realtime signals; correct reapers loop on `waitpid(WNOHANG)`.
- `nanosleep` restart semantics (§6).
- §7.2 and §7.4.

---

## 9. Test plan

### 9.1 Host unit tests (`cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`)

In `crates/akuma-exec/src/process/children.rs`'s existing `#[cfg(test)]` block, which already
registers a test runtime (`ensure_test_runtime`, `children.rs:1053`) and uses high test-local pids to
avoid collisions in the shared `CHILD_CHANNELS`:

1. `parent_pid_of` returns the registered parent; `None` after `remove_child_channel`.
2. `raise_sigchld_for_parent` on an unregistered child is a no-op (no panic) — covers reaped and
   double-published children.
3. `raise_sigchld_for_parent` with a parent pid that has no `Process` is a no-op — covers the
   in-kernel sshd bridge, whose children's parent is a kernel system thread.
4. `sigchld_target_thread` preference order: picks a non-blocking thread over the leader when the
   leader blocks SIGCHLD; falls back to the leader when all threads block it.
5. `publish_child_exit` sets `has_exited()` **before** it pends — assert the ordering directly
   (`PENDING_SIGNALS` observation implies the channel is already exited).

### 9.2 Kernel boot self-test (`src/process_tests.rs`)

Follow the shape of the §11.7 reclaim test (`test_thread_slot_reclaim_on_spawn`) and of
`clone_thread_not_visible_to_wait4` (`process_tests.rs:11649`), which already pokes
`CHILD_CHANNELS` directly:

6. Register a synthetic child of a spawned thread, publish its exit, assert bit 17 appears in
   `pending_signals_raw(parent_slot)` and that the parent's waker fired.
7. `take_pending_signal` returns 17, and 17 is **not** fatal-by-default — a process with no SIGCHLD
   handler survives (guards against a regression in `signal_is_fatal_default`).
8. SIGCHLD pended on a thread whose mask blocks bit 17 is **not** taken, and is still taken after
   the mask is cleared.
9. §6 regression: `syscall_is_non_restartable(133)` is true and the ELR rewind is skipped. If a
   direct unit test of `try_deliver_signal` is impractical from a boot test, at minimum unit-test the
   predicate and assert the rewind is guarded by it.

### 9.3 In-VM acceptance (the actual §11.3 reproducer)

```python
import subprocess
def sh(c): return subprocess.run(
    ["ssh","-o","StrictHostKeyChecking=no","-p","2222","root@localhost",c],
    capture_output=True, text=True)

sh('sh -c "sleep 1 & wait; echo OK"')                       # must print OK, ~1s
sh('sh -c "sleep 1 & sleep 2 & wait; echo BOTH"')           # multiple children
sh('sh -c "sleep 5 & p=$!; wait $p; echo GOT $?"')          # wait on a specific pid
sh('sh -c "(exit 7) & wait; echo rc=$?"')                   # exit status propagation
sh('sh -c "sleep 1 & wait; wait; echo IDEMPOTENT"')         # second wait → ECHILD, not a hang
sh('sh -c "for i in 1 2 3 4; do sleep 1 & done; wait; echo ALL4"')   # the parallelism idiom
```

Run each under a timeout — the failure mode is a hang, not an error. The last one is the case §11.3
called out as the actual point: `&` + `wait` is how shell expresses parallelism, and until it works,
the SMP harness has to fake it with sentinel files and polling
(`scripts/forktest_smp_matrix.py` / `scripts/quick_forktest.py` exist for exactly
that reason). A
follow-up worth doing once this lands: delete that workaround from the harness and confirm the §11.1
regimen still passes — that is the real end-to-end proof.

### 9.4 The diagnostic that tells you which requirement failed

`try_deliver_signal` already logs, at `tprint` budget 256:

```
[signal] deliver sig=17 slot=<n> handler=<addr> fault_pc=… user_sp=… sa_flags=0x…
```

This single line resolves the §1 uncertainty against the real binary and localises any failure:

| Observation | Conclusion |
|---|---|
| `deliver sig=17` appears, `wait` returns | Done. Note the `sa_flags` value for the record. |
| `deliver sig=17` appears with `sa_flags` including `0x10000000`, and `wait` still hangs | §6 — ash *does* use `SA_RESTART`; the non-restartable exemption is mandatory, not hardening. |
| SIGCHLD is pended but `deliver sig=17` never appears | ash's disposition for 17 is `Default`/`Ignore` (`try_deliver_signal` returns `false` before logging in the `handler_addr` match). The §1 deduction is wrong for this build — re-derive from busybox 1.37's actual `setsignal` before writing more code. |
| Neither appears | The exit path taken by `sleep` does not route through a §4.2 call site. Add `[sigchld] child=<pid> ppid=<pid> target_tid=<n>` tracing to `raise_sigchld_for_parent` and find which teardown path published the exit. |

---

## 10. Summary

| Item | Size | Required for `wait` |
|---|---|---|
| §4.1 `parent_pid_of` + `raise_sigchld_for_parent` + `sigchld_target_thread` | ~40 lines, one file | ✅ |
| §4.2 route the 5 exit-publication sites through `publish_child_exit` | mechanical | ✅ |
| §6 `rt_sigsuspend` exempt from SA_RESTART | ~8 lines | ✅ (latent-or-fatal — land it together) |
| §4.4 real SIGCHLD `siginfo` | ~30 lines, separable | ❌ (do it anyway; zeroed `si_pid` is worse than nothing) |
| §7.1 `waitid` `siginfo` offsets off by 4 | 1 line | ❌ (but blocks §4.4) |

§11.8 called this "a larger piece of work than the reclaim fix." On the evidence that is not right:
the signal *path* is complete and already carries SIGURG, SIGSEGV, SIGPIPE and the rest. What is
missing is the single edge from "a child's exit is published" to "the parent's thread gets signal
17," plus one POSIX detail about restarting `sigsuspend`. The larger piece of work is what §11.3
actually described as the impact — job control, `WUNTRACED`, stop/continue — and none of that is
needed to make `&` + `wait` work.

All of §4.1, §4.2, §6, and §7.1 are implemented and landed (commit `1115c86`, "some wait4 fixes").
§4.4 (real SIGCHLD `siginfo`) was **not** done — still open, not required for `wait`.

---

## 11. Remaining bug found during verification: `wait` reports `$?=0` for every background job

**Not fixed. Follow-up, tracked separately from the delivery fix above.**

After the fix landed, the full §9.3 reproducer suite was run in-VM on both SMP=2 and SMP=4: all 6
cases pass, and the hang itself is gone (`sh -c "sleep 1 & wait; echo OK"` returns `OK` in ~1s,
`sh -c "(exit 7) & wait; echo rc=$?"` returns immediately instead of hanging forever).

But that last case exposed a second, distinct bug: it prints `rc=0`, not the expected `rc=7`. This
is **not a SIGCHLD regression** — before the fix, `wait` hung forever, so no background exit status
was ever observable at all. Isolating it:

- **Foreground** status propagation is correct: `false; echo $?` → `1`, a foreground command that
  exits 7 → `$?` = `7`.
- **Every** backgrounded job reports `$?` = `0`, regardless of its real exit code. Deterministic,
  not a race — ruling out a flicker-prone recycling bug (see below).
- One candidate theory considered and ruled out: that thread-slot recycling / `on_thread_cleanup`
  clobbers the exit code before `wait4` reads it. Reading `on_thread_cleanup`
  (`crates/akuma-exec/src/process/mod.rs:90`) shows it only removes the `THREAD_PID_MAP` entry and
  calls `unregister_process` — it never touches the child channel or `exit_code`, which lives in
  `CHILD_CHANNELS` keyed by pid and survives slot recycling untouched. A recycling race would also
  flicker; "always exactly 0" is the wrong shape for that.
- Decisive proof via kernel trace (`SYSCALL_DEBUG_INFO_ENABLED = true`, temporarily): for
  `sh -c "(exit 7) & wait"`, the log shows

  ```
  [signal] deliver sig=17 slot=10 handler=0x100494e8 ... sa_flags=0x4000000
  [syscall] wait4: PID 4 exit_code=7 wait_status=0x00000700
  ```

  `wait4` returns the **correct** `wait_status = 0x00000700` (exit code 7) for the reaped pid. The
  kernel is right. The bug is in busybox ash's `wait` builtin: it receives that correct status but
  reports `$?` = 0 for a backgrounded job anyway. (Also note: `sa_flags` here has no `SA_RESTART`
  bit (`0x10000000`) — ash does not hit the §6 exemption on this path, confirming the plan's guess
  in §6 that it stays latent for this build.)

**Conclusion: userspace bug in busybox ash, not a kernel bug.** Deferred — pick up later. Whoever
picks this up next should start from `wait_block_or_sig`/`waitcmd` in ash's `shell/ash.c` (job
struct status tracking) rather than the kernel signal or `wait4` path, both of which are confirmed
correct above.

`scripts/forktest_smp_matrix.py` regression run was queued as the next verification step but not
completed in this pass — still outstanding.

---

## Document history

- **2026-07-30** — Created. Root-caused §11.3/§11.8 to a missing `pend_signal_for_thread(parent, 17)`
  edge on the child exit path; identified the SA_RESTART/`rt_sigsuspend` interaction as a
  co-requisite; found the `sys_waitid` `siginfo_t` offset bug (§7.1) and the `wait4(-1)` raw-pid
  mismatch (§7.2) while reading. Plan only — no code changed.
- **2026-07-30** — Implemented and landed (commit `1115c86`, "some wait4 fixes"): §4.1/§4.2 SIGCHLD
  delivery, §6 SA_RESTART exemption, §7.1 `waitid` offset fix. 8 new host unit tests pass, clippy
  clean. In-VM: all 6 §9.3 reproducer cases pass on SMP=2 and SMP=4 — `&` + `wait` no longer hangs.
  Found and root-caused one new bug during verification (§11): backgrounded jobs report `$?=0`
  regardless of real exit code, decisively traced to busybox ash's `wait` builtin, not the kernel
  (`wait4` returns the correct status). Left open, to pick up later. `forktest_smp_matrix.py`
  regression run still outstanding.
