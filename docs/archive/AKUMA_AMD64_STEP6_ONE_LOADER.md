# amd64 C1 step 6: one ELF loader

**Date:** 2026-09-08
**Status:** landed. Four rigs green, ring-3 verified on QEMU **and** on the metal,
including the dynamic-linker path that no boot check reaches.
**Parent:** `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md`, whose § "The layout
collision" is the measurement this step acted on.
**Previous:** `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md`.

---

## What this step did

`amd64/src/loader.rs` was a second ELF loader beside `crates/akuma-elf`: it
parsed the headers, placed the `PT_LOAD` segments, read `PT_INTERP`, mapped the
dynamic linker, and then built the initial stack. The **first four** of those are
`akuma-elf`'s now. The fifth stayed, and where the line falls is the whole of the
decision.

```
 740 lines  ->  502 lines
 place_image / load / Placed / PIE_BASE / INTERP_BASE / segment_prot
 USER_VA_LIMIT-per-segment / the `elf` 0.7 direct dependency      — gone
 build_stack / map_range / write_user / widen / the auxv          — kept
```

`amd64` no longer depends on `elf` directly. The tree had one ELF *parser* and
two ELF *placers*; it has one of each.

## The shape chosen, and why it was not (a) or (b)

The hand-off offered three: (a) move amd64 onto `akuma-elf`'s layout, (b)
parameterise `akuma-elf`, (c) take the loading and keep the placing. **(c)**, and
the reason is one number.

| | amd64, before | `akuma-elf` | outcome |
|---|---|---|---|
| `PIE_BASE` | `0x1000_0000` | `0x1000_0000` | identical |
| `INTERP_BASE` | `0x4000_0000` | `0x3000_0000` | **moved, and it is fine** |
| stack top | `ELF_STACK_TOP`, fixed | `compute_stack_top`, capped `0x40_0000_0000` | **cannot move** |

The interpreter base moving from `0x4000_0000` to `0x3000_0000` costs nothing:
both sit in the same hole, above a static-PIE program at `PIE_BASE` and 3.75 GiB
below `mm::MMAP_BASE` (`0x1_0000_0000`). A program would have to be 512 MiB to
reach the linker from `PIE_BASE`. It is carried as a decision, not absorbed as an
accident — and it is the one thing in this step that had to be proven in ring 3
rather than at boot, because nothing in the boot suite loads a dynamic binary.

`compute_stack_top`'s cap is what stops (a). `0x40_0000_0000` is **inside**
`mm.rs`'s mmap window `[0x1_0000_0000, 0x7000_0000_0000)`, the stack is not in
the region list, and `MMAP_TOP` was chosen at 112 TiB precisely so the fixed
`ELF_STACK_TOP` sits outside the window *by construction rather than by collision
test*. An `akuma-elf`-placed stack breaks that construction: `find_free_va` would
hand out the stack's own pages, and the failure is a `SIGSEGV` in ring 3 with no
message. (b) was rejected for its cost rather than its correctness — it touches
the AArch64 kernel's `.text`, which two prerequisites went to some trouble to
keep byte-identical.

## Tallies

Every rig at `SMP=4`. Six new boot checks; every arm is baseline + 6, zero
failures.

| rig | before | after |
|---|---|---|
| QEMU/TCG | 508 / 0 | **514 / 0** |
| Firecracker (the box, KVM) | 497 / 0 | **503 / 0** |
| OVMF/GRUB (the box, multiboot2 under KVM) | 501 / 0 | **507 / 0** |
| bare metal | 501 / 0 | **507 / 0** |

Also held: host tests **1360** passing / 143 suites; the ten `c_stress` memory
probes at **8/10, 0 unexpected** (`scripts/utils/amd64_mem_trials.py`); clippy
clean on the amd64 kernel, the AArch64 kernel and `akuma-syscalls-glue` for
`x86_64-unknown-none`.

**The AArch64 kernel is byte-identical**: `.text` 3 207 052, `.rodata` 270 368,
`.data` 199 896, all three unchanged against `HEAD`. The one shared-crate edit
that is *not* `cfg`-gated — `interp.rs`'s `EM_AARCH64` -> `EM_NATIVE` — resolves
to the same constant on that architecture, which is the point of the fix.

### The ring-3 checks, and why there are two

The boot suite runs under `BypassValidationGuard` and never loads a
dynamically-linked binary — `/bin/busybox.dyn` has been on the amd64 image since
2026-09-06 and **nothing ran it**. So the interpreter path, which is the only
part of the layout this step actually moved, had no coverage at all from a tally.

* **QEMU, `init=/bin/busybox.dyn`**: the stock Alpine `ET_DYN` +
  `PT_INTERP=/lib/ld-musl-x86_64.so.1` busybox as pid 1, printing from ring 3
  after `ld-musl` self-relocated at the new `INTERP_BASE`.
* **QEMU over ssh**: `busybox.dyn echo` / `uname -a` / `ls /lib`, plus 149
  process lifetimes (20 pipelines, 30 static spawns, 20 **dynamic** spawns, a
  `find /` and a 16 517-line `grep -r`). `free` delta **0 KiB**.
* **Bare metal over ssh**: the same 149 lifetimes, `free` delta **0 KiB**, and
  `busybox.dyn` answering on real hardware.

## Two things fixed on the way in, both older than this step

### `akuma_elf::interp` compared `e_machine` to a literal `EM_AARCH64`

`load.rs` has had a `cfg`-selected `EM_NATIVE` since the crate was wanted on
x86_64; `interp.rs` did not, and nothing noticed because no x86 caller had ever
reached the interpreter path. It would have refused **every** dynamically-linked
binary here — after loading the program successfully — with an error naming the
interpreter rather than the mismatch. `EM_NATIVE` is `pub(super)` now, so there
is one constant and one answer.

### amd64 never registered `akuma-elf`'s VFS hooks

`amd64/src/exec_runtime.rs::init` calls `akuma_exec::runtime::register`, not
`akuma_exec::init` — deliberately, because that function registers seven upward
surfaces at once against subsystems this target serves itself. `akuma-elf`'s four
callbacks are registered there now, explicitly. Unregistered, `crate::vfs()`
`require()`s and panics, so the first dynamic binary would have died with
"VfsHooks not registered" rather than reading zeros. That failure mode is the
crate's design working; it still had to be wired.

## The finding: the x86 walk had no upper-half guard

This is the part of the step that was not a port.

`amd64/src/loader.rs` refused a `PT_LOAD` whose extent left the lower half —
`seg_end > USER_VA_LIMIT`, one `if` in one caller, against a `p_vaddr` read from
a file ring 3 chose. `akuma-elf` has no such check, and neither did anything
below it.

`akuma_mmu::UserAddressSpace::new` on x86 **aliases** the kernel's PML4 slots
256, 257 and 511 into every user root rather than copying them — that is what
makes one kernel mapping that cannot drift. So a walk from slot 256 up is a walk
through the *live kernel tables*, and `x86_next_table` widens every intermediate
entry it descends through to `P|RW|US`. One `map_page` at an upper-half VA does
not corrupt one process: it makes a kernel table user-accessible in **every**
address space and installs a leaf in it. No fault, no message.

The guard is now inside `x86_map_page_in`, the single funnel `map_page`,
`map_page_pte` and `map_and_track_pte` all pass through. It belonged to the walk
all along rather than to the one caller that happened to remember it.
x86-only — AArch64's `map_page` indexes `(va >> 39) & 0x1FF`, which folds an
upper-half VA back into the process's own `TTBR0` L0: wrong, but confined to the
address space that asked for it.

`mm.rs`'s `USER_VA_LIMIT` and `akuma_syscalls_mem`'s `MAP_FIXED` half-space check
stay where they are. They answer ring 3 with `EINVAL`, which is a different job
from refusing to write the table at all.

### What the boot check had to be, and what it caught

`uas::upper_half_refusal_test` probes **two** slots, because they fail
differently:

* **slot 256** is the physmap and is *present* in the root by aliasing, so an
  unguarded map allocates nothing and simply writes a leaf into a live kernel
  table — invisible to any free-count check;
* **slot 300** is absent, so an unguarded walk allocates three page-table frames
  on the way down. `page_table_frame_count() == 0` is the discriminator.

Its first run failed, and on something real: `alloc_and_map` takes its frame from
the PMM **before** the map and, unlike `map_and_track_pte`, neither untracks nor
frees it when the map fails. Nothing is lost — the ledger holds it, so `Drop`
returns it — but the address space is left owning a page nothing maps, and the
two "install a frame" entry points disagree about what a failed install leaves
behind. It is the same body on both architectures (`akuma-mmu`'s
`map_and_track`), and this step was simply the first thing ever to take that
error arm on purpose. **Asserted rather than tidied**: the check pins the current
answer and names it, because fixing it changes the AArch64 kernel's OOM path and
wants its own change and its own A/B.

## Divergences this fold carries

Each is stated at `loader.rs`'s module header as well, so a reader of the code
does not have to find this document.

Refusals `place_image` made that `akuma-elf` does not:

* **W+X segment.** Refused outright before; `akuma-elf` enforces W^X by
  construction (`SegProt` has two variants and `PF_X` wins), so such a segment
  loads read-execute and faults on its first write. Strictly safer, less
  legible. `userspace/amd64/user.ld` page-aligns every segment, so neither
  answer is reachable from our own image.
* **`p_filesz > p_memsz`.** Refused before; now the copy is bounded by the
  `memsz` page span and the excess silently goes nowhere. A refusal that became
  a shrug.
* **Segment outside the lower half.** Not lost — moved down into the walk, above.
* **Entry point.** Kept: `load` checks it is a non-zero user address and reads
  its executability back out of the page tables, which costs no second parse.

Gaps `akuma-elf` closes for free, recorded so they do not vanish:

* A `PT_INTERP` of one NUL byte — what a static-PIE emits — used to be read as
  an empty path and turned into a failed `read_file`; skipped now.
* `PN_XNUM` and a bad `e_phentsize` are refused in `parse_headers`, and every
  header field is read through the bounds-checked `elf` 0.7 crate rather than by
  `loader.rs` re-doing the same reads at literal offsets.

Owed, not closed: `build_stack` assembles its word block in a
`[u8; STACK_WORDS_MAX * 8]` on the kernel stack, so `MAX_ARGV` (16) and
`MAX_ENVP` (32) are still hard caps. `akuma-elf`'s `setup_linux_stack` builds on
the heap and has none — but taking it means taking its auxv (fourteen entries
against this target's seven) and its stack placement, which is the layout
question above.

## One boot check changed rather than moved

`elf: every PT_LOAD was placed` compared `img.segments` to a count read
independently out of the image. `akuma_elf::LoadedElf` reports no segment count,
and re-deriving one from the same headers the loader read would have made the
check agree with itself. It is now `elf: every PT_LOAD is mapped`, asked of the
**page tables**: both ends of every segment are probed, so a loader that mapped a
segment's first page and stopped fails here rather than in ring 3. Strictly
stronger, and the tally is unchanged.

## Background

- `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` — the measurement this acted on.
- `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md` — the prerequisite: one x86
  user-space walker, which is what let `loader.rs` name `UserAddressSpace` at
  every signature.
- `docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md` — the x86 `UserAddressSpace`,
  and the PML4 aliasing the upper-half guard exists because of.
- `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3 — why there is one ELF
  parser, which is the argument this step finished by giving it one placer.
- `docs/runbooks/amd64-bare-metal-loop.md` — the four rigs.
