# Post-exit PMM reclaim — is there a leak at the low-memory floor?

**Question (2026-06-05):** at the extreme low-memory floor (the 4.5–5 MB
meow+tcc region) free PMM seems not to recover after a process exits, so a later
spawn / demand-fault hits "0 free pages". Are dead processes leaving artifacts
behind (un-freed user pages / page tables / heap)?

**Answer: no per-process leak.** The single-process teardown path conserves
physical memory exactly. The "never recovered" symptom is a *one-time working-set
step* (the warm thread-stack floor + VFS caches), not a ratchet — and at 4.5 MB
the binding constraint is raw PMM scarcity, not a leak.

## How the teardown frees memory (code trace)

A dying process returns its frames through, in order:

- `return_to_kernel(-N)` (normal exit, and the OOM-SIGSEGV path,
  `exceptions.rs` data/inst abort → `return_to_kernel(-11)`) →
  `unregister_process(pid)` → drops `Box<Process>`.
- `Process::drop` frees `dynamic_page_tables`.
- `UserAddressSpace::drop` frees `user_frames` (each distinct PA once — a
  *mapping* refcount, not an alloc count), `page_table_frames`, and `l0`.
- `kill_process` / `kill_process_with_signal` instead mark the process **Zombie**
  and defer the same Drop to reaping (`on_thread_cleanup` → `unregister_process`
  when the terminated thread slot is recycled).

`mmap` frames are *aliased* in `user_frames` (`syscall/mem.rs` calls
`track_user_frame`), so AS Drop frees them; `Process.mmap_regions` is just
bookkeeping (`PhysFrame` is `Copy`/no-`Drop`, so dropping the Vec frees no pages —
and that is correct, not a leak).

## Evidence (page-precise)

Self-test `test_pmm_conserved_across_spawn_exit_reap` (src/process_tests.rs):
spawns a real `/bin/hello`, drives it through exit **and** kill, forces reap
(`threading::cleanup_terminated_force()`) and `allocator::reclaim_to_pmm()`, and
asserts `pmm::free_count()` does not ratchet down across repeated cycles. Result
at MEMORY=64M:

```
clean 4x drift=0p; kill 4x drift=0p; pinnedspans 0->0
```

Live reproduction, extreme-size kernel at MEMORY=6M, page-precise `[Mem]` line:

```
baseline                         RAM free 3100KB
after 20 spawns (round 1)        RAM free 2972KB   <- one-time step
after 20 spawns (round 2)        RAM free 2972KB   <- flat (no ratchet)
after 20 spawns (round 3)        RAM free 2972KB   <- flat
after forktest_parent            RAM free 2972KB   <- fork/CoW path also recovers
```

The 128 KB step is the warm thread-stack floor (lazy stacks keep
`WARM_FREE_USER` stacks allocated by design) plus retained VFS read-ahead of the
spawned binaries — a stable working set, reclaimed-stable, not growing.

## Diagnostics added (keepers)

- **`allocator::claimed_span_report() -> SpanReport`** + a `spans:` field on the
  `[Mem]` line: how much PMM the kernel heap is sitting on and how much is *stuck*
  (`pinned` = spans Talc can't return because one live allocation pins them).
  This is the real signal for the kernel-heap **high-water mark**: a span only
  returns to the PMM when it is *entirely* free, so fragmentation by long-lived
  allocations is what would keep `pinned` high after a heavy heap workload.
- **Page-precise free RAM** (`(NNNNKB)`) on the `[Mem]` line — the MB figure
  can't show sub-MB recovery, which is exactly the floor symptom.

## What the floor symptom actually is

At these sizes the kernel heap never grows past its tiny seed (6 MB: heap "used"
~134 KB, 0 claimed spans), so the heap high-water / `reclaim_to_pmm` path is not
even exercised — it only bites under genuinely heap-heavy load (many sockets,
big compiles). The 4.5 MB meow+tcc floor is bounded by the **combined user-page
working set vs available PMM**, not by leaked artifacts. To push the floor lower,
shrink the working set (warm-stack floor, per-process metadata), not chase a
leak that isn't there.

If `pinned` is ever observed staying high after a heap-heavy workload drains,
*that* is the high-water bug and the lever is fragmentation (segregate
process-lifetime heap allocations so freed spans become wholly free again).
