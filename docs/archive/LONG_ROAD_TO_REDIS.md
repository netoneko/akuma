# The long road to Redis

> **Fixed, and superseded for what happened next (2026-08-16).** Both fixes in
> §5 landed: `MADV_FREE` returns `EINVAL` (§5.1) and `/proc/<pid>/{cmdline,
> status,stat}` exist (a partial §5.2 — `smaps` still does not). `redis-server`
> starts. It then turned out that no client on the same box could *reach* it,
> for two unrelated reasons in `crates/akuma-net/src/socket.rs`; with those
> fixed the official `redis:alpine` image runs in a box.
> See [`REDIS_END_TO_END.md`](REDIS_END_TO_END.md) and the runbook
> [`../runbooks/run-redis.md`](../runbooks/run-redis.md).
>
> One conclusion below is worth revisiting rather than trusting: §3.4 reads the
> gap as "`/proc/self/` is empty" and §5.2 proposes adding the missing *files*.
> The deeper cause was that **`/proc/self/<anything>` did not resolve at all** —
> the VFS never chased the `self` symlink, so procfs saw the literal string
> `self/status`. Adding files papered over it for the paths that were added;
> `/proc/self/smaps` would have failed even once written. Fixed 2026-08-16
> (`resolve_self` in `src/vfs/proc.rs`), and it cost another hour four days
> later — see [`REDIS_END_TO_END.md`](REDIS_END_TO_END.md) §4.

**Date:** 2026-08-12
**Status:** root-caused, fixed — see the note above
**Short version:** `redis-server` refuses to start because `/proc/self/smaps`
does not exist. It is not a CoW bug and it is not `madvise`, though both were
plausible and both were wrong.

---

## 1. Symptom

```
~ # redis-server --port 8080
1813:C 12 Aug 2026 20:21:57.656 # Failed to test the kernel for a bug that could lead to
  data corruption during background save. Your system could be affected, please report this error.
1813:C 12 Aug 2026 20:21:57.656 # Redis will now exit to prevent data corruption. Note that it is
  possible to suppress this warning by setting the following config: ignore-warnings ARM64-COW-BUG
```

`redis-cli` loads fine and `redis-server memtest` passes. Only the startup
self-check fails.

## 2. Two wrong hypotheses

Both were reasonable. Both cost time. Recording them because the *reason* each
was wrong is the useful part.

### "It's a CoW bug" — wrong

The warning names `ARM64-COW-BUG`, Redis forks to snapshot, and Akuma has a long
CoW bug history. Natural conclusion, and it would have put the fix behind the
highest-risk consolidation work in the tree (`mmu/mod.rs`).

**Why it is wrong:** Redis prints *two different messages*. Reading
`src/server.c:7449`:

```c
if ((ret = checkLinuxMadvFreeForkBug(&err_msg)) <= 0) {
    if (ret < 0) {
        serverLog(LL_WARNING, "WARNING %s", err_msg);   /* CoW bug DETECTED */
    } else
        serverLog(LL_WARNING, "Failed to test the kernel for a bug ...");  /* ret == 0 */
    if (!checkIgnoreWarning("ARM64-COW-BUG")) { ... exit(1); }
}
```

The return convention is inverted from intuition: **`>0` healthy, `<0` bug
found, `0` could not test.** We get the "Failed to test" branch, so `ret == 0`:
Redis never reached an opinion about CoW at all.

### "It's `MADV_FREE`" — also wrong, but instructive

`src/syscall/mem.rs` contains:

```rust
MADV_FREE => 0,
_ => 0,
```

A silent success no-op, indistinguishable from the catch-all — while
`MADV_DONTNEED` immediately above carries a 40-line divergence audit with atomic
counters. An obvious suspect.

**Why it is wrong as a root cause:** the probe does call `madvise(MADV_FREE)`,
and Akuma returns 0, and the probe carries on happily. `madvise` is not what
fails.

**Why it still matters:** it is what denies Redis a graceful exit —

```c
ret = madvise(q, page_size, MADV_FREE);
if (ret < 0) {
    /* MADV_FREE is not available on older kernels that are presumably not affected. */
    if (errno == EINVAL) goto exit;      /* res stays 1 -> redis STARTS */
    res = 0;
    goto exit;
}
```

**Returning `EINVAL` would make Redis start.** By claiming success, Akuma asserts
"I am a modern kernel that implements MADV_FREE", and Redis proceeds to the
follow-up question it cannot answer.

## 3. Investigation

### 3.1 The syscall log was too small

Akuma exposes a per-process syscall log at `/proc/<pid>/syscalls`
(`src/syscall/log.rs`, enabled by `config::PROC_SYSCALL_LOG_ENABLED`). Capturing
it around the failure:

```sh
redis-server --port 8080 > /tmp/redis.out 2>&1 &
P=$!; wait $P; cat /proc/$P/syscalls
```

The retained window (last 64 entries — `PROC_SYSCALL_LOG_MAX_ENTRIES`) showed:

```
 TIMESTAMP_US       NR  DUR_US  RESULT
     677012041     220   13421      29     <- clone -> child pid 29
     677012046      63      51       4     <- read  -> 4 bytes
     677012098     260    1034      29     <- wait4 -> reaped 29
     677013232      29       1   ...591    <- ioctl -> -25 (ENOTTY), harmless isatty
     ...
     677014441     215       6       0     <- 30+ munmap, teardown
```

Two conclusions: the fork **and** the pipe read both succeeded, so neither was
the failure; and the interesting calls (`mmap`, `mprotect`, `madvise`) had
already scrolled out of a 64-entry ring. `NR 29 -> -25` is `ioctl` returning
`ENOTTY` — an `isatty()` probe, a red herring.

**Lesson:** 64 entries is too short to diagnose a process that dies during
startup. Enlarging the ring would have meant rebuilding a tree another agent was
actively editing, so the next step was a targeted reproducer instead.

### 3.2 The first reproducer passed — which was the real clue

A C reproducer written from memory of `linuxMadvFreeForkBugCheck()` ran clean and
reported "Redis starts normally". Since real Redis does not, the reproduction was
wrong, not the kernel.

**Cause:** the function was renamed and rewritten. Older Redis had
`linuxMadvFreeForkBugCheck()` in `server.c`; Redis 8.x has
**`checkLinuxMadvFreeForkBug()` in `src/syscheck.c:209`**, with different
internals and the inverted return convention above. Reciting an upstream function
from memory is not a substitute for reading it.

### 3.3 Reading the real source

Fetched inside the VM (box 0 has working DNS/HTTPS over smoltcp):

```sh
cd /tmp && curl -sL https://github.com/redis/redis/archive/refs/tags/8.0.0.tar.gz -o r.tgz
busybox tar -xzf r.tgz            # GNU-tar arg order differs; busybox tar works
grep -rn 'checkLinuxMadvFreeForkBug' /tmp/redis-8.0.0/src/
```

The decisive part of `syscheck.c`, in the child after the fork:

```c
} else if (!pid) {
    /* Child: check if the page is marked as dirty, page_size in kb.
     * A value of 0 means the kernel is affected by the bug. */
    ret = smapsGetSharedDirty((unsigned long) q);
    if (!ret)        res = -1;   /* bug */
    else if (ret == -1) res = 0; /* failed to read  <== our case */
    ret = write(pipefd[1], &res, sizeof(res));
    exit(0);
}
```

and `smapsGetSharedDirty()` (same file, ~line 175):

```c
f = fopen("/proc/self/smaps", "r");
if (!f) return -1;
```

### 3.4 The gap

```
$ for f in maps smaps status stat statm cmdline fd; do ... done
maps     MISSING
smaps    MISSING
status   MISSING
stat     MISSING
statm    MISSING
cmdline  MISSING
fd       MISSING
```

`/proc/self/` is empty. Not just `smaps` — the whole per-process directory.

## 4. Verified chain

A corrected reproducer, transcribed from `syscheck.c` rather than memory and
instrumented at every step, run in the VM:

```
mmap ok p=0x20030000
mprotect ret=0 errno=0
madvise(MADV_FREE) ret=0 errno=0 (-)
  [child] fopen(/proc/self/smaps) FAILED errno=2 (No such file or directory)
  [child] smapsGetSharedDirty=-1
  [child] reporting res=0
parent read ret=4 res=0

RESULT res=0 -> redis prints 'Failed to test' and EXITS
```

Every step matches the real failure. Root cause confirmed.

## 5. Fixes

Neither is in `mmu/mod.rs`. **Redis is not blocked on the CoW/page-table
consolidation work** and should not be scheduled behind it.

### 5.1 Unblock — one line, `src/syscall/mem.rs`

Return `EINVAL` for `MADV_FREE` instead of a fabricated `0`. Honest: Akuma does
not implement MADV_FREE, and Redis explicitly treats EINVAL as "older kernel,
presumably not affected" and starts.

**Caveat, do not flip blindly.** Allocators (jemalloc, musl) probe `MADV_FREE`
and fall back to `MADV_DONTNEED` on EINVAL. Akuma's `MADV_DONTNEED` diverges from
Linux — it zeroes the *physical frame* in place where Linux drops the *mapping*
(the audit block at `src/syscall/mem.rs:87` and its counters exist for exactly
this reason). Redirecting every allocator onto that path is a real behavioural
change and wants its own testing.

### 5.2 Proper — `src/vfs/proc.rs`

Implement `/proc/<pid>/smaps`. **A stub is worse than nothing here**: if it
reports `Shared_Dirty: 0`, Redis takes `if (!ret) res = -1` and announces "Your
kernel has a bug that could lead to data corruption", then exits anyway — a worse
outcome than the current message.

A correct implementation must emit, for the VMA containing the queried address:

- the `from-to` range line `smapsGetSharedDirty` matches with `%lx-%lx`, and
- a `Shared_Dirty:` line with a genuine non-zero kilobyte count for a CoW page
  the child has faulted.

That means real shared-and-dirty accounting, not a constant. `COW_REFCOUNTS`
already tracks sharing; the dirty half comes from the PTE state. This is the one
place where the investigation does touch CoW — just not the way the original
hypothesis assumed.

While in there, `maps`, `status`, `statm` and `cmdline` are missing too and are
cheap by comparison.

## 6. Reproducing

```sh
# devbox.img may be write-locked by another running VM. SNAPSHOT=1 opens the
# backing file read-only and discards writes, so it cannot clash or corrupt.
DISK=devbox.img MEMORY=4096 SMP=4 SNAPSHOT=1 INSTANCE=0 \
  cargo run --release --features devbox-smoltcp,no-tests
```

Then over SSH on port 2222:

```sh
apk add redis          # redis is NOT in devbox.img on disk
redis-server --port 8080
```

Note `apk add redis` reports two `applet not found` errors from its pre/post
install scripts; the binaries install correctly regardless.

### The probe

The instrumented reproducer now ships as a permanent control binary:
**`userspace/forktest/c_stress/smapsdirty.c`** → `/bin/smapsdirty`, built
unconditionally by `userspace/build.sh` and copied to `bootstrap/bin/`. No redis
install needed:

```
~ # smapsdirty
smaps-present                FAIL  fopen(/proc/self/smaps) errno=2 (No such file or directory)
proc-self-files              FAIL  5 missing: maps status stat statm cmdline
madv-free-accepted           PASS  ret=0 errno=0 (-)
redis-arm64-cow-check        FAIL  res=0 (child could not read /proc/self/smaps) -> redis EXITS
```

Per the `c_stress` convention, **calibrate on real Linux before believing a
FAIL** — every failure there means the probe is wrong, not the kernel:

```sh
docker run --rm --platform linux/arm64 \
  -v "$PWD/smapsdirty:/smapsdirty:ro" alpine /smapsdirty     # expect 4 PASS
```

It is also the regression check for both fixes in §5: fix 5.1 flips probe 4 to
PASS via the `EINVAL` path; fix 5.2 flips probes 1, 2 and 4.

## 7. What this exposed beyond Redis

- **`/proc/<pid>/` is empty.** Any tool that reads `maps`, `status`, `statm`,
  `cmdline` or `fd` will misbehave. Redis is the first to fail loudly.
- **`MADV_FREE` lies.** Returning success for advice that is ignored is legal
  under POSIX, but it defeats capability probes — software uses the *observable*
  behaviour of MADV_FREE to decide what kind of kernel it is on.
- **The syscall-log ring (64 entries) cannot diagnose startup failures.** Worth
  raising, or making the depth configurable, before the next one of these.

## 8. Method notes

Three things generalise:

1. **Read the upstream source; do not recite it.** The first reproducer passed
   because it was written from memory of a function that had been renamed and
   rewritten. Fetching `syscheck.c` took two minutes and settled it.
2. **A reproducer that disagrees with reality is a finding**, not a dead end — it
   proved the model was wrong while there was still time to be cheap about it.
3. **Distinguish the error messages a program can emit before theorising.** The
   entire CoW hypothesis collapsed on noticing that Redis has a *different*
   message for a detected CoW bug.

---

## Background

- `src/syscall/mem.rs:87` — the `MADV_DONTNEED` divergence audit and counters,
  relevant to fix 5.1's caveat.
- `src/vfs/proc.rs` — where `/proc/<pid>/smaps` would live.
- `src/syscall/log.rs`, `config::PROC_SYSCALL_LOG_*` — the per-process syscall
  log used in §3.1.
- [`MADVISE_WILLNEED_FILE_CORRUPTION.md`](MADVISE_WILLNEED_FILE_CORRUPTION.md) —
  the sibling madvise gaps (WILLNEED fixed, DONTNEED divergence latent), if
  present in this archive.
- Upstream: `redis/redis` `src/syscheck.c` (`checkLinuxMadvFreeForkBug`,
  `smapsGetSharedDirty`) and `src/server.c` ~line 7449 for the call site. The
  kernel bug being probed for is Linux commit `ff1712f953e2`, "arm64: pgtable:
  Ensure dirty bit is preserved across pte_wrprotect()".
