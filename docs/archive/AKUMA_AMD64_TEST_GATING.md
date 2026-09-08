# amd64 gets a `no-tests` feature, and the line count stops lying

**Date:** 2026-09-09
**Status:** landed. Both configurations build clippy-clean and **both boot**;
the tested one at baseline (QEMU/TCG 515/0 `SMP=1`, 524/0 `SMP=4`), host tests
1360/0, AArch64 gate clean.

---

## The problem, in one measurement

```
$ scripts/cloc_akuma.py amd64/src
Production                    38      1754     11494     13871    100.0%
Tests                          0         0         0         0      0.0%
```

`amd64/src` reported **0.0% test** — against a boot suite of 56 functions and
~3 300 lines. Not because the counter is wrong: `scripts/cloc_akuma.py` files a
line under *tests* when its file is a test file, when it sits under `#[test]`,
or when its item's `cfg` **cannot hold in a `no-tests` world**. This target had
no `no-tests` feature, so no item could satisfy the third rule and none of the
first two applied — every self-test read as kernel code.

That is not a cosmetic complaint. `LINE_COUNT_ANALYSIS.md` is a living document
used for comparison against other kernels, and `amd64/src` was inflating the
production side by ~3 300 lines while contributing nothing to the test side.

## What landed

- **`no-tests` as a real feature of `akuma-amd64`**, matching the AArch64
  kernel's, where it is not hypothetical: `build_devbox.sh`,
  `build_devbox_smoltcp.sh` and `build_extreme_size.sh` all pass it. So this one
  is held to the same standard — it has to *boot*, not merely compile.
- **56 functions gated** — every `fn …(t: &mut Suite)` in `amd64/src` — plus
  their `use akuma_selftest::Suite;` imports, `boot::{self_tests, SuiteCtx,
  Verdict}`, and the scaffolding that exists only for them: the embedded probe
  ELFs (`HELLO_ELF`, `THREADPROBE_ELF`), the hand-assembled ring-3 program
  builder, `Image::{new, from_elf}`, the test-process helpers, the independent
  `PT_LOAD` re-reader, `run_sh_capture`.
- **`FEATURES=` in `amd64/run.sh`**, so the configuration can be booted.

```
Production                    38      1439      9901     10713     76.5%
Tests                          0       320      1668      3293     23.5%
```

**13 871 → 10 713 production lines**, 3 293 test. Nothing was deleted and no
behaviour changed; the tested build's binary and tally are unchanged.

## The one judgement call, stated

The `no-tests` build surfaced ~40 dead-code errors, and they are **two different
things**:

- Scaffolding that exists *for* the suite — gated, and that is the point.
- Ordinary kernel accessors whose only current caller happens to be the suite:
  `paging::{translate, unmap_page, unmap_page_in, walk_in, translate_in}`,
  `lapic::{stop_timer, calibrated_count}`, `sched::{preemptions, blocks, wakes,
  backstop_wakes, is_blocked}`, `fs::mount_count`, `fs::read_at`. These are the
  kernel's own surface. Filing them under *tests* would be a lie about what they
  are, and deleting them would throw away API the tested build exercises on
  every boot.

The second group gets a crate-level
`#![cfg_attr(feature = "no-tests", allow(dead_code, unused_imports))]` with the
reasoning written at the attribute. Gating them would have bought ~200 more
"test" lines and made the number less true, which is the opposite of the point.

## `self_tests` is not only a suite, and that had to be dealt with

Its own header already said so: `fd::init_console` and `usermode::init_syscall`
"sit *inside* the suite because the userspace tests after them need both". So do
`lapic::init` and `lapic::start_timer`. A build with no suite still needs all
four, or ring 3 has no console descriptor, no `IA32_LSTAR` and no preemption.

The fix was **already half-written**: `multiboot2::kmain_mb2` has a `skiptests`
command-line lever that performs exactly that sequence inline, with a comment
saying "those are real bring-up, not tests". Rather than adding a second copy,
that arm became `boot::late_init`, and the `no-tests` build calls the same
function. Its `secondaries` argument is a hook, not an argument list, because
the trampoline check needs a `keep_out` range that differs by boot protocol —
a PVH start-info block on one path, GRUB's information block or root image on
the other.

The pair that could genuinely drift — `init_console` + `init_syscall` — is
spelled once, in `boot::wire_console_and_syscalls`, called from both. The LAPIC
half stays spelled twice because the suite checks `lapic::init()`'s result and
interleaves `smoke_test`/`stop_timer` around the timer; folding those would mean
passing a `Suite` to a build that has no suite. Its absence is also not silent:
nothing schedules.

## Verified

| check | result |
|---|---|
| `cargo clippy -p akuma-amd64 --target x86_64-unknown-none --release` | clean |
| ... `--features no-tests` | clean |
| QEMU/TCG `SMP=1` | **515 / 0** (unchanged) |
| QEMU/TCG `SMP=4` | **524 / 0** (unchanged) |
| ring-3 check, `SMP=1`, 40 sessions | 40/40, `free` unmoved, `grandfork` ALL PASS |
| host tests | 1360 / 0 |
| `cargo clippy --release` (AArch64 gate) | clean |

And the configuration that only exists for the line count was **booted**:

```
$ FEATURES=no-tests SMP=1 INIT=/bin/busybox INITARGS=uname,-a sh amd64/run.sh
Akuma/amd64 — long mode reached
  Akuma/amd64 (x86_64 bring-up)  0.1.0-amd64
-- running /bin/busybox --
Akuma akuma 0.1.0 4f3737a9-release x86_64 GNU/Linux
-- init exited --
```

No self-test line, straight to init, and a real userspace program ran. The
`[SCHED] WARNING: yield_now with IRQs masked` lines it prints are **pre-existing
and not this change**: the tested build prints the same eight on the same boot
command.

## Background

- `scripts/cloc_akuma.py` — the three rules, and why `#[cfg(not(feature =
  "no-tests"))]` is treated as a test gate.
- `docs/archive/LINE_COUNT_ANALYSIS.md` — the living document these numbers
  feed.
- `docs/reference/subsystems/config-flags.md` § "The `akuma-amd64` package" —
  the feature table this adds.
