# Defect B — cargo's heap corrupts under `-j4` (null `Rc`)

**Status: root-caused and FIXED 2026-08-14.** It was `MADV_DONTNEED` zeroing a
CoW-shared frame out from under the peer process.

> **The fix, the evidence and the follow-ons live in
> [`MADV_DONTNEED_SHARED_FRAME.md`](MADV_DONTNEED_SHARED_FRAME.md)** — read that
> first. It is not duplicated here, so this file cannot drift from it.
>
> This document is the **task brief** the hunt ran from, kept because ~30 comments
> across `src/`, `crates/`, `scripts/` and `userspace/` point at it for context
> that has no other home: the D8/D9 mmap-region-extent work, the UAF quarantine
> instrument, the `EAGER-UPGRADE` report, and the traps below. It was
> `proposals/CARGO_HEAP_NULL_RC.md` until the move into `docs/archive/` — the
> `proposals/` directory is gitignored, so every one of those references was
> dangling in a fresh clone.

**Read alongside:**
[`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
§13 (the audit that ruled out PMM-level UAF and narrowed the search) and
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
→ §"Status" → "Defect B" (the ELR decode recipe).

**The hypothesis list below is spent.** Theory 3 — "`madvise` range rounding …
`MADV_DONTNEED` zeroes regardless of backing" — was the right one, in its
*sharing* form rather than its rounding form. The rounding half is still open and
still unexercised; see the follow-ons in `MADV_DONTNEED_SHARED_FRAME.md`. The
`Reproduce` and `Traps` sections stay accurate and are the reason to keep reading.

## What's known

During an in-guest `-j4` self-host kernel build, `cargo` itself takes a null
dereference and the build dies with `EXIT=139` after ~15 crates. Observed once in
5 runs (2026-08-07), at T72 — **mid-build, not teardown**, correcting an earlier
note that filed it as a teardown-only curiosity.

```
[T72.68] [WILD-DA] pid=17 FAR=0x0 ELR=0x104e48c8 last_sc=222   (222 = mmap)
  x0=0x314da660 x1=0x258 x2=0x203fff82b0 x3=0x203fff8338
  x4=0x0 x5=0x3fd x6=0x11f37d60 x7=0x300c2280
  x8=0x0  x9=0x1 x10=0x0 x11=0x20 …
[T72.70] [WILD-DA] pid=17 FAR=0x0 ELR=0x104e48c8 last_sc=98    (98 = futex)
```

Same PC twice, 20 ms apart (two threads). pid 17 is `/usr/local/bin/cargo`
(confirmed via PSTATS). cargo's text loads at `seg_va=0x10000000`,
`filesz=0x1da1c6c` → PC = file offset `0x4e48c8`:

```
4e48b4 <drop_glue<cargo::compiler::unit::UnitInner>>:
  4e48c0:  ldr x8, [x0, #288]   ; the Rc<PackageInner> pointer field
  4e48c8:  ldr x9, [x8]         ; FAULT — x8 == 0
```

**The finding:** this is `Rc::drop`'s refcount decrement, and the pointer at
`UnitInner+288` read back as **zero**. Safe Rust cannot construct a null `Rc`.
So a live pointer qword in cargo's **anonymous heap** was zeroed underneath it.
This is a kernel memory-management bug, not a cargo bug. That's the thing to
explain.

## Reproduce

Fresh boot per attempt (snapshot disk, so each boot starts clean):

```bash
DEVBOX_DISK=disk_selfhost.img DEVBOX_MEMORY=14336 SMP=4 INSTANCE=1 SNAPSHOT=1 GDB=1 \
  bash overlays/devbox/run-smoltcp.sh > boot.log 2>&1 &
# guest (ssh -p 2322), detached — there is no `nohup` binary, use busybox:
#   busybox setsid sh -c 'cd /root/akuma && export PATH=/usr/local/bin:/usr/bin:/bin \
#     CARGO_HOME=/root/.cargo && cargo clean && \
#     cargo build --release -p akuma -j4 --offline > /root/b.log 2>&1'
```

~1-in-5 hit rate, so budget several runs, and **always boot with `GDB=1`**
(gdbstub on `1234+INSTANCE` = `:1235`) — QEMU's stub must be armed at launch; a
crash on a VM booted without it is uninspectable.

Full serial log from the actual occurrence:
`/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/8c6bfa3b-85f8-494b-b771-1c1381c52460/scratchpad/run3_sigsegv_serial.log`
(a dead session's scratchpad — readable, don't write there).

## Hypotheses, roughly in order

1. **`madvise` range rounding.** Linux `MADV_DONTNEED` rounds start **up** and
   end **down**, discarding only whole pages fully inside the range. If
   `src/syscall/mem.rs` rounds outward, it zeroes a page holding live allocator
   data — which is exactly one zeroed qword region in a heap. Read the handler
   and compare against Linux semantics directly. Related known issue:
   `MADV_DONTNEED` zeroes regardless of backing (correct for anon, wrong for
   file-backed `MAP_PRIVATE`) — documented latent in
   `docs/archive/BKL_VFS_CARVE_OUT.md` §10, whose `WILLNEED` sibling was a real
   zero-fill corruption bug.
2. **`munmap` partial-range / refcount.** `last_sc=222` (mmap) at the fault, and
   cargo does ~2400 mmaps per build. See the `munmap` user-frames double-free
   history.
3. **CoW after fork.** cargo forks constantly; prior art in the fork CoW TLB/ASID
   flush fix (wrong-ASID flush let a parent write a shared CoW page).
4. **File-page-cache dedup leaking into a private mapping**
   (`src/file_page_cache.rs`; `config::SHARED_FILE_PAGES_ENABLED = false` is the
   A/B kill switch) — less likely for anon heap, but it's the newest
   allocator-adjacent code.
5. Anon page eviction/reclaim discarding a **dirty** page.

## Existing tools — use before writing new ones

`userspace/forktest/c_stress/` already has a content-integrity kit, all
README'd: `mmapsum` (read vs mmap vs madvise vs 2-thread digests — its `madv:`
digest is the regression check that caught the WILLNEED bug), `fpfault`,
`neonfault`. A targeted `madvise`-range test in that style is likely cheaper and
far more deterministic than another 10-minute build run.

## Traps

- The `ssh` CLI is blocked by policy — drive it from Python `subprocess` (`-p 2322`).
- Serial logs interleave across cores and contain binary bytes: always `grep -a`.
- Two different binaries load at base `0x10000000`; cargo is the one with
  `filesz=0x1da1c6c`, not `0x109ad0`. Decoding against the wrong one silently
  gives a plausible-but-wrong offset.
- `[BKL] stuck tag=511` storms (hundreds per build) are a known separate class —
  noise here, don't chase.
- Guest has no `nohup`/`df`/`wc` on `PATH`; `busybox --install -s /root/bbin` and
  **append** `/root/bbin` to `PATH` (append, so busybox's `ar` doesn't shadow
  binutils and break the build).
- Never wait synchronously on a QEMU process; poll its log. A stalled serial log
  is the wedge signal — SSH banner timeouts are normal under build load and mean
  nothing.
- Kernel/syscall changes need a boot-suite self-test in `src/process_tests.rs`.
- **Do not commit or push** — the user drives all commits.

## Deliverable

Root cause with evidence, a fix, a regression test, and a doc update (extend the
Defect B section rather than starting a new doc). Note defect **A** — the
unexplained all-core wedge in the same table — is a separate open item; don't
conflate them.
