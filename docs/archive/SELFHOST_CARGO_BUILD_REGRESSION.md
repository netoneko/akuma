# Self-host regression: `cargo install cargo-binutils` triggers kernel exceptions

**Status: FIXED 2026-08-25** (`crates/akuma-exec/src/process/mod.rs`,
`fork_process`'s sibling-mmap-region collection). Root cause and fix below;
original capture kept as-is under "Symptom".

## Root cause

`fork_process`'s CoW-fork path collects every OTHER thread's eager mmap regions
(each pthread's stack, plus musl-malloc's `mmap`-backed arenas ≥128 KB) into a
`sibling_ranges` buffer so the child inherits them too. That buffer was a
**fixed `Vec::with_capacity(2048)`**, sized once, with `region.pages > 0`
entries silently dropped past the cap (`overflow = true`, one warning line,
no failure). A `cargo`/`rustc` build has many concurrently-live worker
threads, each accumulating its own heap arenas over the build's lifetime —
comfortably tens of thousands of live sibling regions by the time a build
script forks a subprocess. Once the process table crossed the 2048-region
mark, every fork from that thread group silently dropped whichever regions
didn't fit, and if the dropped set happened to include the range holding the
child's about-to-be-used heap/stack data, the child's *very next syscall*
(observed: `chdir()`, syscall nr=49, called by `std::process::Command`'s
`current_dir()`-via-fork-before-exec path) faulted trying to read a pointer
into memory that fork never copied.

Confirmed via `[FORK-COW] WARNING: sibling mmap region list truncated (>2048
regions)` immediately preceding the crashing fork in a
`SYSCALL_DEBUG_INFO_ENABLED=true` capture, and by both `FAR` (the faulting
address) and `SP_EL0` at fault time landing in the same address band — i.e.
genuinely a stack/heap pointer, not a wild one, and `TTBR0` at fault time
correctly matched the *child's own, freshly-created* address space (ruling out
the TTBR0-staleness bug family — [[project_relr_fork_parent_entry_point]],
[[project_vfork_stale_ttbr0]], [[project_cow_fork_mmap_region_extent]] — this
was a genuinely different bug that happens to share their "kernel dereferences
user pointer, gets EFAULT" signature).

## Fix

Two-pass collection instead of one fixed-capacity guess: count matching
regions first (still IRQs-disabled, no allocation, `for_each_process` run #1),
allocate a `Vec` sized exactly to that count between the two passes (IRQs
enabled here — safe to allocate), then collect (`for_each_process` run #2,
same no-alloc discipline). The overflow guard and warning stay, now only
firing on the narrow TOCTOU window where a sibling mmaps concurrently on
another core between the count and collect passes, rather than routinely on
any big build.

**Verified**: isolated QEMU instance (`INSTANCE=1`, APFS-cloned + fsck'd
`devbox.img`, per [[project_isolated_qemu_verification]]) reproduced the exact
crash signature within ~45s of `cargo install cargo-binutils` on the pre-fix
kernel (4 faulting `cargo` children, same signature as below); the post-fix
kernel ran the identical command to a clean `Installed package
cargo-binutils v0.4.0` with zero kernel exceptions. `cargo test -p
akuma-exec` (265 tests) still green.

## Original capture (2026-08-25, not yet root-caused at time of writing)

Recorded from a live `devbox-smoltcp` self-hosting session (the "kernel self
hosting loop" / `KERNEL_DROPOFF` work on branch `even-more-fixes`, see
`d6fb0aa8`/`169e799c`). Kept verbatim for the original evidence trail.

## Symptom

Running `cargo install cargo-binutils` inside the guest during self-hosting
triggers repeated kernel-side data-abort exceptions as `cargo`'s build-script
machinery forks/clones worker processes. Every occurrence has the same shape:

```
[Exception] Sync from EL1: EC=0x25, ISS=0x6
  ELR=0x402082cc, FAR=0x338cbe00, SPSR=0x80000345
  Thread=20, TTBR0=0x880000a85aa000, TTBR1=0x40372000
  Instruction at ELR: 0x38401423
  Likely: Rn(base)=x1, Rt(dest)=x3
  WARNING: Kernel accessing user-space address!
  This suggests stale TTBR0 or dereferencing user pointer from kernel.
[EFAULT] nr=49 pid=905 tid=20 ELR=0x30068c20 args=[0x338cbe00, 0x2, 0x0, 0x11, 0x2, 0x0]
[PROC-EXIT] pid=905 tgid=905 name=/usr/local/bin/cargo code=1
```

Recurred at least 4 times in the same session across distinct child
`cargo` processes (pid 905, 906, 907, 908), all spawned via `fork`/`clone`
from a small set of parents (882, 901-904). Every hit lands at the **same**
`ELR=0x402082cc` / **same** faulting instruction `0x38401423`
(`Rn(base)=x1, Rt(dest)=x3`), with a different `FAR` each time — the `FAR`
values are all in the same narrow low-address band (`0x338cb*`), which looks
like the same user-space structure at a slightly different stack/heap offset
per process rather than a random address.

Each faulting process exits with code 1 (`[PROC-EXIT] ... code=1`) and the
kernel's own diagnostic explicitly flags the pattern: **"Kernel accessing
user-space address ... stale TTBR0 or dereferencing user pointer from
kernel."** The VM did not visibly wedge — other processes kept running and a
`SIGUSR1` (`sig=10`) delivery/`sigreturn` sequence shows up shortly after in
the same log window — but the affected `cargo` children died.

## Why this isn't just filed as a duplicate of a known TTBR0 bug

Several previously-fixed bugs in this repo have the exact same signature
("kernel dereferences a stale/wrong TTBR0 after fork") —
[[project_relr_fork_parent_entry_point]] (RELR trampoline running a stale
`Process`), [[project_cow_fork_mmap_region_extent]] (CoW fork losing mmap
extents), [[project_vfork_stale_ttbr0]] (`vfork_process` stale TTBR0). This
could be a recurrence, a fourth variant, or something specific to the volume
of concurrent `fork`/`clone` a `cargo install` build-script fan-out produces
(4+ siblings forking near-simultaneously across cores 0 and 3 in the capture).
It has **not** been diagnosed enough to say which; treat it as its own bug
until proven identical to one of those.

## Reproduction

Not yet isolated to a minimal repro. What's known to trigger it:

```
cargo install cargo-binutils
```
run inside a self-hosting `devbox-smoltcp` guest, on the branch/kernel active
2026-08-25 on `even-more-fixes` (post `169e799c` "kernel self hosting loop").

## Next steps (all done, kept for the record)

- ~~Reproduce in an isolated instance~~ — done, see "Fix" above.
- ~~Resolve `nr=49` and the userspace return address~~ — `nr=49` is `CHDIR`
  (`src/syscall/mod.rs`'s `nr::CHDIR`); the return address is
  `std::process::Command`'s fork-then-`chdir`-then-exec path.
- ~~Check whether it's specific to `cargo install`~~ — it's specific to any
  build with enough concurrently-live threads/heap arenas to cross 2048
  aggregate sibling mmap regions; `cargo install` just gets there fast.
