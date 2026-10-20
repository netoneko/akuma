# crush-bin survives its ssh PTY — orphan source unknown (maybe a tid cleanup bug)

**Status: OPEN, hypothesis only.** Found 2026-10-20 while verifying kernel `27924603`
(signal return preserves rcx/r11 + FPU file) on ryzen metal, SMP=8, GRUB entry 20.

## What happened

1. `crush-glm run "Reply with exactly the word OK"` over `wifi-ssh.py`: printed `OK`, rc 0.
2. `ssh_pty_steps.py ... 'cd /src/github.com/netoneko/akuma && exec crush-glm'`, waited 25 s,
   script exited (ssh PTY closed). crush had reached its Ready screen.
3. Minutes later `pidof crush-bin` still listed live pids (1069, 1071-1074), all `State: R`.
   `[PSTATS]` for them over the next 70 s: pid 1069 `epoll_pwait` + `nanosleep` + `wait4`,
   `in_kernel` ≈ wall time; its threads spinning in the syscalls PSTATS labels `accept` and
   `unlinkat` (see caveat below), `in_kernel` ≈ 70 s of 72 s. Nothing was typing to it. Nothing
   reaped it.
4. `kill -9` on each live pid; dmesg: only `[signal] pid=1069 ...` and `pid=1071 ...` killed by
   signal 9, and every other pid vanished (group kill took the rest). `pidof` empty afterwards.

## What dmesg shows, and does not

- **Absent:** `[kill] pid=... stale tid=... now owned by pid=...`, `[TRAMP-MISMATCH]`,
  `[exit] slot S row names pid X`, any panic. So **there is no log evidence of a stale-tid
  mis-kill in this run**. The tid-cleanup idea is a hypothesis from the shape of the symptom,
  not something the log shows.
- **Present:** `[threads] new high-water: 107 live user threads (terminated=1 ...)` — one crush
  (Go runtime) is ~100 clone_thread children, each its own pid row (`pid` vs `tgid`,
  `docs/archive/` amd64 clone fold). Those are what `pidof`/PSTATS list as "many crush-bin
  processes"; they are threads of one process, not 5-20 separate orphans.
- `[tty] raw->canonical by pid=902 (/usr/local/bin/crush-bin) via amd64 console TCSETS`: the
  one-shot `crush run` flipped the *console* tty back to canonical. Unexplained here; may be
  unrelated.
- Long `[bkls>]` holds (owner cores rotating) throughout. Not examined.

## Candidate explanations (none tested)

1. **No hangup when the PTY/session closes.** Already a known trap
   (`docs/runbooks/crush-on-akuma.md`: "Closing a pty does not kill its children on Akuma").
   If sshd closes the master and nobody delivers SIGHUP to the foreground group, a TUI that
   ignores read EOF stays up. By itself this does not explain 100 % in-kernel time.
2. **Tid / slot cleanup bug** (the user's guess). The leader dies or is signalled, other
   thread rows keep running because their slot teardown or group-fatal propagation misses
   them. Related, already-open: the stale-tid kill in `docs/README.md`'s symptom table, the
   grace-expired orphan finding (`-j4 "futex stall" was a process with no thread`), and
   `[exit] slot S row names pid X`. A group where `kill -9` of the leader's tid takes the whole
   group, but the group survives the *terminal* going away, is consistent with either.
3. **The pegged threads are a futex/nanosleep spin, not an orphan problem at all.**
   `in_kernel` ≈ 100 % in two syscall buckets while nothing is happening is a busy kernel path
   (epoll one-event-per-call spin, as in `AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`). The orphan is
   how it stays alive; the spin is why it is visible.

**Caveat on the labels.** PSTATS names syscalls from the asm-generic (AArch64) table. On amd64
nr 202 is `futex` and nr 35 is `nanosleep`; the buckets called `accept` and `unlinkat` are
probably those, but that was not checked against the amd64 dispatcher, so read them as
"probably futex / nanosleep".

## Next probes

- Reproduce without ssh: start `crush-glm` in a herdr pane, close the pane, and look at
  `/proc/<pid>/status` (`State`, `PPid`, `Threads`) of the leader and a few thread pids.
- `cat /proc/<tid>/status` for several thread pids of one orphan: is `Tgid` the leader, is the
  leader's row still present? An orphan whose leader row is gone is the cleanup bug.
- Send SIGHUP to the leader by hand: does the group exit? If yes, candidate 1; if no, 2.
- Check `[exit] slot` lines at the exit of a crush that *did* exit cleanly vs one killed.
- Never `kill -9` a pid that has already exited (tid reuse).

## Background

[`docs/runbooks/crush-on-akuma.md`](../runbooks/crush-on-akuma.md) § Traps;
[`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](AMD64_CLAUDE_LOGIN_INPUT_STUCK.md);
[`AMD64_TLB_STUCK_MMAP_REGIONS.md`](AMD64_TLB_STUCK_MMAP_REGIONS.md).
