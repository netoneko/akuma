# ext2: one file's data written into another file's blocks

**Date:** 2026-10-05. **Found:** bringing up rio on the trashcan panel
(`AKUMA_AMD64_RIO_FBDEV_BUILD.md`), while iterating on a rio config in a
throwaway `HOME`. **Status: OPEN — observed once, not reproduced yet.** A
second, *reproducible* defect turned up while trying: concurrent `O_APPEND`
writers lose writes (§5).

## 1. What was seen

A freshly written rio config, `/tmp/crt-home/.config/rio/config.toml`
(inode 48381), read back as rio's debug trace — the lines rio appends to
`/tmp/akuma-fb.log` — instead of TOML:

```
$ head -c 300 /tmp/crt-home/.config/rio/config.toml
EventLoop::new done (screen + tty ok)
event loop entered, tty opened
handler NewEvents enter
handler NewEvents exit
...
[pty] reactor up: channel=Token(0) read=Token(1) write=Token(1) child=Token(2)
```

* `wc -c` / `stat` size: **2243 bytes** (about the size of the config that
  was written), but `grep -c ""` counted **1391 lines** — far more data than
  2243 bytes can hold. The read path returned bytes past the inode's size, or
  the size reported and the blocks read disagree.
* `stat` reported 5 blocks for it.
* The log that should have received those lines,
  `/tmp/akuma-fb.log` (inode 48373, later renamed to `.old`), was only
  **880 bytes**, although that rio run wrote ~1400 trace lines.
* The two files are different inodes (48381 vs 48373), so it is not two
  names for one inode: the **log's appends landed in data blocks the config
  already owned**.
* Other files checked were intact, including the real
  `/root/.config/rio/config.toml` (inode 48370) written minutes before.

rio then ran without its config (it parsed garbage, used defaults), which is
how it was noticed.

## 2. The sequence that produced it

All on `/` (`/dev/sda1`, ext2), one ssh command at a time, nothing else
writing to `/tmp` except rio:

1. `cat > /tmp/crt-home/.config/rio/config.toml` (overwrite an existing file:
   `O_TRUNC` frees its blocks, the write allocates new ones), then
   `: > /tmp/akuma-fb.log` (truncate the log, freeing its blocks).
2. Deploy step: `wget` a 29 MB binary to `/tmp/rio-bin.new`, `mv` over
   `/tmp/rio-bin`, then `: > /tmp/akuma-fb.log` again (truncate the already
   empty log).
3. Run rio with `HOME=/tmp/crt-home`. rio appends to `/tmp/akuma-fb.log`
   **from two threads concurrently** (the fb event loop and the pty reactor),
   each line an `open(O_CREAT|O_APPEND)` + one `write` + `close`.
4. The config now holds the trace.

The same sequence ran several times earlier in the session without
corruption; the difference in this round was only timing.

## 3. Why the allocator makes this cheap to hit

`crates/akuma-ext2/src/ext2.rs`, `free_block`:

* clears the bitmap bit **and pulls the scan hint back** to it — "immediate
  reuse of deleted-file space": the next allocation in that group gets
  exactly the block that was just freed;
* drops any cached copy without flushing (invalidate-on-free, design D-3).

So any path that still holds a block number after the block was freed — or
any lost update that leaves a block *allocated to a file but clear in the
bitmap* — immediately hands that block to the next writer. Here the next
writer was the log.

## 4. Hypotheses, most likely first

1. **Lost bitmap update.** The config's new blocks (step 1) were set in a
   staged `bitmap_cache` entry; a following free in the same group (the log
   truncate) or a cache reload wrote back / re-read a bitmap image without
   those bits. The blocks then belong to the config's inode but read as free,
   and the log's next allocations take them. Fits: different inodes, the
   config's size intact, its blocks holding the log's text.
2. **Concurrent appenders on one inode** (step 3): two threads extend
   `/tmp/akuma-fb.log` at once; if the append path reads the inode, allocates
   and writes it back outside one critical section, one thread's block
   pointers can be lost (leaking blocks still marked allocated) or a block
   can be allocated twice. §5 shows the append path is *not* atomic today.
3. **Deferred inode frees** (`drain_deferred_frees`, inode pins): `mv` over a
   running binary's path unlinks a mapped inode; its blocks are freed later.
   If that free used stale block pointers it would release blocks now owned
   by someone else.

The "1391 lines vs 2243 bytes" observation (§1) also deserves its own look:
a read must stop at `i_size`.

## 5. Reproducible: concurrent `O_APPEND` writes are lost

```sh
cd /tmp/ext2r; : > B
(for j in $(seq 1 150); do echo "x $j ..." >> B; done) &
(for j in $(seq 1 150); do echo "y $j ..." >> B; done) &
wait; grep -c "" B        # 298, not 300
```

Linux guarantees that with `O_APPEND` the seek to end-of-file and the write
are one atomic step, so concurrent appenders never overwrite each other. Here
two lines out of 300 were lost — two writers computed the same end-of-file
and one overwrote the other. Any multi-threaded or multi-process logger is
affected (rio's trace, shell `>>`, syslog-style writers).

## 6. What did not reproduce it

* 40 iterations of: rewrite file A (60 lines), truncate B, append 120 lines
  to B, check A — sequential, one process. Clean.
* 25 iterations of the same with **two concurrent appenders** on B. A stayed
  clean (B lost lines, §5).

Not yet tried: adding the large `wget` + `mv`-over-a-running-binary step, or
appending from two threads of one process (rio's case) rather than two
processes.

## 7. Workarounds in userspace (until fixed)

* Rotate logs with `mv`, never truncate (`: > file`) a file other writers may
  append to.
* Keep important files (configs) committed elsewhere: the rio panel config
  lives in the rio fork as `misc/akuma/config.toml`.

## 8. Suggested next steps

* A kernel-side stress test in `crates/akuma-ext2/src/tests.rs`: interleave
  file rewrite (`O_TRUNC` + write), truncate of a second file and concurrent
  appends to it, then fsck-style check that no block is referenced by two
  inodes and every referenced block is set in the bitmap.
* Make the append path's "find EOF, allocate, write, update size" one
  critical section under the state write lock (fixes §5 and closes
  hypothesis 2).
* Audit `bitmap_slot` / `bitmap_cache` staging around eviction and reload
  (hypothesis 1).
* An e2fsck of the box's disk would show whether blocks are currently
  cross-linked (run it offline).
