# `mprotect` could not downgrade a cached translation — wrong TLBI ASID scope

**Status: FIXED 2026-08-05.** Found while chasing the `-j4` self-host
thread-spawn SIGSEGV (`docs/runbooks/debug-thread-spawn-segv.md` §2b carries
the maintained summary; this doc is the archived record). Not shown to be the
cause of that SIGSEGV — it is a real, independent defect found en route.

## Symptom

A `PROT_NONE`/`PROT_READ` downgrade via `mprotect` on a page some code had
already touched (and therefore already had a cached, writable TLB
translation) silently did not take effect. The PTE was correctly rewritten in
memory, but the stale, more-permissive translation stayed live in the TLB and
kept being used:

- musl's `pthread_create` — `mmap` the stack, then `mprotect(guard,
  PROT_NONE)` — left the guard page **writable**, so a stack overflow
  silently scribbled on the next mapping instead of faulting.
- A dynamic loader's RELRO `mprotect(GOT, PROT_READ)` left the GOT
  **writable**.

## Root cause

`flush_tlb_range` invalidated with `tlbi vale1is, va>>12`. That instruction
takes its target ASID from operand bits **[63:48]**. `va >> 12` of any user
VA leaves those bits zero, while every user process runs under a non-zero
ASID — so the invalidation matched no cached entry at all. `sys_mprotect`
(`src/syscall/mem.rs`) publishes its PTE edits through exactly this function,
so every permission downgrade on an already-touched page was silently a
no-op at the TLB level.

## Fix

Widened the invalidation to `tlbi vaae1is, va>>12` ("VA, All-ASID"). This is
*required*, not merely conservative: `UserAddressSpace::new_shared` allocates
a fresh ASID while reusing the parent's `l0_frame`, so one L0 table can be
live under several ASIDs at once (CLONE_VM threads, vfork-fastpath
children), and a single PTE edit has to invalidate the translation under all
of them — a plain per-ASID `vale1is` targeted at the *correct* ASID would
still miss the other live ASIDs sharing the same table.

## Regression coverage

`userspace/forktest/c_stress/mprotectlb.c` — deterministic, one
mmap/touch/mprotect/access cycle per phase. Measured **3 FAIL before, 3 PASS
after, 3 PASS on real Linux aarch64** (calibrated).

## Background

- `docs/runbooks/debug-thread-spawn-segv.md` §2b — maintained summary, part
  of the wider thread-spawn-SIGSEGV investigation this was found during.
- [`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md)
  §5.3b — current-state pointer from the thread-lifecycle reference doc.
