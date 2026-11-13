# B8: one ELF parser and one page loop in Taoxichuan's loader (2026-11-02)

**Stability: B**: the loader changed for every exec; gates below.

Step 2g of [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md) (B8's rest): `load_task` onto `akuma-elf-exec`.
Before it, Taoxichuan parsed a program with `akuma-ring3-amd64`'s `Image` and its interpreter with `akuma-elf-exec`,
and mapped each through its own copy of the page loop. Doing the move found two defects in the loop the two copies did
not share.

## What changed

- **`akuma_elf_exec::program`** (new): the program's `PT_LOAD` segments at the base its type decides (`PIE_BASE` for a
  static-PIE, else 0) and the facts the loader needs besides: entry, `AT_PHDR` (`PT_PHDR` when present, else computed
  from the segment that file-backs `e_phoff`), `AT_PHNUM`, and the image end where `brk` starts. `PIE_BASE` and `PHENT`
  moved into the crate with it. 5 new host tests.
- **`load_segments` checks segment bounds**: a segment whose file bytes run past the image, or whose `memsz` is smaller
  than its `filesz`, is `ExecErr::SegmentBounds`. Taoxichuan is its only caller.
- **Taoxichuan's `place_segment`**: one page loop for program and interpreter (a fresh zeroed frame per page, the
  file-backed slice copied in from `p_offset`), refusing a segment whose end overflows or reaches past `USER_TOP`.
  `USER_TOP` is one constant now; it was four literals in `main.rs` and a local in `child.rs`.
- **`ring3::Image` is deleted** with its types (`Segment`, `Prot`, `LoadError`, `MAX_SEGMENTS`, `PIE_BASE`) and its four
  tests; `akuma-elf-exec`'s cover the same cases. `akuma-ring3-amd64` keeps the stack builder and the crossing.

Image audit 77,994 -> 77,836 (`scripts/cloc_akuma.py --audit taoxichuan`); `taoxichuan/src` 3,588 -> 3,565 audit lines.

## Found on the way

1. **A truncated or hostile interpreter panicked the kernel.** The interpreter loop sliced `ldso[p_offset..p_offset +
   p_filesz]` with nothing checking that range against the file: `akuma-elf-exec::load_segments` validated the phdr
   table but not the segments it describes, while `ring3::Image` did check them for programs. Any `PT_INTERP` naming a
   short file (a half-written `ld-musl`, or one made to be short) was an index panic in ring 0. Now `load_segments`
   refuses it and the exec fails.
2. **No segment was checked against the user half.** Neither loop looked at where a segment lands, and
   `map_page_in` walks whatever slot the address indexes, widening intermediate entries with the user bit. A segment
   at or above `0x0000_8000_0000_0000` therefore walked the PML4 slots every root shares with the kernel. A
   `vaddr + memsz` that overflows wrapped the same way. Now both are refused before any frame is taken.

Neither was seen in a real run; both need an image built to trigger them, and every image the rigs run is well formed.

## Behaviour that changed

- A `W|X` interpreter segment maps `USER_RW` (writable wins), as program segments always did; the interpreter loop
  had picked `USER_RX`. No image this kernel runs has one.
- A program header size other than 56, and `e_entry == 0`, are refused (`ring3::Image` accepted a larger stride and a
  zero entry). No ELF64 toolchain emits either.

## Verified

Host: `akuma-elf-exec` 12 tests (5 new: static placement, static-PIE + `PT_PHDR`, no segment / no entry, the three
bounds refusals), `akuma-ring3-amd64` 5. Taoxichuan (Mac TCG): battery 11/11 + goose (`apk` is a static-PIE, `git` and
goose's shell are dynamic), `cow_leak_gate` delta 0, tokioprobe x3, toolchain (`rustc`, `cargo` with a build script);
the B9 signal and probe set at `smp=1` and `smp=4`. Pre-commit hook green.

## Background

- [`TAOXICHUAN_FIXED_POINT.md`](TAOXICHUAN_FIXED_POINT.md): what the loader has to bring up.
- [`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md) row 8: one ELF loader.
