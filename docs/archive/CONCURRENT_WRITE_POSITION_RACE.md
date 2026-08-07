# Concurrent `write()` on a shared fd could corrupt data (TOCTOU on file position) — fixed 2026-08-07

## Summary

Two threads sharing a file descriptor (`CLONE_FILES` — any pair of
`clone_thread` siblings, i.e. any `pthread_create`d threads) that called
`write()` on the same fd close together in time could corrupt each other's
output: both could read the same stale file position, both would write to the
same on-disk offset, and whichever write physically committed last would
overwrite bytes the other had just written. No userspace-visible error, no
crash — just silently wrong bytes on disk. Found while root-causing an
unrelated bug ([`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
§7.15), confirmed with a minimal reproduction, and fixed the same session.

This is the kind of bug that would show up as intermittently corrupted build
artifacts — a `.o` file, a linker output, a log file — under any workload
where multiple threads of one process write to a shared fd, which is exactly
the shape of that doc's still-open Failure B ("half-written linker output").
**Not confirmed as the cause of Failure B** — that requires its own
verification — but flagged there as a real candidate now that this exists as
a fix rather than a theory.

## How it was found

While instrumenting a probe for an unrelated hang (barrier/Condvar lost-wake,
see the J4 doc), a run's log file was consistently missing its very first
line of output — not corrupted, not truncated, just **absent**, every single
run, regardless of timing or workload. That's not what a random race looks
like; it's what a **deterministic overwrite at a fixed offset** looks like.

A minimal, dedicated reproduction confirmed it directly: a small Rust program
(`offset_race.rs`, not committed — recreate from the listing below) opens one
file, spawns N threads, and has each do `ITERS` raw `libc::write()` calls of a
fixed-size, thread-tagged block directly on the shared fd — deliberately
bypassing Rust's own `io::Stdout` locking, since the question is entirely
about kernel-side offset handling, not userspace synchronization.

```rust
// N threads, each calling raw write() on the same fd with no userspace lock.
// A clean implementation must produce exactly N*ITERS*LEN bytes, every LEN-byte
// block starting with its own thread's tag and carrying its own iteration number
// intact — any cross-thread byte mixing is a kernel bug, since nothing here can
// race in userspace.
let f = OpenOptions::new().write(true).create(true).truncate(true).open(path).unwrap();
let fd = f.as_raw_fd();
for t in 0..threads {
    thread::spawn(move || {
        for i in 0..ITERS {
            let buf = /* LEN bytes tagged with `t` and `i` */;
            unsafe { libc::write(fd, buf.as_ptr(), LEN) };
        }
    });
}
```

Cross-compiled (`aarch64-unknown-linux-musl`), run on `release-smp-shared` +
`SMP=4`:

```
total_bytes=51200 expected=51200
n_chunks=800 bad_blocks=136 remainder_bytes=0
thread 0 clean_blocks=183 (expected 200)
thread 1 clean_blocks=173 (expected 200)
thread 2 clean_blocks=166 (expected 200)
thread 3 clean_blocks=142 (expected 200)
RESULT: FAIL
```

136 of 800 64-byte blocks (17%) came back with mixed content from more than
one thread. Note `total_bytes` matched exactly — the corruption is torn
*content* at otherwise-correctly-sized offsets, consistent with two threads'
reservations partially overlapping rather than the file ending up shorter (as
a full duplicate-offset collision would produce).

## Root cause

`sys_write` (`src/syscall/fs.rs`) handled a `File` fd's position in three
separate steps, only the first and third of which were lock-protected:

1. `proc.get_fd(fd_num)` — locks `SharedFdTable`'s `Spinlock<BTreeMap<...>>`,
   **clones** the entry (including `.position`), unlocks. (`Process::get_fd`,
   `crates/akuma-exec/src/process/fd.rs`.)
2. Performs the actual disk write via `crate::fs::write_at(path, write_pos,
   buf)` using that cloned, now-detached `write_pos` — **no lock held**.
3. `proc.update_fd(fd_num, |entry| entry.position += n)` — locks the table
   again, writes the new position back, unlocks.

Each individual step is correctly synchronized. The *sequence* is not: the
lock is released between steps 1 and 3, and the actual I/O — the slow part —
happens entirely outside it. Two threads sharing this fd racing `write()`
could both complete step 1 (reading the same, not-yet-advanced `.position`)
before either reached step 3. Both would then write to the same on-disk
offset in step 2, and whichever `write_at` call physically landed last would
overwrite the other's bytes at the overlap.

This is a textbook TOCTOU gap: a lock protects each access to a data
structure, not an invariant that spans multiple accesses with unguarded work
in between. `O_APPEND` writes have a related, **still-open** issue — they
derive their position from `crate::fs::file_size(path)` fresh on every write
rather than from `.position` at all, which has the same shape of race
(two threads can both read the same file size before either extends it) and
is not fixed here; see "Not fixed" below.

## The fix

Added `SharedFdTable::reserve_write_pos` (`crates/akuma-exec/src/process/fd.rs`,
with `Process::reserve_write_pos` as a thin delegator so `sys_write` calls it
the same way it calls every other per-fd `Process` method), which reads the
current position **and** advances it past the caller's reservation in one
lock hold — closing the gap between "read" and "write-back" instead of
leaving disk I/O in the middle of it:

```rust
pub fn reserve_write_pos(&self, fd: u32, count: usize) -> Option<usize> {
    with_irqs_disabled(|| {
        let mut table = self.table.lock();
        match table.get_mut(&fd) {
            Some(FileDescriptor::File(file))
                if file.flags & open_flags::O_APPEND == 0 =>
            {
                let pos = file.position;
                file.position = pos.saturating_add(count);
                Some(pos)
            }
            _ => None,
        }
    })
}
```

`sys_write` now calls this **once**, for the full requested `count`, before
its chunking loop starts (it was previously reading `.position` directly from
the `get_fd()` clone). The per-chunk loop no longer writes `.position` back
at all for the plain (non-`O_APPEND`) case — the reservation already
accounted for the entire `count` up front, so doing it again per chunk would
double-advance the position and skip bytes on the next writer.

One deliberate asymmetry: a short or failed write must **never** roll the
shared position back to "give back" an unused tail. By the time a short write
is discovered, another thread may already have reserved the range
immediately after this one's *full* reservation — rewinding would hand that
range out a second time, reopening exactly this race. The correct on-disk
result of a short write here is a sparse hole (the reserved-but-unwritten
tail), not a rewound position. Only `O_APPEND` still writes `.position` back
per chunk, matching its pre-existing (unfixed) behavior exactly.

## Verification

Same probe, same repro command, rebuilt kernel:

```
total_bytes=51200 expected=51200
n_chunks=800 bad_blocks=0 remainder_bytes=0
thread 0 clean_blocks=200 (expected 200)
thread 1 clean_blocks=200 (expected 200)
thread 2 clean_blocks=200 (expected 200)
thread 3 clean_blocks=200 (expected 200)
RESULT: PASS
```

Also re-ran with 8 threads (`total_bytes=102400`, `bad_blocks=0`, all 8
threads' blocks clean) to check the fix isn't thread-count-sensitive.

**End-to-end regression check, real workload:** HTTPS download via `curl` to
a file (exercises the plain, non-append `File` write path this fix changed)
— md5 matched a host-side download of the same URL bit for bit, both
single-threaded and with 4 concurrent `curl` processes each writing their own
file. Confirms the fix doesn't disturb the overwhelmingly common
single-writer-per-fd case; TLS/HTTPS itself is unaffected since that traffic
goes through the `Socket` fd arm, untouched by this change.

Host unit tests (`cargo test -p akuma-exec --target <host-triple>`): all 179
passed, 0 failed. Five of those are dedicated regression coverage for this
fix, added the same session (`crates/akuma-exec/src/process/fd.rs`,
`reserve_write_pos_tests`): sequential non-overlapping reservations, the
`O_APPEND` refusal, missing/non-`File` fd handling, a zero-length no-op, and
— the one that actually exercises the bug's shape — 16 real `std::thread`s
each issuing 500 concurrent `reserve_write_pos` calls with no coordination
beyond the function itself, asserting the resulting 8,000 reservations tile
the file exactly (sorted starts form an exact arithmetic progression: no
repeats, i.e. no overlap; no gaps, i.e. nothing lost). This is a logic-level
regression test for `reserve_write_pos` itself, run entirely on the host
without a VM boot; it complements, but does not replace, the raw-`write()`
guest probe above, which is what actually proved the original *bytes on
disk* were corrupted and confirmed the fix closes that specific path through
`sys_write`.

## Not fixed (left for a future session)

**`O_APPEND` has the same shape of race, untouched here.** Every `O_APPEND`
write calls `crate::fs::file_size(&f.path)` fresh to find its start offset,
with no lock spanning "read size" and "write there" — two threads (or two
processes with independent fds on the same file) can read the same size
before either has extended the file, and race exactly like the bug above.
Fixing this needs FS-level serialization (the current-size read has to be
atomic with the extend), not just an fd-table-level fix like
`reserve_write_pos` — `O_APPEND` across genuinely independent file
descriptions (different opens, possibly different processes) can't be solved
by anything scoped to one process's `SharedFdTable`. Out of scope here;
tracked as a known gap.

**Whether this explains Failure B is unconfirmed.** The corruption shape is
suggestive (this doc's own missing-first-line symptom looks a lot like
"half-written output"), but Failure B's actual reproduction
(`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §4) involves a
linker/compiler process, and it hasn't been checked whether that process's
output-writing threads (if any — `ld`/`cc` may or may not be multi-threaded
in the relevant window) actually share an fd across threads the way this
reproduction does. Next session chasing Failure B should check that before
assuming this fix closes it.

## Background

- [`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md) —
  the investigation this was found during (§7.15); its own Failure B is the
  most likely beneficiary if this turns out to be related.
- [`SHARED_FD_TABLES.md`](SHARED_FD_TABLES.md) — why fd tables are shared
  (`CLONE_FILES`) in the first place, the precondition for this race to be
  reachable at all.
- [`WRITE_AT_SYSCALL.md`](WRITE_AT_SYSCALL.md) — the `write_at` I/O path this
  bug's racing `write_pos` was handed to; unrelated bug, same syscall.
