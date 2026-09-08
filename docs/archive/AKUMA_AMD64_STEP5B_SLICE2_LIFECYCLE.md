# amd64 C1 step 5b, slice 2: identity and lifecycle move to `akuma-exec`

**Date:** 2026-09-08
**Status:** landed for QEMU/TCG (both SMP arms) and Firecracker; the bare-metal
ring-3 check is **blocked on an ssh lockout**, see § "What is not verified".
**Parent:** `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md`, slice 2 —
"delete the `Spawn` table".

---

## What moved

Slice 1 registered a `Process` and left every reader pointed at this target's own
tables. Slice 2 turns the registration into the **authority** for identity and
lifecycle:

| question | answered by, before | answered by, now |
|---|---|---|
| what pid am I? | walk `UserCtx.proc_slot` -> spawn row | `THREAD_PID_MAP` (`pid_for_thread`) |
| who is my parent? | `Spawn::ppid` | `Process::parent_pid` |
| has my child exited, and with what? | `Spawn::exit` | `Process::exited` / `exit_code` |
| what is running as pid N? | spawn row + a synthetic init entry | `find_process` |
| what is in `/proc`? | spawn table | `for_each_process` |
| what is its cmdline? | `Spawn::cmdline` | `ProcessImage::args` |

`Spawn` went from **9 fields to 6**, and the six are not arbitrary — they are
exactly what `akuma_exec::Process` has no home for:

```
pid           the key the row is found by
stdout_pipe   \
stdin_pipe     |  this target's stdio bridge (`crate::pipe`). akuma-exec's
borrowed_io    |  equivalent is `channel` + `stdin`/`stdout: StdioBuffer`,
console_io    /   which is the exec-channel machinery — C2, not this step.
exec_slot     the scheduler task slot, this target's own bookkeeping
```

That is the honest answer to "can slice 2 delete the `Spawn` table": **no**, and
the reason is structural rather than effort. 13 of `exec_runtime.rs`'s 17
`not_wired!` stubs read "C2: fd.rs folds into glue", and the four stdio fields
are the same wall from the other side.

### Threads got an identity, because `current_pid` needed one

`current_pid()` could not move onto `THREAD_PID_MAP` while `clone_thread`
published nothing into it: an unmapped tid answers *init*, not *my process*, so
every `CLONE_VM` thread would have silently become pid 1. Slice 2 therefore
publishes `tid -> tgid pid` at `clone_thread` and removes it at `teardown`, the
same relation AArch64 keeps and next to the `futex::purge_task` that exists for
the identical stale-slot reason.

This is the sort of thing that has to be looked for. The old `current_pid()` was
*correct* for threads by a different route — the per-CPU `UserCtx` carries the
shared `proc_slot` — so nothing would have failed loudly.

## Two things the fold nearly broke silently

- **`ps` would have gone blank.** Slice 1 registered `image.args` as
  `Vec::new()`, which was invisible while nothing read it. Moving the procfs
  readers across without filling it would have emptied every `COMMAND` column.
  The argv is now recorded once, at registration, and `execve` refreshes it —
  `Spawn::cmdline` was the second copy and is deleted.
- **`init_entry()` was not vestigial.** Removing it as "dead now that init is in
  the table" failed three `proc: /proc/self/...` boot checks immediately: the
  self-tests run **before** `run_init` registers pid 1, on a task registered
  nowhere, and `current_pid()` answers 1 for it. It is back as an explicitly
  scoped fallback for that window — with the reason written down, which it had
  not been.

The first was caught by reading, the second by the boot suite. Both are the same
failure mode: a reader moved to a new source before the new source was filled.

## Verification

| rig | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 520 / 0 | **520 / 0** |
| QEMU/TCG `SMP=1` | 511 / 0 | **511 / 0** |
| Firecracker (the box, KVM) | 507 / 0 | **507 / 0** |
| host tests | 1360 / 0 | **1360 / 0** |

No tally moves: slice 2 adds no checks, it changes who answers. Ring-3 over ssh
on QEMU: `ps` renders real commands (`/bin/sshd` for pid 1, where the synthetic
entry used to say `init`), `/proc/1/cmdline` is right, dynamic busybox runs, 20
sessions clean, `ps` steady at 5-6 rows across repeated subshell churn — the
reparenting and the reap both still work.

**`grandfork` still reports `ALL PASS`.** That is the point of having built it
before this refactor: `sys_waitpid` was rewritten onto a different table one
step after being fixed, and the probe that pins the fix is the one that says the
rewrite preserved it.

### What is not verified

The **bare-metal ring-3 check did not run.** The box boots — TCP connects on
2222 and the banner reads `Akuma_0.1`, so the kernel is up, sshd is running and
it has read the disk — but the test key is refused, so no command could be run
and the boot tally could not be read. Recovery is a power cycle (the one-shot
GRUB entry is consumed, so it lands back on Ubuntu).

Two candidates, and they are distinguishable in one command:

1. **Key provenance.** `mkdisk.sh` generates `amd64-ssh-test-key` only `if [ !
   -f ]`, at a path **relative** to the cwd, in a per-machine `target/`
   directory. The box has its own copy. A local image copied at 17:55 today also
   stopped authenticating against the current local private key, so a test key
   was regenerated on at least one side during the session.
2. The known, still-unexplained **signature A** lockout (every key refused with
   a correct `authorized_keys`), recorded in
   `project_amd64_sshd_intermittent_lockout`.

**Check first:** compare `ssh-keygen -lf` on the box's
`target/x86_64-unknown-none/release/amd64-ssh-test-key.pub` against the local
one, from the Ubuntu side after the power cycle. If they differ it is (1), and
signature A is not implicated — which also means one of signature A's past
sightings may have been this instead.

## Background

- `docs/archive/AKUMA_AMD64_STEP5B_SLICE1_REGISTRATION.md` — the registration
  this makes authoritative.
- `docs/archive/AKUMA_AMD64_WAIT4_OWNERSHIP.md` — the bug found between the two
  slices, and the probe that pins this one.
- `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` — slices 3 and 4.
