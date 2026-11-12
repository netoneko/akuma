# One `mprotect` for both kernels, and the CoW write it used to grant

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 5's first call of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## Symptom

On AArch64, a forked child that `mprotect`s a private page to `PROT_READ` and back to
`PROT_READ|PROT_WRITE`, then writes it, changed the **parent's** copy:

```
MAP_PRIVATE  parent sees 0x2222 -> *** WRONG ***
MAP_SHARED   parent sees 0x2222 -> ok
cowprotect: FAIL
```

(`userspace/forktest/c_stress/cowprotect.c` on the devbox at `4a8b2596`, SMP=4.) macOS and
Linux print `0x1111` for the private leg. amd64 already passed.

## Cause

Glue's `sys_mprotect` wrote the region's protection into every present PTE with
`update_page_flags_no_flush(va, to_pte(prot))`. Fork had demoted the page to read-only and
left the frame shared (`cow_ref_get(pa) > 0`). `mprotect(RW)` then made it writable in the
child's page table while the parent still mapped the same frame, so no write fault happened
and nothing copied. The same path would also have granted a write to a shared read-only file
page from `akuma-fpcache` mapped `MAP_PRIVATE`, writing into the page every other mapper of
that file reads.

amd64's own `mprotect` (`amd64/src/mm.rs`) had the rule: a writable page whose frame is still
shared is mapped read-only and CoW-marked, unless its region is shared by identity
(`MAP_SHARED`, where CoW would mean "stop sharing").

## Fix

The two copies became one, in glue, with amd64's rule:

- `UserAddressSpace::reprotect_present(start, end, prot, keep_cow)` on both walkers. x86 is
  amd64's former leaf walk (one descent, absent subtrees skipped, CoW marker written); AArch64
  is the per-page loop glue had, with the same contract. A page user mode cannot touch is left
  alone only for `PROT_NONE`.
- `akuma_cow::marks_cow` (moved from `akuma-fpcache-rw`) decides `keep_cow`;
  `Prot::without_write` gives the read-only form, keeping execute exactly.
- amd64 dispatches `mprotect` to glue; its `sys_mprotect` is deleted.

Divergences between the two copies, and what the one implementation does now:

| | AArch64 (glue) before | amd64 before | now |
|---|---|---|---|
| CoW-shared frame, region writable | granted write | read-only + marked | read-only (+ marked on x86) |
| `addr` unaligned with `len == 0` | `0` | `EINVAL` | `EINVAL` (Linux checks alignment first) |
| range past the user half | walked | `EINVAL` | `EINVAL` (`akuma_mmu::USER_VA_END`) |
| no owner process | `EINVAL` | `ESRCH` | `ESRCH` |
| I-cache after adding execute | `dc cvau` + `ic iallu` | none (coherent) | same, `cfg(aarch64)` |

## Verification

- `cowprotect` (new, in `scripts/mem_suite.py`): FAIL on the pre-change AArch64 kernel, PASS
  after. macOS arm64 native build: PASS.
- AArch64 devbox `mem_suite` 13/13 (SMP=4).
- amd64 TCG boot suite SMP 1/2/4: 837/847/847, 0 failed. The first SMP=2 run had two
  failures (`sigregs: teardown leaks nothing`, `wait4: no children -> ECHILD`, both from a
  helper killed by signal 9 and not yet reaped) that did not reproduce on rerun.
- Host tests: `akuma-mmap` (`without_write_drops_only_write`), `akuma-cow`
  (`identity_shared_pages_are_never_cow_marked`), `akuma-fpcache-rw`.

## Open

- amd64 `munmap`, `mmap`, `mremap`, `madvise`, `msync`, `brk` are still amd64's own. `munmap`
  waits on VA placement: glue returns freed ranges to a free list that amd64's placer never
  reads.
