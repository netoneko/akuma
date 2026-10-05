# Handoff prompt: ext2 data corruption on the amd64 trashcan

Paste everything below the line into a fresh session started in this repo.

---

You are working in the Akuma kernel repo (`~/github.com/netoneko/akuma`). The
goal of this session is **one bug**: ext2 on the amd64 bare-metal box (the
"trashcan") occasionally replaces a file's data blocks with stale contents.
It has destroyed a config file and, on 2026-10-05, a freshly pushed kernel
image. Find the root cause, fix it, and prove the fix. Nothing else.

## Read first, in this order

1. `CLAUDE.md` — this repo's rules override anything below. In particular:
   **the user drives all commits** (never commit, push, reset or rebase; run
   clippy and the host tests, leave the work uncommitted); no fork/multi-agent
   fan-out; never launch background agents without asking; justify every
   allocation; console output only via `safe_print!`/`tprint!`.
2. `docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` — **the whole
   record**, especially §9 (the 2026-10-05 kernel-image occurrence with
   block-level evidence), §4 (hypotheses) and §10 (the `O_APPEND` fix, which
   is a *different* bug and is already done).
3. `docs/runbooks/amd64-bare-metal-loop.md` — how to change kernel code and
   see it on the box. Its first rule applies: **grep `docs/archive/` before
   forming a theory** (`ls docs/archive | grep -i -E 'ext2|fpcache|inode|deferred|cache'`).
   `EXT2_UNLINK_INODE_BLOCK_LEAK.md`, `EXT2_WRITEBACK_DESIGN.md`,
   `SELFHOST_ZERO_PAGE_HUNT.md` and `AKUMA_EXT2_CLEANUP.md` are the
   likeliest priors.

## What is known

* **ext2 block size 4096**, root on a USB disk (`akuma-xhci` +
  `akuma-usb-storage`), write-back block cache (`ClockBlockCache`, up to
  ~256 MB, `crates/akuma-ext2/src/ext2.rs`).
* The 2026-10-05 occurrence, on kernel `94eda586`: `cat > /root/akuma-amd64.new`
  (8.5 MB) **over an existing similar-sized older copy** (so `O_TRUNC`, then
  ~2 100 block allocations reusing the just-freed blocks); md5 correct right
  after. Then: two small files pushed with `cat >` (one of them over a binary
  that had been executed, i.e. mapped), a shell loop doing `: > B` truncations
  plus 600 concurrent `>>` appends in `/tmp/ext2r`, two probes run. Minutes
  later: **exactly five 4 KiB blocks** of the kernel file wrong — file blocks
  1655–1656 and 1659–1661, all in the **double-indirect** range (indirect #0,
  entries 619–625). Contents are *stale old data* (one block is an older
  build's bytes at that build's layout), never the newly appended text. The
  bad file is still on the box as evidence: `/root/akuma-amd64.new`, md5
  `449a73dc9486c1e06a41e99193ba5576`, inode 9439. The correct bytes are any
  build whose md5 is `19258767810762b6b530e1e4ee152861` (also on the box as
  `/boot/akuma-amd64.pty`).
* The only ext2 path that drops a dirty cached block **without writing it** is
  invalidate-on-free (`free_block` → `invalidate_block` → cache `remove`). So
  the leading theory is a **stale free**: block numbers freed again (or a
  pointer block reverted to a stale copy) after the new file owned them.
  First suspect: `drain_deferred_frees` / `release_last_link` freeing an
  unlinked-while-pinned inode's blocks from its record after those blocks were
  already freed and reallocated. Second: an owned `read_block` copy of an
  indirect or double-indirect block written back stale (`ensure_block`,
  `free_blocks_from`, `truncate_inode`, which also does not handle
  triple-indirect).
* **Do this first:** §11 of the corruption doc — `sync(2)` is not dispatched
  on amd64 and nothing ever sends SCSI `SYNCHRONIZE CACHE`, so a `reboot -f`
  loses recent writes (seen 2026-10-05: two renames lost, one freed inode
  half-persisted). Fix that and make `sys_reboot` flush first, or every
  on-box result is suspect. The box's disk is currently inconsistent
  (`/boot/akuma-amd64.prev`, inode 9467, became a 2102-byte file): ask the
  user for an offline `e2fsck` from Ubuntu.
* Ruled unlikely, not excluded: device-level loss (the USB write path checks
  CSW status and transfer length, serialized under the xHCI lock).
* **Not reproduced on the host yet.** Two tests in
  `crates/akuma-ext2/src/tests.rs` pass and should keep passing:
  `rewrite_truncate_rename_and_concurrent_append_keep_files_apart` (with an
  `e2fsck -fn` oracle) and `overwrite_of_a_double_indirect_file_survives_eviction`.
  Neither has inode pins or deferred frees — add those (`akuma_primitives::inode_pin`)
  and concurrency, and make the test fail first.

## How to work

* Start on the host. Write the failing test before the fix; a test that
  never failed proves nothing (temporarily revert the fix and show it fails).
* Useful kernel-side instruments already exist: `E2_VERIFY_HITS` /
  `[E2C-BAD]` (cache hit vs disk re-read), the `DEFERRED_DRAIN_*` counters,
  `[INODE]` lines. Add a temporary check if needed (e.g. assert a freed block
  is not referenced by any live inode, or log every free in the double-indirect
  range) and **remove it before finishing**.
* On the box: `ssh akuma`, driven from Python via `scripts/utils/hpbox.py`
  (`akuma(cmd, timeout=...)`, `akuma_push(local, remote)`, `wait_for("akuma")`;
  the CLI has a 60 s timeout). The box is the user's: **say what you are about
  to run before running anything long, heavy or disruptive, and ask before a
  reboot** if anyone may be using the console.
* **Verifying a file on disk needs a cold cache.** An md5 right after a write
  is served from the block cache. A reboot is the cheap cold-cache check
  (streaming GBs through `cat` to evict is slow and was a mistake).
* **Deploying a kernel while this bug is open:** push to a *fresh* name (no
  `O_TRUNC` of a large existing file), reboot or otherwise get a cold read,
  verify md5, then `mv` into `/boot/akuma-amd64`. Never use
  `scripts/install_kernel_amd64.sh` over an existing kernel until the bug is
  fixed (it `cp`s over the old file). Keep `.prev`; `.good` is the GRUB
  fallback and needs a physical keypress.
* An offline `e2fsck` of the box's disk (from Ubuntu, which needs someone at
  the GRUB menu) would show current cross-links; ask the user if you want it.

## Report at the end

Root cause with evidence (code path, the failing-then-passing test), the fix
(files, reasoning for every allocation), host test + clippy results, what you
ran on the box with real output, and what is still unverified. Update
`AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` (mark it FIXED with the date only
if it is) and add a row to `docs/README.md`'s symptom matrix if the symptom is
not already there. Leave everything uncommitted.
