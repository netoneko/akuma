# One `brk` for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. The last call of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md)'s goal.*

## Why

After step 6 `brk` was the one memory call still answered per kernel:

- **amd64 never moved the break.** Its dispatcher answered the current break for every request, so
  musl's `__expand_heap` fell back to `mmap` for everything.
- **AArch64 grew it with no checks** (`Process::set_brk`):
  - It mapped every unmapped page up to the new break and skipped any page already mapped. A
    break grown over a `MAP_FIXED` mapping or another image therefore silently aliased it, and
    the heap's next stores landed in that mapping.
  - An allocation failure skipped the page and still reported success, leaving a hole the program
    believed was heap.
  - The `map_and_track` result was dropped.
  - A shrink unmapped nothing, so a regrowth handed back the old bytes rather than zeros.

Routing amd64 to that as it stood would have been worse than amd64's refusal. A static-PIE
program loads at `0x1000_0000` and the interpreter at `0x3000_0000`, in no region, so the heap
of a 250 MB program (Chromium) runs into the interpreter.

## What changed

`Process::set_brk` is both kernels' `brk` (amd64's dispatcher calls glue's `sys_brk`):

- **Growth is refused**, answering the current break as Linux does, when:
  - it asks for more pages than the PMM has free (this also bounds the checks' walks);
  - any region overlaps it;
  - any page in it is already mapped (amd64's interpreter is in no region).
- **Out of memory or a failed map** part-way frees what this call mapped (the ledger entry
  included) and refuses.
- **Shrinking unmaps** the pages past the new break, so a regrowth reads zeros, as on Linux.

Two AArch64 pieces went, because the guard read them as someone else's mapping:

- **The lazy heap region.** `exec` registered a demand-paged zero region from the initial break
  up to the mmap window (`compute_heap_lazy_size`). A `MAP_FIXED` inside it split it, and the tail
  piece then refused every later growth (`brkgrow`'s last case). With `set_brk` mapping its own
  pages the region only reserved VA that the mmap window, which starts 256 MiB above the image,
  already leaves free. Both kernels now have the heap in no region. `mprotect` reprotects present
  pages whatever records them, and `MADV_DONTNEED` and fork know the heap.
- **The loader's 16 pre-mapped heap pages** (`akuma-elf`'s `load_elf_with_stack`). They made the
  first growth refuse, since the pages above the break were already mapped.

Probe: `userspace/forktest/c_stress/brkgrow.c`, in `scripts/mem_suite.py`. It checks grow,
shrink then regrow zeroed, refusal over a `MAP_FIXED` page, refusal of a 1 TiB request, and growth
again after that page is unmapped.

amd64's boot check `mmap: the lazy path was actually taken` became a note. The suite's static
musl programs now take their heap from `brk` and fault nothing lazily, so a zero there no longer
means the lazy arm regressed; `mem_suite` is what proves it.

## Divergences, pinned

| | AArch64 before | amd64 before | now (both) |
|---|---|---|---|
| growth | always "succeeds" | never | maps zeroed pages, or refuses |
| growth over another mapping | aliases it | — | refused |
| OOM during growth | holes, success | — | undone, refused |
| shrink, then regrow | old bytes | — | zeros |

## Verification

- clippy; host tests.
- AArch64 HVF boot suite SMP 1/2/4 and amd64 TCG SMP 1/2/4: no failures. amd64 has one check
  fewer (the lazy-path check above became a note).
- `mem_suite` 14/14 on the AArch64 devbox (SMP=4) and amd64 (SMP=2), `brkgrow` included. The
  `shm*` and `mapnoreplace` probes pass on both.
- AArch64 devbox self-host `-j4`: EXIT=0 in 1m02s, no tripwire lines.
- amd64 self-host on the ryzen (Firecracker guest, `kbuild -c -j 4 --features no-tests`), the
  first with musl's heap on `brk`: EXIT=0, 127 crates, one benign `[MM] fault race`, 10
  `[BKL] stuck` lines, no panics.
