# Shared spawn parked each child's channel under tid 0, which is amd64's boot task (2026-11-12)

**Status: fixed in the tree (`crates/akuma-exec/src/process/spawn.rs`), verified by the amd64
boot suite. Found while moving the terminal ioctls to glue.**

## Symptom

After amd64's `ioctl` started sending registered processes to glue's `term::sys_ioctl`, two boot
checks failed at every SMP count:

```
  winsize: TIOCGWINSZ on a console fd succeeds   [FAIL] got 0xffffffffffffffe7 want 0x0
  winsize: and reports the tracked size, not a compiled-in 24x80   [FAIL] got 0x0 want 0x320084
```

`-25` is `ENOTTY`. The caller is the boot row (pid 1, registered by `fd::boot_row_register`), with
fd 0 bound to `Stdin` and the console's channel registered for its thread.

## Reading it

A temporary print in both `ioctl`s showed fd 0 bound to `Stdin` and glue seeing the same pid. The
difference was the channel: `current_channel()` returned a channel with `is_terminal() == false`
right after the test had spawned `/bin/hello`, and `None` at an earlier call. The boot row had
registered the console's channel, which is a terminal.

## Cause

`spawn_process_named` registered the new child's channel under a **placeholder tid 0**
(`register_channel(0, ..)`), then, on the child's thread, `remove_channel(0)` and
`register_channel(tid, ..)`. Tid 0 is a real thread: amd64's boot task, the one the suite runs
on. Every spawn from it replaced its console channel with the child's (not a terminal), and the
child's first run then deleted that entry. Nothing ever read the placeholder: `current_channel()`
consults the `Process` field first, which is what the placeholder's own comment said
("mostly for legacy").

amd64's own `console_ioctl` answered fd 0 without asking for a channel, so the clobber was
invisible until glue's terminal gate (`is_terminal()` on the current channel) served the boot row.
On the AArch64 kernel tid 0 is the boot thread too; nothing there registers a channel for it, so
nothing observed the overwrite. Two concurrent spawns also raced on that slot.

## Fix

Both lines removed. The child's channel is registered once, under the child's own tid, on its
thread.

## Verified

amd64 boot suite at SMP 1/2/4 with the glue ioctl path: 829/839/839, 0 failed, both checks
included. AArch64 HVF suite unchanged (312/320/318).
