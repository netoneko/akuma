# ext2: one file's data written into another file's blocks

**Date:** 2026-10-05. **Found:** bringing up rio on the trashcan panel
(`AKUMA_AMD64_RIO_FBDEV_BUILD.md`), while iterating on a rio config in a
throwaway `HOME`. **Status: OPEN — seen a second time 2026-10-05 (§9), on a
kernel image, with block-level evidence; root cause not found.** The
*reproducible* defect found alongside it — concurrent `O_APPEND` writers lose
writes (§5) — is **FIXED** 2026-10-05 (§10).

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


## 9. Second occurrence, 2026-10-05 — a kernel image, caught at block level

While deploying the pty kernel (`AKUMA_AMD64_PTY.md`), on the **old** kernel
(`94eda586`, ext2 block size 4096, root on the USB disk):

1. `hpbox.akuma_push` wrote an 8 584 272-byte kernel to
   `/root/akuma-amd64.new` with `cat >` — **over an existing, similar-sized
   older copy** (so `O_TRUNC`, then ~2 100 fresh block allocations that reuse
   the just-freed ones). The push's own check, the box's `md5sum`, matched the
   local file (`19258767…`).
2. Then, in order: two small files pushed (`/tmp/ptyprobe`, `/tmp/lifeprobe`;
   the second later re-pushed over itself after being executed), three rounds
   of the §5 two-appender repro in `/tmp/ext2r` (each starting `: > B`), both
   probes run.
3. A few minutes later the same file read back as `449a73dc…`, same size.

Per-block comparison against the local file: **exactly five 4 KiB blocks**
differ, in two runs — file blocks 1655–1656 and 1659–1661 (offsets
`0x677000–0x678fff`, `0x67b000–0x67dfff`), all in the **double-indirect** range
(indirect #0, entries 619–625). Their new contents:

* Four are sparse `0x00`/`0xFF` patterns (~400 non-zero bytes each) that occur
  nowhere in the new kernel, the probes, or any kernel image kept in `/boot`.
* One (block 1661) is **this kernel build's own bytes from offset `0x671090`**
  — not block-aligned. The same font table sits `0xbf70` lower in an older
  build, so this is what an *older* image would hold at block 1661: stale data
  of a previous file, at that file's layout.
* None of it is the appended log text.

What that rules in and out:

* The data was right after the write and wrong after unrelated activity, and
  the wrong bytes are *old* contents, not another file's *new* writes. The only
  ext2 path that discards a dirty cached block without writing it is
  invalidate-on-free (`free_block` → `invalidate_block`), so the leading
  explanation is a **stale free**: these five block numbers were freed again
  (or their pointer block reverted) after the new file owned them, the dirty
  copies were dropped, and the disk's previous contents became the file's.
  The deferred-free path (`drain_deferred_frees` freeing an unlinked-while-
  pinned inode's blocks by its *record*, after those blocks were already freed
  and reallocated) is the first suspect; a stale owned copy of an indirect
  block written back is the second.
* The USB mass-storage write path checks every CSW and the transferred length
  and is serialised under the xHCI lock, so a silent device-level loss is
  unlikely but not excluded.
* Not reproduced on the host: `overwrite_of_a_double_indirect_file_survives_eviction`
  (truncate + unaligned rewrites of a double-indirect file, 32 KiB cache,
  churn, fresh-mount comparison) and
  `rewrite_truncate_rename_and_concurrent_append_keep_files_apart` (§8's
  plan, with `e2fsck -fn` as the oracle) both pass. Neither has pinned inodes
  or deferred frees, which is the next thing to add.

**Operational consequence, now in the deploy recipe:** an md5 taken right
after a write proves nothing about the disk — the cache serves the new bytes
until it evicts them, and GRUB reads the disk. Push a kernel to a **fresh
name** (no `O_TRUNC` of a large existing file), force eviction by streaming
more than the cache (`find /root/.cargo /usr -type f | xargs cat`), verify,
then `mv` it into place and verify again after another eviction. The corrupted
copy was installed as `/boot/akuma-amd64` for a few minutes before this was
noticed; it was replaced with the running kernel's image before any reboot.

The streaming eviction turned out to be the wrong tool: ~2.7 GB through `find |
xargs cat` over USB takes many minutes, and it was stopped. **A reboot is the
cheap cold-cache check**: the fresh copy, `/boot/akuma-amd64.pty` (written as a
new file, no `O_TRUNC`), was re-read 39 s after a reboot of the old kernel and
matched (`19258767…`). The corrupted `/root/akuma-amd64.new` was left on the
box as evidence (md5 `449a73dc…`, inode 9439; bad file blocks 1655–1656,
1659–1661).

**Status after this session: the cross-file corruption is NOT fixed.** The
`O_APPEND` fix (§10) closes hypothesis 2 only; the kernel-image corruption does
not look like an append race (stale old contents, no appended text), so the
pty kernel carries the bug too. Handoff: `docs/handoff-ext2-corruption.md`.

## 10. `O_APPEND` fixed (2026-10-05)

`Filesystem::append(path, data) -> (offset, written)`: ext2 implements it in
`write_locked`, which picks the offset (the size **inside** the state write
lock) and writes under one hold; glue's `sys_write` uses it for `O_APPEND`
descriptors instead of `file_size` + `write_at`. Host test
`concurrent_appends_lose_nothing` (two threads × 150 lines) fails 3/3 on the
old two-step path and passes on the new one. On the box, old kernel: the §5
repro gave **273, 271, 264** of 300 (worse than the first measurement — real
SMP). New kernel (`8bece079`): **300, 300, 300**.

Per-chunk atomicity: `sys_write` still splits a write into 64 KiB chunks, so a
single `write(2)` larger than that can interleave with another appender
between chunks. Linux holds the inode lock for the whole call.

## 11. Writes lost across `reboot -f` — no durability barrier at all (2026-10-05)

Deploying the `/dev/pts` kernel: `mv /boot/akuma-amd64 /boot/akuma-amd64.prev
&& mv /boot/akuma-amd64.pty2 /boot/akuma-amd64 && sync`, md5 of both names
correct, then `busybox reboot -f` straight away. After the reboot **both
renames were gone** (the old names pointed at the old inodes again), but
**part** of the first rename had landed: the inode it replaced (9467, the
previous `.prev`, an 8.5 MB kernel) now reads as a 2102-byte file dated 1970.
The filesystem is inconsistent on disk; it needs an offline `e2fsck`.

Two gaps explain it, and neither is ext2's write-back logic (`rename` ends in
`flush_meta`, so its blocks were handed to the device):

* **`sync(2)` is not dispatched on amd64** — x86_64 162 has no
  `syscall_table!` row, so `busybox sync` gets `ENOSYS` silently.
* **Nothing ever sends SCSI `SYNCHRONIZE CACHE`** to the USB disk
  (`akuma-usb-storage`, `amd64/src/xhci.rs`), and `amd64/src/reboot.rs`
  flushes nothing before resetting. A USB bridge/drive with a volatile write
  cache acknowledges a `WRITE(10)` before the data is on the media; a reset
  (which can cut port power) loses whatever it had not written yet, in its own
  order — hence "some of the rename, not all of it".

This does **not** explain §9 by itself (no reboot happened there), but it is
the same class of symptom — the disk ends up with older contents than the
cache acknowledged — and it must be fixed before any on-box verification of
§9 can be trusted. Fix shape: a `SYNCHRONIZE CACHE(10)` CDB in
`akuma-usb-storage`, an `xhci::flush()`, called from `sync`/`syncfs`/`fsync`
(after the ext2 flush) and from `sys_reboot` before the reset, plus a
`Sync`/`Syncfs` row in `akuma-syscalls-abi`. Until then: after a metadata
change on the box, do not `reboot -f` right away; and treat `.prev` as gone.

## 12. Checkpoint 2026-10-05 (on-box goose session, commit `1c1b8eeb` on `litter/local-console`)

State as found, then verified from the Mac:

* **§11 is implemented, not yet run on the metal.** `BlockDevice::flush` (default no-op),
  `UsbDisk::flush` -> `xhci::flush` -> SCSI `SYNCHRONIZE CACHE (10)`
  (`cdb::synchronize_cache_10`, 2 host tests); `Ext2Filesystem::sync` ends at
  `dev.flush()`; `sync`/`syncfs`/`fsync`/`fdatasync` have real handlers
  (`akuma-syscalls-glue::fs::{sys_sync,sys_fsync,sys_syncfs}`, rows `Sync`=162 /
  `Syncfs`=306 in `akuma-syscalls-abi`); `perform_reset` flushes before the reset.
  `cargo check -p akuma-amd64 --target x86_64-unknown-none --release` and the default
  AArch64 `cargo check` are clean; `akuma-ext2` (116), `-usb-storage`, `-syscalls-abi`,
  `-syscalls-linux` host tests pass. The box is still running the *old* kernel
  (`uname`: `8bece079`).
* **§9 is still not root-caused.** Goose added a standalone fsck-style walker and
  `pinned_unlink_deferred_free_and_concurrent_rewrite_keep_blocks_apart` (pins +
  deferred frees + concurrent appends + a latency-injecting device); it passes, so the
  host model still does not reproduce the bug.
* **Audit notes (read-only, no defect pinned):** reads hold the state read lock, writes the
  write lock, so the `with_block` miss-then-`insert` window cannot race a writer unless the
  lock itself is bypassed; `ClockBlockCache::insert` never overwrites a resident (possibly
  dirty) entry. Four of the five bad blocks in §9 look like **bitmap images**
  (sparse `0x00`/`0xFF`), which points at ... **[CORRECTED in §13: wrong — they are other
  files' live data.]** Next probe: `e2fsck -fn` / `debugfs` on the box's
  `/root/akuma-amd64.new` (inode 9439) and check whether bad file blocks 1655-1656,
  1659-1661 sit in some group's bitmap range or equal a `bgd.block_bitmap`.
* Two lines in the checkpoint had lost their newlines (`xhci::write_bytes` signature,
  a `///` in `akuma-vfs-glue`) — repaired.


## 13. Root cause found: lost bitmap writes across `reboot -f` (2026-10-05)

**Tool.** `Ext2Filesystem::audit()` (`crates/akuma-ext2/src/ext2/audit.rs`) is an in-kernel
`e2fsck -fn`: it walks every allocated inode and reports a block **claimed twice** and a block
**claimed but free in the bitmap**, as `[E2-FSCK]` lines (`dmesg`). amd64 runs it at mount when
`/.ext2audit` exists (`touch /.ext2audit`, reboot; remove the file to stop paying for it — it
walks ~100k inodes over USB). Two host tests pin it (clean fs = clean; a forced shared block +
a forced stale free are both reported). `free_block` also refuses a second free of a free
block now (`[E2-DFREE]`, counter `E2_DOUBLE_FREE`) instead of inflating the free counts.

**What the audit saw on the box** (100 255 inodes): `cross_linked=173`,
`claimed_but_free=7`, `out_of_range=2125` (the last is the consequence: an *indirect* block that
is also another file's data block reads as garbage pointers; 173 -> 170 once the resize inode,
which legitimately overlaps, was skipped).

* The five/eleven bad blocks of §9 are **not** bitmap images and not stale kernel bytes: block
  1655 of `/root/akuma-amd64.new` held `.git/FETCH_HEAD`, 1659/1663/1668 held goose's
  `sessions.db` pages. Those blocks are *also* claimed by those files' inodes
  (`.new` = inode 9439 shares blocks with 2609 = `.git/FETCH_HEAD`, rewritten 01:02, **after**
  `.new` was written 00:21).
* The pattern is general, not one file: Sep-26 `dmesg-boot-*.txt` (inodes 14213/14222) share
  contiguous runs of blocks with the Oct-1 `dumpster-akuma-amd64.transcript.jsonl` (12271, an
  appended log) and `akuma-amd64.splash2` (9499); `librustc_driver.so`, a 369 MB git pack, a
  54 MB wav, `nca.v3` and others are on the list. Always **an older file whose blocks a later
  allocation reused** — the allocator believed those blocks were free.

**Cause (strong inference, one direct test).** The bitmap block for those blocks was not on disk
as the kernel last wrote it. The old kernel had no durability barrier (§11): `sync` was
`ENOSYS`, nothing sent `SYNCHRONIZE CACHE`, and every deploy cycle ends in `reboot -f`. A USB
bridge that acknowledges `WRITE(10)` from a volatile cache loses writes in its own order at
reset, so the *file data and inode* of a block can reach the media while the *bitmap* update
does not. The next boot mounts that older bitmap, hands the block out again, and two files own
it. This also explains why it looked random (it depends on what the drive happened to have
written) and why it kept happening across weeks of `reboot -f` cycles; it is the same event as
§11, seen from the allocator's side instead of the rename's.

**Direct test of the fix** (barrier kernel `c8547aa2`, `SYNCHRONIZE CACHE` accepted by this
drive — no `[xhci] SYNCHRONIZE CACHE failed` line): wrote three 40 MB random files, deleted one,
`sync`, `reboot -f` **immediately**, no settling delay. After the boot: all md5s intact, the
deleted file still deleted, the new kernel installed, and `claimed_but_free` still **7** — no
new lost bit. (A single pass, not a soak; the old kernel's loss was probabilistic. Repeat the
pass a few times before calling it closed.)

**Not fixed — existing damage.** The 170 cross-links are on disk. The newer owner usually won
the data, so the *older* file is the damaged one (the Sep-26 dmesg logs, the librustc/packfile
pages the audit lists). Nothing in-kernel repairs this; `e2fsck -fy` from Ubuntu will (it
duplicates the blocks per owner). Until then: do not trust `librustc_driver.so`, the llama.cpp
pack or other long-lived files on that list, and reinstall them before a self-host build. The 7
claimed-but-free blocks (inodes 14222, 46095, 46187) can still be handed out again.

**Open:** the dmesg lines for the second owner are capped (80); raise `PRINT_CAP` or add a
`path` lookup if the full list is wanted. rio on the pty swallowing Esc/Enter/`1` is a
separate issue, `AKUMA_AMD64_PTY.md` §7.
