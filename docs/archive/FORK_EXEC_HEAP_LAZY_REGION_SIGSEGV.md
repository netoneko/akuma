# `( cmd; more-cmds ) &` SIGSEGV — a grandchild fork loses its inherited mmap regions

Found 2026-07-30 on branch `another-smp-attempt-0` while chasing fresh BKL attribution data
for [BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md) §7. **Root-caused and fixed 2026-07-30.**

Two separate bugs were found along the way. The second one is the crash:

1. A fork **lazy-region propagation** gap (§"Propagation fix" below) — real, fixed, but
   verified *not* to be the cause of this crash.
2. **CoW fork dropped the extent of every inherited `mmap` region**, so a *grandchild* fork
   shared none of them and faulted on the first touch (§"Root cause"). This is the crash.

## Symptom

Any real program exec'd from inside a **multi-statement backgrounded subshell** segfaults
immediately, deterministically:

```sh
( wget -q -O /tmp/out.bin http://10.0.2.2:8899/p32.bin; echo $? > /tmp/out.rc ) &
```

→ `Segmentation fault`, exit status 139 (SIGSEGV), every single time. No concurrency needed —
reproduces with exactly one backgrounded job, and reproduces identically at `SMP=1`.

## What does *not* trigger it

| pattern | result |
|---|---|
| `wget -q -O /tmp/out.bin $URL &` (no subshell, single command) | works |
| `( true; echo done > /tmp/x ) &` (subshell, but a trivial command) | works |
| `( wget -q -O /tmp/out.bin $URL; echo $? > /tmp/out.rc ) &` (subshell, real command) | **SIGSEGV** |
| 4 concurrent copies of the crashing pattern | SIGSEGV, same as 1 |
| the crashing pattern at `SMP=1` | SIGSEGV, same as `SMP=4` |

This table is the root cause in disguise: the trigger is **two levels of fork**. The shell
forks a subshell (needed because there is more than one statement in the parens), and the
subshell forks *again* to exec the real command. `wget &` is a single fork-then-exec and is
fine; `( true; ... ) &` never forks a second time because ash runs builtins in-process.

## Root cause

`Process::mmap_regions` was `Vec<(usize, Vec<PhysFrame>)>` — a start VA and the frames
backing the region. **Every consumer derived the region's extent from `frames.len()`**, and
that was the only record of how big the region was.

A CoW-forked child owns none of its inherited frames: `cow_share_range` maps the parent's
pages read-only into the child and refcounts them in `UserAddressSpace::user_frames`; a write
fault later hands the child a private frame. So `fork_process` gave the child region entries
with a deliberately empty frame list:

```rust
new_proc.mmap_regions = parent.mmap_regions.iter()
    .map(|(va, frames)| (*va, Vec::with_capacity(frames.len())))   // len 0, not `frames.len()`
    .collect();
```

`Vec::with_capacity(n)` has **length zero**. The child's regions therefore reported an extent
of 0 pages. That was invisible for the child itself — it had the pages mapped — but when the
*child* forked, the sharing loop computed a zero-length range and skipped every region:

```rust
for (va_start, parent_frames) in &parent.mmap_regions {
    let len = parent_frames.len() * mmu::PAGE_SIZE;   // 0 for an inherited region
    if len > 0 { cow_share_range(...)?; }             // → never runs
}
```

The grandchild ended up with **no mapping at all** for VAs its parent had resident and was
about to hand it live pointers into. First touch → write to an unmapped page.

### Why it landed on `0x20120338` every time

busybox is a `static-pie` loaded at `0x1000_0000` with `code_end = 0x1012_3000`, so
`ProcessMemory::new` computes `next_mmap = (code_end + 0x1000_0000) & !0xFFFF = 0x2012_0000`.
The **first `mmap` any busybox process makes is musl's first malloc arena**, and it is
*eager* (`MMAP_EAGER_MAX_PAGES = 16`, arena is 1 page), so it goes into `mmap_regions`:

```
[mmap] pid=121 len=0x1000 prot=0x3 flags=0x22 = 0x20120000 (eager)
```

The faulting instruction (`ELR=0x1004e8f0`, file offset `0x4e8f0` in `bootstrap/bin/busybox`)
is a double-pointer-dereference write through a fixed global in `.data`:

```
adrp x0, 0x11f000       ; x0 = 0x1011f000
ldr  x0, [x0, #0xf88]   ; x0 = *(0x1011ff88)   — PTR_A, fixed global in .data
ldr  x0, [x0]           ; x0 = *(PTR_A)        — PTR_B = 0x20120030
str  wzr, [x0, #0x308]  ; *(0x20120338) = 0    <-- faults
```

`PTR_B = 0x20120030` is not a wild value at all: it is a **perfectly valid heap pointer**,
`0x30` into that first malloc arena, inherited through two forks. `0x20120338` is inside the
same page. The grandchild simply had nothing mapped there.

`ISS=0x46`/`0x47` (DFSC = level-2 translation fault, `WnR=1`) is exactly right for this: the
whole 2 MB L2 block is absent in the grandchild, not merely permission-denied — which is why
it never touched the CoW refcount machinery.

### Direct confirmation

With a temporary per-region trace in `fork_process`, one run of
`( /bin/busybox cat /etc/hosts > /tmp/o0; echo $? > /tmp/rc0 ) &`:

```
[FORK-DBG] parent_pid=137 child_pid=140 ... mmap_regions=4     <- shell (ran the mmaps)
[FORK-DBG]   mmap va=0x20120000 pages=1
[FORK-DBG]   mmap va=0x20121000 pages=1
[FORK-DBG]   mmap va=0x20122000 pages=2
[FORK-DBG]   mmap va=0x20124000 pages=1
[FORK-COW] shared 1090 pages

[FORK-DBG] parent_pid=139 child_pid=141 ... mmap_regions=4     <- subshell (a CoW child)
[FORK-DBG]   mmap va=0x20120000 pages=0                        <- extent gone
[FORK-DBG]   mmap va=0x20121000 pages=0
[FORK-DBG]   mmap va=0x20122000 pages=0
[FORK-DBG]   mmap va=0x20124000 pages=0
[FORK-COW] shared 1085 pages                                   <- exactly 5 fewer: 1+1+2+1

[DA-MISS] pid=141 ppid=139 va=0x20120338 lr_count=7 parent_lr=7 parent_has_va=false
[Fault] Data abort from EL0 at FAR=0x20120338, ELR=0x1004e8f0, ISS=0x47
[Fault] Process 141 (/bin/busybox) SIGSEGV after 0.01s
```

`1090 - 1085 = 5` — precisely the four lost regions' five pages.

Note the `parent_has_va=false` in `[DA-MISS]`: that diagnostic only consults
`LAZY_REGION_TABLE` (`lazy_region_lookup_for_pid`). These are **eager** regions, so it was
never going to report them, and reading it as "nobody ever reserved this VA" is what sent the
first pass at this investigation looking for a phantom wild pointer. The VA was validly
reserved — by the grandparent, as an eager mmap.

## The fix

`mmap_regions` is now `Vec<MmapRegion>` (`crates/akuma-exec/src/process/types.rs`) with the
extent recorded **independently of frame ownership**:

```rust
pub struct MmapRegion {
    pub start_va: usize,
    pub pages: usize,            // authoritative extent — survives CoW fork
    pub frames: Vec<PhysFrame>,  // frames this process owns; EMPTY when CoW-inherited
}
```

- `MmapRegion::owned(va, frames)` — the process that called `mmap`; `pages == frames.len()`.
- `MmapRegion::inherited(va, pages)` — a CoW-forked child; extent kept, owns no frames.
- `contains(va)` / `len_bytes()` use `pages`; `frame_for(va)` consults `frames` and returns
  `None` for an inherited region (it needs a real PA and there isn't one).

Call sites split along that line:

| site | uses |
|---|---|
| `fork_process` CoW share + RO demotion (`process/mod.rs`) | `pages` |
| `fork_process` sibling-thread eager mmap replication | `pages` |
| `sys_munmap` region sizing + split (`syscall/mem.rs`) | `pages` |
| `sys_mremap` "is this mapped" probe | `pages` (`contains`) |
| `remove_mmap_region` VA-range reclaim (`process/children.rs`) | `pages` |
| `[DP-eager]` fault fallback re-map (`exceptions.rs`) | `frames` (`frame_for`) |
| MAP_SHARED writeback / `msync` | `frames` |

The child-derivation is now a named helper,
`inherit_mmap_regions_for_cow_child(&[MmapRegion]) -> Vec<MmapRegion>`
(`crates/akuma-exec/src/process/children.rs`), so the invariant that used to be lost inside a
`.map()` closure is testable on the host.

Two consequential bugs fell out of the same root and are fixed with it:

- **The parent was never demoted to RO for an inherited region.** `demote_range_to_ro(...,
  parent_frames.len())` demoted 0 pages, so a subshell's writes went straight through to a
  page it still shared CoW with its own child, silently clobbering the child's snapshot.
- **`munmap` on an inherited region unmapped nothing** while still recycling the VA range
  into `free_regions` for `alloc_mmap` to hand back out. `sys_munmap` now iterates `pages`
  VAs and, for pages with no owned frame recorded, takes the PA from the live PTE via
  `unmap_and_free_page_no_flush` (which also handles the case where a CoW write fault already
  swapped in a private frame — reading a stale PA out of a recorded frame list would have
  been a double-free).

### Verification

Same repro, same build, after the fix:

```
[FORK-DBG] parent_pid=118 child_pid=120 ... mmap_regions=4     <- subshell (a CoW child)
[FORK-DBG]   mmap va=0x20120000 pages=1 owned=0                <- extent kept, owns nothing
[FORK-DBG]   mmap va=0x20121000 pages=1 owned=0
[FORK-DBG]   mmap va=0x20122000 pages=2 owned=0
[FORK-DBG]   mmap va=0x20124000 pages=1 owned=0
[FORK-COW] shared 1090 pages                                   <- same as the owning parent
[syscall] execve(path="/bin/busybox", args=["wc","-c","/bin/busybox"]) PID 120
```

No `[DA-MISS]`, no `[WILD-DA]`, no SIGSEGV; the grandchild reaches `execve` and runs:

```sh
~ # ( /bin/busybox wc -c /bin/busybox > /tmp/o0; echo $? > /tmp/rc0 ) &
~ # cat /tmp/rc0
0
~ # cat /tmp/o0
1116408 /bin/busybox
```

## Propagation fix (real bug, separate from the crash)

While chasing the above, a genuine second bug was found and fixed: `fork_process`'s
lazy-region sharing only used `cow_share_range` to copy pages **currently resident** in the
parent — it never copied the parent's lazy-region *descriptors* into a fresh
`LAZY_REGION_TABLE` entry for the child. A lazy region the parent registered but hadn't
touched yet (a `.data`/`.bss` page nobody wrote to since exec, a stack page deeper than the
parent's current usage) had nothing resident to share, so the child got **no coverage for
that VA at all** — not resident, not lazy — an unconditional SIGSEGV on first touch.

Fixed by `propagate_lazy_regions_to_child(parent_pid, child_pid)`
(`crates/akuma-exec/src/process/children.rs`), called from both `fork_process` branches,
which copies every `LazyRegion` descriptor (VA, size, flags, `LazySource` — including
file-backed sources with their path/inode/offset) into a fresh entry for the child.

**This is worth keeping but it is not the crash in this doc.** Verified directly: after
landing it, `[DA-MISS]` for the still-crashing pid showed `lr_count=6 parent_lr=6` (all 6
parent regions propagated correctly) and the crash was completely unchanged — because the
faulting VA belonged to an *eager* region, which this code path never touches.

## Tests

Host-testable (`cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`):

- `crates/akuma-exec/src/process/children.rs::mmap_region_inheritance_tests` — extent survives
  one CoW fork (`pages` kept, `frames` empty), survives a *second* fork with a non-zero range
  (the actual regression, asserting the reported `0x20120338` still lands inside the
  grandchild's region), and `frame_for` declines on an inherited region.
- `crates/akuma-exec/src/process/children.rs::lazy_region_propagation_tests` — the
  propagation fix above.

## Follow-up

`scripts/bkl_smp_regimen/payload/job.sh` still works around this by writing each parallel
worker out as its own script and backgrounding a single `sh workerN.sh &` instead of an inline
`( cmd; ... ) &`. That workaround is now unnecessary and can be reverted to the simpler inline
form whenever the SMP campaign is next touched; it is harmless as-is, so there is no urgency.
