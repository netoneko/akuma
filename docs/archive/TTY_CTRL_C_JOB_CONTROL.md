# `^C` could not interrupt a job-control shell's foreground job (2026-10-19)

**Status: FIXED 2026-10-19** (`42643bd0`). The current-state rule is in
[`docs/reference/subsystems/tty.md`](../reference/subsystems/tty.md) § "`^C` and job control";
this file is the incident record.

## Symptom

`^C` at `sleep 100`, run from `sh -i` (busybox) over ssh, did nothing on either architecture and
before any of the akuma-tty work. The kernel logged `[ISIG] … members=0`.

## Cause

`kill_process_group` always spared the group leader, on the assumption that the foreground group
is the login shell's own (the leader being the shell that must survive its child's `^C`). A
job-control shell does the opposite: it gives each job its own process group and `TIOCSPGRP`s to
it, so the foreground group's leader **is the job**, and nothing was left to signal.

## Fix

`TerminalState` records the session shell's group (`session_pgid`, set by `set_session_leader`
where `foreground_pgid` used to be assigned). `spares_leader()` is true only while the foreground
group is that one. `akuma_exec` gains `kill_foreground_group`, used by the write-time `^C` path
and the read-time `ISIG` signals. `PtyPair`'s signal path never had the exclusion and is
unchanged.

## Verification

Live on AArch64 (Lima) and amd64 (Firecracker on p3): `sleep` exits 130; `^C` at a nested shell's
prompt leaves that shell alive; the outer shell survives. Boot suites unchanged (AArch64 300
PASSED plus the known epoll-latency failure; amd64 804 passed, 0 failed).

## Background

- [`docs/reference/subsystems/tty.md`](../reference/subsystems/tty.md) — the terminal stack
  (`akuma-tty`) this rule lives in.
- [`AKUMA_TTY_EXTRACTION.md`](AKUMA_TTY_EXTRACTION.md) — the extraction that made the one line
  discipline.
