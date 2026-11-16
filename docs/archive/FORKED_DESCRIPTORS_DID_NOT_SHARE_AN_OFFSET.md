# Forked and dup'd descriptors did not share a file offset (2026-11-16)

**Status: FIXED** for regular files.

## Symptom

`{ /bin/echo one; /bin/echo two; /bin/echo three; } > f` left `f` holding only `three`. Any
shell group or subshell redirected once and running more than one external command kept only the
last command's bytes, silently. Input had the same shape: two `head -c 3` on one redirected file
both read the first three bytes.

Found while testing a first-boot script on an Akuma guest: `{ cat old; echo new; } > tmp`
produced a file the size of `old` with `new` written over its start. The same script passed on
Linux.

## Cause

A descriptor's offset was a plain `usize` in `KernelFile`, and `dup`, `fork` and fd passing copy
the `KernelFile` by value. Each copy therefore had its own cursor. A forked child started at the
offset it was forked with, wrote, and exited; the parent's offset never moved, so the next child
started at the same place. POSIX makes all of these one open file description with one offset.

It was a known divergence, pinned by tests at three layers (`AKUMA_FDTABLE_EXTRACTION.md`
§ "The dup fix"), recorded as a `dup` oddity. Its effect across `fork` was not written down.

## Fix

`KernelFile`'s offset is an `Arc<AtomicUsize>` behind `pos()`, `set_pos()` and `advance()`. A
clone shares the cell, so every copy of a descriptor shares the offset; a second `open` of the
same path makes a new `KernelFile` and a new cell. The fd table itself is unchanged: it still
copies payloads by value, and the wider one-description-per-open design is still not done.

Two concurrent writers through copies of one descriptor now race on one offset (read the
offset, write, advance), as two threads sharing a descriptor always did. Linux serializes that
with a per-description lock; this does not.

## Verification

- Host: `a_dup_shares_the_file_offset` and `a_forked_table_shares_inherited_offsets_only` in
  `akuma-exec`'s fd table tests replace the test that pinned an independent cursor.
- amd64 boot suite (QEMU): "fd: a dup shares the file offset" replaces "a dup's cursor is
  independent"; 832 passed, 0 failed.
- AArch64 QEMU, devbox image: the three-`echo` group writes three lines; `{ cat f; echo tail; }`
  appends; two `head -c 3` on one input read consecutive bytes; gcc, git, rustc, cargo and make
  run; boot suite unchanged.

## Still open

- A directory descriptor's `getdents` snapshot and a block device's position are still per copy.
- The `nonblock` flag is still copied at `dup`, not shared (`fdtable.md`, divergence 2).

## Background

- [`AKUMA_FDTABLE_EXTRACTION.md`](AKUMA_FDTABLE_EXTRACTION.md) § "The dup fix" — the full design.
- [`../reference/subsystems/fdtable.md`](../reference/subsystems/fdtable.md) — current rule.
