# `/proc/<tid>/maps` walked a thread group's page tables under the wrong lock

**Found:** 2026-10-21, ryzen bare metal (amd64, SMP). **Fixed:** 2026-10-22.
**Status:** fix landed and builds; **root cause is the best fit, not reproduced.**

## Symptom

A kernel `#PF` in ring 0 while an agent (goose, on Kimi) ran
`find / -maxdepth 6 -type d -name 'elf-0.7*'`, which descends into `/proc`:

```
[EXCEPTION] #PF page fault err=0x0          (read of a non-present page)
rip=0xffffffff802e1a5f  cs=0x8  cr2=0xffffa0200a6d5000
cr3=0x36c8e000  freed=0  core=1  task_slot=65  slot_root=0x36c8e000
```

Symbolized against the booted kernel (`/boot/akuma-amd64`, the 21:47 build; a
different build gives an unrelated stack — check the binary first):

```
procfs::tree::list -> for_each_live_member -> fd::pid_map_rows
  -> SlotTable::with_active_mut -> x86_walk_leaves (collect_leaf_runs)
  -> akuma_pagetable::amd64::lookup<KernelPhysMap>        <- #PF
```

`cr2 - PHYSMAP_BASE` is physical `0x200a6d5000`, ~129 GiB. The physmap covers 64 GiB
(`PHYSMAP_LIMIT`) and the machine has far less RAM: the walker followed a **garbage frame
field** out of a page table. It was not zero and not PMM poison (poison has bit 0 clear, so
the walk would have skipped it as not-present) — it was a frame that had been freed and
reused for other data. `cr3 == slot_root` and `freed=0` say the tables being walked were not
the faulting thread's own.

## Cause (best fit)

`fd::pid_map_rows(pid)` locked **`pid`'s** `Process::address_space` and walked its L0.
For a `CLONE_VM` thread, that L0 is the thread-group leader's, but the thread's
`address_space` lock is a *fresh* one (deliberate; see the comment in `Process::new`) — the
lock that serializes the tables is the leader's. So taking the member's lock excluded
nothing: the leader's `execve` (`install_image` drops the old `UserAddressSpace`, freeing its
table frames) or exit ran concurrently with the walk, or a member outlived its leader's
tables with a stale root. `find` over `/proc/<pid>/task/<tid>/…` is exactly what reaches
members. Multi-threaded `exec`ing programs (cargo, tokio runtimes, agents) make the window
real.

Not shown: the exact interleaving. No dmesg survived (the box rebooted), and nothing
reproduces it on demand.

## Fix

1. `akuma_exec::process::address_space_owner_of(pid)` — resolves a member to the owner
   that holds its L0 (`children.rs`). `pid_map_rows` now reads the regions from, and walks
   the tables of, the **owner**, and after taking the owner's lock re-checks that the owner's
   L0 is still the one resolved (and not shared); if an `execve` swapped it, the file reads
   as absent instead of walking freed tables. As a side effect `/proc/<tid>/maps` now shows the
   leader's regions (a member's own list is empty).
2. `KernelPhysMap::table_addr` (the walker's physmap view in `akuma-mmu`) asserts
   `akuma_primitives::addr::physmap_contains(pa)` and names the frame. Before, it was a bare
   `pa + PHYSMAP_OFFSET`: the `akuma-pagetable` docs said a frame outside the physmap
   "fails loudly", which was true of `amd64/src/phys.rs::phys_to_virt` but never of this
   one. A bad table is now a panic with the frame in the message rather than an anonymous
   `#PF`. `PHYSMAP_LIMIT` has one definition (`akuma-primitives::addr`); `phys.rs` takes it
   from there.

## Verified

- `cargo test -p akuma-primitives` — the limit and the amd64 comparison
  (`below_physmap_limit`, including the 129 GiB frame from this fault) are pinned.
  `physmap_contains` is the identity-true answer on the host.
- `cargo check`/`clippy` for `akuma-amd64` on `x86_64-unknown-none` and the three touched
  crates on the host; `akuma-mmu`, `akuma-exec`, `akuma-pagetable` host tests pass.
- **Not** run on metal. There is no repro, so a green run proves nothing; what to watch is
  below.

## Still open

- The race itself is unreproduced. To exercise it: a multi-threaded process that
  `exec`s in a loop while another process `find`s `/proc/<tid>/maps`.
- The fix covers the `maps`/`statm` walk. Other foreign-pid walkers that lock a member's
  `address_space` should be audited for the same lock choice
  (`COW_PILE_AUDIT.md` §2 already flags "three fresh locks on a shared address space").
- If the assert fires, its message gives the bad frame and the stack names the walker — that
  is new information, not a regression.

## Background

`docs/archive/AMD64_TLB_STUCK_MMAP_REGIONS.md` and
`docs/archive/AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md` also have `pid_map_rows` on their
stacks; `docs/archive/PAGE_TABLE_UAF_BKL_STORM.md` is the earlier page-table-frame-freed-under-a-peer
incident this one resembles.
