# amd64 C1 step 5b, slice 3: `/proc` is a real mount

**Date:** 2026-09-08
**Status:** landed. QEMU/TCG 521/0, Firecracker 508/0, bare metal 512/0, host
1360/0.
**Parent:** `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md`, slice 3.

---

## What landed

`akuma-vfs-glue`'s `ProcFilesystem` is mounted at `/proc`. It compiled for this
target since C1 step 4a and was deliberately left unmounted, with the reason
recorded in `amd64/Cargo.toml`: it renders from `akuma-exec`'s process table,
and this target registered nothing there, so mounting it *would have replaced a
working synthetic `/proc` with a report of an empty machine*. Slices 1-2 spent
that reason.

`/proc` is now a **union**, not a swap. `fd.rs` answers what it has a
target-specific source for; anything it does not serve falls through to the
mount, where it used to return `ENOENT`. The fall-through is at the same point
in all three of `sys_openat`, `sys_newfstatat` and `sys_access`, which is what
keeps them agreeing — an invariant this target already has a self-test for.

New on this target, from the mount:

```
/proc/uptime      25.07 25.07
/proc/loadavg     0.00 0.00 0.00 2/2 0
/proc/stat        cpu  0 0 0 21167 0 0 0 0 0 0
/proc/<pid>/mounts, /proc/<pid>/fd/
```

Unchanged, and still served here: `meminfo`, `mounts`, `net/dev`, per-pid
`stat`/`status`/`cmdline`, `self/maps`, `self/statm`, and the
`/proc/<pid>/fd/0` stdin bridge.

## What did not move, and why each one didn't

This is the useful half of the slice, because "mount it and delete `fd.rs`'s
`/proc`" is what the plan said and it is not what the code allows.

- **`meminfo`, `mounts`, `net/dev`** read *this* target's PMM, mount table and
  interface list. The crate's same-named files read the AArch64 kernel's.
  **A shared format is not a shared source** — `akuma-procfs` and
  `akuma-syscalls-net` already guarantee the bytes; what differs is who is
  asked. `render_meminfo` in particular carries a hard-won comment about every
  field having to be present or `free` underflows its `used` column.
- **`/proc/<pid>/fd/0`** is `sshd`'s stdin bridge onto `crate::pipe`. That is
  C2, the same wall the four `Spawn` stdio fields sit behind.
- **`self/maps`, `self/statm`** have no equivalent in the crate's procfs at all.
  Note their `SELF_ONLY` restriction now has a **stale justification**: the
  comment says the two files describe an address space and "nothing joins"
  `PROCS` (slot-keyed) with the spawn table (pid-keyed). Slices 1-2 built that
  join — `THREAD_PID_MAP` plus `Process::address_space` — so serving them for
  *any* pid is now possible. Left as it is here because it is a capability
  change, not a fold.
- **per-pid `stat`/`status`/`cmdline`** — tried, reverted, and this one is worth
  the paragraph:

### The boot suite runs before `run_init`

Routing those three to the mount failed `proc: open/stat/access agree on
/proc/self/{stat,status,cmdline}` immediately, all three at once. The suite
executes on a task registered in no process table, and `current_pid()` answers 1
for it; `fd.rs` can answer for that window because `proc_by_pid` carries an
explicit pid-1 fallback, and the mounted filesystem cannot, because it reads the
table and the table is still empty.

That is the **second** time this window has bitten in two slices — it also
killed the attempt to delete `init_entry()` in slice 2. It is worth stating as a
rule rather than rediscovering a third time:

> Anything that reasons "init is in the process table now" is wrong during the
> boot self-tests. Either keep a fallback for that window, or register pid 1
> before the suite runs.

Closing it properly means the latter, which is its own change: `run_init`
registers pid 1 too and would then be re-registering rather than creating.

So the duplication that remains is duplication, not divergence: one source
(`akuma-exec`'s table), one format crate (`akuma-procfs`), two callers.

## Tallies

| rig | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 520 / 0 | **521 / 0** |
| Firecracker (the box, KVM) | 507 / 0 | **508 / 0** |
| bare metal `SMP=4` | 511 / 0 | **512 / 0** |
| host tests | 1360 / 0 | **1360 / 0** |

`+1` everywhere: `mount: /proc is mounted as proc`. The existing
`mount: exactly one mount after boot` became `exactly two` — it caught the new
mount on the boot it landed, which is what that check is for. It now also
asserts the *type* column, because a mount recorded as the wrong type still
resolves paths and still lists in `df`.

Bare-metal ring-3: 40/40 subshell sessions, `used` 1050971 -> 1048708, `ps`
steady at 5 rows, `uptime`/`loadavg` live, `free`/`df`/`ps`/`maps`/`net/dev` all
unchanged.

### A harness trap, again

The Firecracker arm first reported 507 — the *previous* kernel. `hpbox.firecracker`
boots `BOX_KERNEL`, which only `hpbox.build()` refreshes; `deploy()` syncs source
and does not compile. A tally that fails to move is the tell, and it is the same
stale-artifact shape `docs/archive/AB_STALE_BAKED_ARTIFACTS.md` records.

## Background

- `docs/archive/AKUMA_AMD64_STEP5B_SLICE2_LIFECYCLE.md` — the slice that filled
  the table this mount reads.
- `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` — slice 4.
