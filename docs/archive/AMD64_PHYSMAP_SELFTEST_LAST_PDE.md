# `physmap: reaches PHYSMAP_LIMIT` [FAIL]: bisected to `410117ce` (akuma-fbdev + akuma-fbcon)

**Date:** 2026-10-18. **Status:** breaking commit found by rehearsal bisect; the
corruption *mechanism* is still a theory (boot-stack overflow is the prime one). One
instrumented rehearsal closes it — see "How to finish this".

## The symptom

Every multiboot2 (GRUB) boot with tests enabled ends:

```
  physmap: reaches PHYSMAP_LIMIT   [FAIL] got 0xffffffffffffffff want 0xffffff000
  physmap: 4 GiB does not alias physical 0   [OK]
Akuma/amd64 self-test: 803 passed, 1 FAILED
```

`got 0xffffffffffffffff` is `translate()` returning **None** — the software walk of
the live page tables found a **not-present** entry for the last 4 KiB page of the
64 GiB physmap (`va 0xFFFF_80FF_FFFF_F000`). The walk descends PML4[256] →
`__pdpt_high[63]` → `__pd0[63][511]`; kernel execution proves PML4[256] is live, so
either `pdpt_high[63]` or `pd0`'s **very last PDE** reads as zero in RAM. The 4 GiB
probe right below it passes, and so does everything else — the machine boots and
runs normally.

## It happens everywhere multiboot2 happens (it was never metal-only)

Verified 2026-10-18, all with the same signature:

| where | loader | accél | kernel | verdict |
|---|---|---|---|---|
| trashcan metal (HP, `d6ed6691`, Mac-built) | GRUB mb2 | — | audit + baseline | **FAIL** (both; the control run) |
| ryzen metal `klog-30..34` (2026-10-06, five boots in a row) | GRUB mb2 | — | plain builds | **FAIL** ×5 — so it **predates the whole unsafe audit** |
| ryzen QEMU rehearsal, entry 3 (q35, OVMF→GRUB→mb2, `-cpu host`) | GRUB mb2 | KVM | ryzen-built tests kernel | **FAIL** |
| same | GRUB mb2 | **TCG** | same | **FAIL** |
| same, `MEM=2048` (single low RAM run) | GRUB mb2 | KVM | same | **FAIL** |
| same, entry 4 (**`nofb`**) — same binary, same GRUB, same QEMU | GRUB mb2 | KVM | same | **804 passed, 0 failed** |
| Mac QEMU microvm 2 GiB / 16 GiB (PVH `-kernel`), HEAD, Mac-built | PVH | TCG | tests-enabled | **OK** (both) |
| ryzen rehearsal of **`c7639eb3`** (the 64 GiB change, Sep 12) | GRUB mb2 | KVM | tests-enabled | **OK** (659/0) |

Ruled out by that matrix, in order: bare metal vs VMM (it reproduces under QEMU),
KVM vs TCG, the two-run RAM shape (16 GiB microvm has the trashcan's exact
3+13 GiB split and passes), the memory-map clamping in `mem.rs`, CPU features
(`-cpu host` fails), the Mac vs ryzen toolchains (the ryzen-built Oct 15 tests
kernel fails in the rehearsal; Mac-built HEAD passes under PVH), and the 64 GiB
physmap raise itself.

The surviving split is brutal: **every pass boots through the PVH entry
(`kmain`), every fail through GRUB/multiboot2 (`kmain_mb2`) — and among mb2
boots, the flip is exactly "framebuffer console came up" vs `nofb`.**

## The bisect

`overlays/ryzen/qemu.sh` rehearses the *same chain as the metal* (OVMF → GRUB →
multiboot2) on ryzen itself, and with `KERNEL=<path>` it boots any binary. A
variant of `bisect.sh` that builds **without** `--features no-tests` (the stock
one compiles the self-test out) and greps the serial log for the verdict:

```sh
# on ryzen, as root, one verdict per commit (KERNEL= boots the tests build on entry 2)
git -C /home/netoneko/akuma-metal/akuma fetch --deepen=2000 origin   # c7639eb3 had fallen off the shallow clone
# worktree build per commit (no no-tests), then:
KERNEL=$K sh overlays/ryzen/qemu.sh 2 90 std   # grep "physmap: reaches" serial-2.log
```

`c7639eb3` (good) .. `3d05c20` (bad), one probe per midpoint, each verdict
deterministic:

| commit | date | verdict |
|---|---|---|
| `c7639eb3` unlock more memory on amd64 (1→64 GiB) | 09-12 | OK |
| `ceb6a3a3` hda codec verbs | 09-28 | OK |
| `a6a8fc60` fpcache-rw | 10-03 | OK |
| `771452b7` more amd64 shared cache fixes | 10-03 | OK |
| **`f7465da1` akuma-fbdev checkpoint** | 10-03 | **OK** (re-ran to confirm) |
| **`410117ce` akuma-fbdev + akuma-fbcon** | **10-03** | **FAIL** |
| `5ceb3bb0`, `480bede4`, `2e6bdbcf`, `94eda586`, HEAD | 10-03..18 | FAIL |

**The breaking commit is `410117ce` — "akuma-fbdev + akuma-fbcon" (2026-10-03).**
The timing also explains the historical record: ryzen's first metal boots were
2026-10-06, three days after the commit — `klog-30..34` were simply the first
mb2+fb boots that ever ran this self-test.

## What `410117ce` touches, and the one theory that fits the geometry

The commit's boot-path surface is small: `fbdev::register` (a new static),
`FB_MUTED`, and — the interesting one — **`MAX_COLS` 160→240, `MAX_ROWS` 56→68**
("98 KiB of cells") plus two new baked font cuts. Nothing in the diff writes page
tables.

The failure's shape is what pins the mechanism. The boot region is laid out
(`amd64/src/boot.s`):

```
0x202000 __pml4 / pdpts
0x206000 __pd0        (256 KiB, 64 page directories)
0x246000 __ap_pml4    (4 KiB)
0x247000 __boot_stack (256 KiB, grows DOWN)
```

A stack that overflows `__boot_stack` by **just 4,104 bytes** walks down through
`__ap_pml4` — whose contents the BSP never reads (only `smp::ap_boot_root`
rewrites it, before any AP boots, on SMP boots) — and clears the present bit of
`__pd0`'s **entry 32,767**, the exact PDE whose loss this self-test and *nothing
else* observes: the region it maps is past the RAM of every machine in the fleet
(14–16 GiB), and on SMP boots the AP root is rebuilt from scratch anyway. The
overflow also explains the *silence*: no panic, no fault, no other symptom, on any
machine, ever — just one self-test line. The `nofb` flip fits because
`init_console` (surface fill, `Console::new`, font choice, grid bring-up) is the
deepest thing `kmain_mb2` does before `early_init`, and PVH never does it at all.
`boot.s`'s own comment already warns this stack "grows DOWN into the page
directories declared immediately above it" — the ordering was chosen to make an
overflow survivable, and this is what survivable looks like.

**Not yet proven.** The link between this commit's changes and ~4+ KiB of extra
boot-stack depth is not established — the grid itself was already built straight
into the `CONSOLE` static precisely to keep it *off* the boot stack. What the
commit plausibly deepened (a bigger grid in some console path, a new local, a
shift in inlining) is exactly what one diagnostic run answers. Alternative
mechanisms (something in the fb path writing zeroes over `__ap_pml4` +
`pd0`'s tail, or a PAT/`map_wc` side effect) are not excluded; `map_wc`'s own
range arithmetic was checked and only ever touches the framebuffer's directories
(dir 3 on the rehearsal), never dir 63.

## How to finish this (one rehearsal, no metal)

1. In `physmap_covers_its_limit` (or beside it), print:
   - `lookup()`'s `Missing { level }` for the failing VA — one line says whether
     it is `pdpt_high[63]` or `pd0[63][511]`;
   - the raw `u64`s at `__ap_pml4` (all of it should be **zero** on a `nosmp`
     boot before SMP init) and `__pd0[63][504..512]`.
2. Rebuild, rehearse entry 3 on ryzen. Non-zero `__ap_pml4` on a `nosmp` boot is
   the smoking gun for the overflow; its last 8 bytes vs `pd0`'s tail says how
   deep it went, which points back at the frame that overflowed.
3. Fix options once confirmed, cheapest first: move `__boot_stack` **below** the
   page tables in `boot.s` (an overflow then dies loudly instead of editing the
   map), grow the stack, or shrink whatever local brought the frame past 256 KiB.
   Note `physmap_covers_its_limit`'s promise: this test exists so that a drift in
   the boot map is "a failed self-test rather than a `phys_to_virt` that returns a
   plausible pointer into nothing" — it caught exactly that, four days late.

## Why it matters beyond one test line

The corrupted PDE covers physical `0xFFE0_0000..=0xFFFF_FFFF`. On today's fleet
that is unbacked and unreachable — but the check exists to keep `phys_to_virt`
honest, and a machine with RAM above ~63.99 GiB (or any future user of the top of
the window) would be silently missing its last 2 MiB. It is also the second bug
found where the mb2 entry path does real work the PVH rigs never exercise; every
QEMU/Firecracker rehearsal of the boot suite is PVH-shaped, and the self-test
kernels only ever met GRUB on metal, where nobody greps the verdict line.

## Background

- [`AMD64_METAL_CONTROL_RUN_2026_10_18.md`](AMD64_METAL_CONTROL_RUN_2026_10_18.md)
  — the trashcan control run that proved the failure pre-existing and wrongly
  suspected MAXPHYADDR / 1 GiB pages (a software walk reads RAM; CPUID cannot
  make a present entry read as absent).
- [`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md) — the
  audit whose verification surfaced the failure.
- [`AMD64_MAC_BUILT_KERNEL_METAL_HANG.md`](AMD64_MAC_BUILT_KERNEL_METAL_HANG.md) —
  unrelated in mechanism (that one has no kernel output at all), but the same
  lesson: the mb2 path on real hardware is under-observed.
- The probe rig: `overlays/ryzen/qemu.sh` (rehearsal, `KERNEL=` override),
  `overlays/ryzen/bisect.sh` (stock version builds `no-tests` — this bug needs
  the tests-enabled variant above). Failures are deterministic per binary; each
  rehearsal is ~2–4 min and touches nothing on the metal.
