# amd64: the frame ledger, and dynamic linking

**Date:** 2026-09-06
**Status:** both shipped and verified on QEMU `-M microvm`.

Two steps toward CoW `fork` and self-hosting on amd64, taken together because
the first is the second's teardown prerequisite and both are verified by the
same boot.

---

## 1. `FrameSet` -> `akuma_user_space::FrameLedger`

### What was wrong

`amd64/src/loader.rs`'s `FrameSet` was a `Box<[usize]>` of 2048 entries — 16 KiB
of heap per live process — with `free_all` calling `free_page` unconditionally
on every entry. Three defects, all now gone:

1. **The ceiling was reachable from a shell.** A `fork` needs one entry per
   mapped user page and busybox is ~400 pages, so a modest fork storm ran the
   array out and `fork` returned `ENOMEM`: `sh: can't fork: Out of memory` on an
   otherwise idle 2 GiB machine. It was hit *by accident* earlier the same day,
   trying to force a `dmesg` ring wrap with a `wall` loop.
2. **It could not count.** Every entry was freed exactly once at teardown, which
   is correct only while no two mappings share a frame — i.e. only while there is
   no CoW. **A refcount per frame is CoW's prerequisite, not its consequence:**
   the fault handler can be written without one, and teardown then frees a page
   the sibling is still reading. That bug is silent and arrives much later.
3. **It duplicated something already host-tested.**
   `akuma_user_space::FrameLedger` (`docs/archive/AKUMA_USER_SPACE_LEDGER.md`) is
   the same job with the two-counts rule and 14 tests, and it builds for
   `x86_64-unknown-none`.

### What changed

`pub type FrameSet = akuma_user_space::FrameLedger;`, plus `free_all_frames`
(freeing stays amd64's job — the ledger deliberately cannot call the PMM to
release a frame). `MAX_PROC_FRAMES` is **deleted**; there is no capacity to
exhaust, so the `"image needs more frames than a process may own"` error and its
failure arms went with it. `loader::load` and friends now take `&FrameSet`
rather than `&mut`, because the ledger is internally synchronised.

Two bugs fixed on the way, both pre-existing in shape:

- `fork_from`'s mapping-failure path called `free_page` on a frame the set had
  already recorded, so the bail-out below freed it **again**. It now hands the
  ledger's claim back first and frees only if `remove_user_frame` says the free
  is ours — the same contract CoW teardown will need.
- `Process::new`'s "the set is full" arm became unreachable and was deleted
  rather than left as `if false`.

### Result

`ps` and 240/0 self-tests unchanged, and **free memory is byte-for-byte stable
across 100 `fork`+`exit` cycles** (`delta = +0 KiB`), which is the teardown
working.

A 60-fork loop that previously failed now completes. **An 800-fork loop still
fails**, and this is worth stating plainly: the *cap* is gone, the *copying* is
not. Eager `fork` still duplicates every page, so real memory is still the
ceiling until CoW lands. The ledger moved the failure from an arbitrary
2048-entry limit to the machine's actual RAM.

---

## 2. `PT_INTERP` — dynamic linking

### Before

Blocker #6 in `AKUMA_AMD64_STREAMLINING.md` §11. The loader refused a
`PT_INTERP` segment outright: static and static-PIE only.

### The kernel's job is small

A `PT_INTERP` segment names an interpreter — `/lib/ld-musl-x86_64.so.1` for
everything Alpine ships. All the kernel has to do is:

1. Place the program, as always.
2. Place the interpreter (an `ET_DYN` image) at `INTERP_BASE`.
3. **Enter at the interpreter's** entry point, not the program's.
4. Report `AT_BASE` (where the interpreter landed) alongside the existing
   `AT_PHDR`/`AT_PHNUM`/`AT_PHENT`/`AT_ENTRY`.

Everything after that — mapping shared libraries, resolving symbols, running
initialisers, jumping to the program — happens in ring 3, in the interpreter,
through syscalls this kernel already serves. There is no kernel-side symbol
resolution and there should never be.

### Implementation

`load` was split: `place_image(image, space, frames, force_base)` places one
image and returns a `Placed` (entry, end_va, phdr_addr, and any `PT_INTERP`
path); `load` calls it once for the program and again for the interpreter.
`LoadedImage` gained two fields:

- `prog_entry` — the **program's** entry, for `AT_ENTRY`. `entry` is now where
  ring 3 is actually entered, which for a dynamic image is the linker. Reporting
  the linker's own entry in `AT_ENTRY` would loop.
- `interp_base` — for `AT_BASE`, `0` for a static image (what Linux reports too).

`INTERP_BASE = 0x4000_0000`, between `PIE_BASE` (0x1000_0000) and
`mm::MMAP_BASE` (0x1_0000_0000), so it collides with neither the program below
nor the mmap window above. `ET_EXEC` images link lower still (busybox at
0x40_0000), so both program shapes clear it.

`AT_BASE` is not decoration: a PIE interpreter is linked at 0 and has no other
way to find its own relocations. Omit it and `ld-musl` self-relocates against
address 0 and faults in the first page, before running a line of the program.

### A bug the change nearly shipped

`STACK_WORDS_MAX` — the bound on the stack buffer `build_stack` assembles the
initial frame in — still budgeted **12** auxv words while the new code wrote
**14**. On a program with a full argv that is an index-out-of-bounds panic in
the kernel. Both now derive from one `AUXV_WORDS` constant with a
`debug_assert!` tying the count to the bound, so the next auxv entry is a build
failure rather than a fault.

### The disk

`mkdisk.sh` fetches Alpine's `musl` (for `/lib/ld-musl-x86_64.so.1`) and stock
`busybox` — which is `ET_DYN` with `PT_INTERP=/lib/ld-musl-x86_64.so.1`, the
exact shape needed. Installed as **`/bin/busybox.dyn`, not over `/bin/busybox`**:
the static one is what `init=` and every `sh` runs, and swapping it would make a
dynamic-linking regression look like the machine failing to boot.

(Trap: an apk is a gzipped tar with a signature member first, so members must be
named explicitly. And the first `cd` in a subshell moves `$OLDPWD`, so a second
`$OLDPWD`-relative path resolves elsewhere — which is how the interpreter landed
on the image and `busybox.dyn` silently did not.)

### Result

```
$ /bin/busybox.dyn uname -a
Akuma akuma 0.1.0-amd64 Akuma/amd64 (x86_64 bring-up) x86_64 Linux
```

A **stock Alpine dynamically-linked binary**, compiled by nobody here, loaded
through a real dynamic linker. Six of seven applet probes pass — `uname`, `ls`,
`date`, `cat`, `md5sum`, and `sh -c 'echo ...'` (a `fork` through the dynamic
linker). The seventh, `printf`, fails **identically on the static busybox**, so
it is not the dynamic path.

---

## Probe results

The Tier 3 probes in `docs/runbooks/verify-trim-fat-change.md`, run on AArch64
because the frame accounting they exercise is what `akuma-user-space` moved.
**All seven pass:**

| probe | result |
|---|---|
| `elftest` | PASS (exit 42 is success by design) |
| `forkprobe` | `sockfd PASS`, `24 children live` |
| `stackstress` | `PASSED` after 100/100 |
| `bssfork` | `failures=0` |
| `bssfork 20 8 1` (control) | `failures=0` |
| `cowstale` | `reader_checks=139400441 reader_faults=0 failures=0` |
| `madvshared` | `ALL PASS` |

`cowstale` and `madvshared` are the two that most directly exercise the CoW
frame accounting, so they are the ones that matter here. Note the runbook's own
warning: `cowstale` is sampled-flaky (1/15 at SMP=4 post-fix), so **one clean run
is evidence, not proof**.

Tier 1 gate (`scripts/verify_trim.py --tier 1`): four clippy profiles clean,
**1307 host tests, 0 failed.**

---

## What is left, in order

Toward **CoW** (`fork` still copies every page eagerly):

1. **A CoW bit in `Prot`** — x86 PTE bit 9 (AVL) is free. Without it a write
   fault on a read-only page cannot tell *CoW-demoted* from *`mprotect`
   read-only*; amd64 has no region table, so the PTE is the only record. This is
   testable in a self-test without touching `fork`.
2. **A CoW arm in `page_fault_dispatch`**, before the user-copy fixup, on
   present+write+user with the CoW bit set. If the refcount is 1, flip RW in
   place — no copy. That case is most of the win, because the common shape is
   fork-then-immediately-`execve`.
3. **`fork_from` becomes a share loop**: `cow_ref_inc` and map into the child
   read-only+CoW, *and re-map the parent's own writable pages the same way*.
   Demoting only the child leaves the parent writing through to memory the child
   can see change.
4. Teardown already decrements — that is what step 1 of this document bought.

Known limit to record rather than discover: `smp.rs` states `invlpg` is
core-local with **no TLB shootdown**. Demoting a parent page another core has
cached needs one, so CoW is correct at SMP=1 and needs that machinery before
SMP>1.

Toward **self-hosting**, the rest of `AKUMA_AMD64_STREAMLINING.md` §11 with #6
now closed: threads + futex (#2, entirely absent), a per-process fd table (#3,
one global 64-entry table), mmap VA reuse (#4, a global monotonic bump), whole-file
caching (#5), and signals (#7).

## Background

- `docs/archive/AKUMA_USER_SPACE_LEDGER.md` — the ledger this adopts.
- `docs/archive/AKUMA_ELF_ARCH_NEUTRAL.md` — why `akuma-elf` is arch-neutral now,
  and what still blocks `akuma-exec`.
- `docs/archive/AKUMA_AMD64_STREAMLINING.md` §11 — the self-host blocker list.
