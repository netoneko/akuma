# `ps` / `ls /proc` never return on a long-running Akuma

**Date:** 2026-10-15. **Status:** OPEN — observed, not root-caused, not reproduced
on a fresh boot. **Where:** amd64 trashcan (`192.168.1.120`), kernel
`0.0.8 35f1a72b-release-smp-shared` (2026-10-14, 8 commits behind `ugh` HEAD),
uptime 22 h 12 min, pid 1 = `herd`.

## Symptom

Anything that **enumerates `/proc`** spins forever and has to be `^C`'d. It does
not fail and it does not print an end-of-directory; it burns CPU (`ls /proc` was
at 0:15 CPU after ~15 s wall, and still climbing).

| command (over ssh on :2222) | result |
|---|---|
| `echo hi`, `uptime`, `ls /tmp`, `ls /proc/1` | fine |
| `cat /proc/1/stat`, `cat /proc/self/stat` | fine — `1 (herd) R …`, 44 fields |
| `ls /proc` | **never returns**, 0 bytes of output reached a pipe |
| `timeout 3 ls -1 /proc \| head` | `Terminated`, no lines — it does not even emit the first batch |
| `ps` | prints the first rows (`kot run`, the shell, itself), then **never exits** |
| `ls -d /proc/[0-9]*` | never returns (glob enumerates `/proc`) |
| `pkill` | never returns (enumerates `/proc`) |

So per-pid reads work and the **directory listing of `/proc` itself does not
terminate**. The first row of `ps` appearing says the listing produces entries;
`ls`'s empty pipe says nothing is flushed before the loop. Consistent with a
`getdents64` on `/proc` that never returns 0 (a cursor that does not advance, or
a cache that is rebuilt on each call).

## What it is NOT

- **Not simply an old kernel.** `git log 35f1a72b..HEAD` touches none of
  `crates/akuma-procfs`, `crates/akuma-vfs-glue` or `akuma-syscalls-glue/src/fs.rs`
  except `0d58f739` (symlinks in the middle of a path), which would not cause
  this. Assume HEAD has it until a fresh boot proves otherwise.
- **Not the 41-vs-44 stat-field bug** (`AKUMA_AMD64_STREAMLINING.md` §4b);
  `/proc/<pid>/stat` is the right shape here.

## Hypothesis: it scales with uptime

The user's reading is that this shows up on **long-running** boxes. Plausible
mechanisms to check, in order of cost:

1. the `/proc` directory cache (`dir_cache` in `amd64/src/fd.rs` header) grows
   with every process ever spawned and is re-walked per call, so a getdents
   buffer that never fits makes the caller re-ask forever;
2. a stale pid-table row (a slot freed but still listed) whose entry never
   renders, so the cursor stalls on it — the table has leaked before
   (`project_amd64_unregistered_hook_tables`: no slot recycler);
3. the listing is sized from a pid count that has overflowed a fixed buffer.

## Next steps

1. On the next fresh boot, run `ps; ls /proc` at t=0 and again every few hours;
   record the uptime and process-churn count at which it first hangs.
2. On a hung box, read `/proc/1/..` is fine, so use `getdents64` directly
   (a 20-line C probe under `userspace/forktest/c_stress/`): print each call's
   returned byte count and the last `d_off`. A non-advancing `d_off` confirms
   (1)/(2).
3. Add the probe to the gate once root-caused.

## Collateral

My diagnostic probes spawned `ls /proc`, `ps` and a `/proc/[0-9]*` glob on the
trashcan; their ssh clients timed out but the **server-side processes keep
spinning** and cannot be killed by name (`pkill` also enumerates `/proc`). Kill
them by pid from the console, or they go with the next reboot.

## Background

- [`AKUMA_AMD64_STREAMLINING.md`](AKUMA_AMD64_STREAMLINING.md) §4b — the
  `/proc/<pid>/stat` formats and `ps`'s walk.
- [`AKUMA_GENTOO.md`](AKUMA_GENTOO.md) — where this was found (2026-10-15).
