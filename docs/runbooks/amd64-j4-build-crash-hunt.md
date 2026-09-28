# amd64 `-j4` self-host build: what is known, and how to attempt it on the trashcan

**Stability: B.** The ryzen numbers are measured; the metal half is a checklist
for a run that has **not happened yet** — nothing here was executed on the HP box.

The question: does `kbuild -c -j 4` at SMP=4 finish on the machine, or does it die
the way [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)
§3 recorded (3 of 4 builds `rc=139`, at 22–35 crates)? Two rigs have data.

## 1. Where things stand (2026-09-29)

| rig | kernel under test | `-j4` result | source |
|---|---|---|---|
| HP box, **Firecracker** guest (Ubuntu personality, 4 vCPU / 6 GiB) | 2026-09-18 tree | ~10 clean builds (`-j1`×1, `-j2`×1, `-j4`×5, `-j8`×2, gen 2), 0 failures | `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §§14, 17, 19 |
| HP box, **bare metal** (SMP=4, 16 GiB, USB root) | 2026-09-18 tree | `-j4` **rc=139 ×2** (30 and 22 crates). `-j1`: one build compiled all 95 crates then the LLD link crashed (512 MiB heap), one `rc=139` at 35 crates, then gen-2 and gen-3 **clean** after the 1 GiB-heap fix (640 s / 632 s) | `AKUMA_AMD64_BARE_METAL_SELFHOST.md` §3–§4 |
| **ryzen**, **Firecracker** guest (4 vCPU, 4 or 6 GiB) | `0e331f5` (`even-more-cats`) | **21 of 21 pass**, `-j4`, ~1m 30s each, 10 of them with the whole `target/` wiped | [`selfhost-kernel-build-amd64.md`](selfhost-kernel-build-amd64.md) § "Second rig: ryzen" |

Read the ryzen row for what it is: a guest on KVM, zero failures in 21 (95 %
upper bound ≈ 13 % per build; ≈ 26 % for the 10 whole-graph runs alone). It made
the "the guest only passes because its vCPUs are time-sliced" explanation less
likely — ryzen has 16 hardware threads and was otherwise idle — but every other
difference from the metal (CPU, virtio-blk vs USB, 4–6 GiB vs 16, no xHCI)
stands. It does **not** say the metal is fixed.

### The two trees are not the same kernel

The trashcan, when checked on 2026-09-29, ran **`97fd2d10`** ("last fix from meow",
built on the box at 21:37 UTC, `uname` = `97fd2d10-release-smp-shared`). That commit
is on `litter/cats/meow/amd64-audio`, **60 commits past** its merge-base
(`a33988d5`) with `even-more-cats`, and `0e331f5` is 2 past it. Within `amd64/`
the difference is `hda.rs` (+788, new — HDA audio), `pci.rs`, `multiboot2.rs`,
`main.rs`, and **`sched.rs` (50 changed lines)**. **That gap is not cosmetic — it is a known hang.** The 50 lines of `sched.rs`
are `2d073ca0` (2026-09-26), which **removed `yield_now`'s dropped-window
exception** after two metal hangs of exactly this shape: `[SWITCH NO-BKL] from=4
to=2 core=2 via=yield_now`, then silence
([`AKUMA_AMD64_BKL_NETWORKING.md`](../archive/AKUMA_AMD64_BKL_NETWORKING.md)
§ "2026-09-26"). The box's tree has the exception: its
`amd64/src/sched.rs` (dated Sep 25) contains `bkl_free_by_choice` ×3 and no
"REMOVED 2026-09-26" text, read on the box 2026-09-29. **Confirmed the hard way
the same evening:** the trashcan crashed with `[SWITCH NO-BKL] … via=yield_now`
on `97fd2d10` and had rebooted (`up 0 min`, still `97fd2d10`) when checked.
Neither `litter/amd64-audio` nor `litter/main` (both tip `97fd2d10`) nor
`litter/even-more-cats` (tip `a33988d5`, 2026-09-25) has the fix — it exists
only on `origin/even-more-cats` (`2d073ca0`, `0e331f5`). The console ring dies
with the reboot, so the crash's exact `from=`/`to=`/`core=` line is whatever the
person at the box read off the screen.

**So do not attempt `-j4` on the box with `97fd2d10`: a `yield_now` hang there
would be read as a `-j4` failure and is not one.** A `-j4` result on the box
compares against ryzen only after one of these:

* **(recommended)** get `2d073ca0` into the audio branch — a merge of
  `origin/even-more-cats` (it adds `2d073ca0` and `0e331f5`; `sched.rs` differs
  only by that fix) — then build and install that on the box (`kinstall`, keep
  `.good`). Committing it is the owner's call; nothing here does it. Then the
  box and ryzen differ only by the audio code, **or**
* build `0e331f5` itself on the trashcan and boot *that*, **or**
* run ryzen's rig on `97fd2d10` (change the clone's branch in `stage1.sh`; ryzen
  needs access to the `litter` remote) to see whether the audio tree hangs there
  too — a `yield_now` hang would be expected, **or**
* accept the difference and say so with the result — but not for `97fd2d10`.

## 2. Before you touch the box

| check | how | why |
|---|---|---|
| which kernel it runs | `ssh akuma "uname -a"`; `md5sum /boot/akuma-amd64` | a result names its kernel; the file at that path is overwritten by self-builds |
| a way back | `/boot/akuma-amd64.good` exists; GRUB entry `Akuma/amd64 (known good)` | a wedge can only be undone from the machine — `nosmp` needs a GRUB edit and Akuma cannot write ext4/vfat ([bare-metal §6.1](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)). Someone at the box, or accept a power cycle |
| the console history | `ssh akuma "dmesg" > metal-pre.log` | the ring is 64 KiB and a busy box overwrites the boot in minutes; metal serial is not readable |
| free RAM / heap | `ssh akuma "free"`; `grep -i heap` in the dmesg | the kernel heap is 1 GiB at ≥ 8 GiB usable (512 MiB below), and the 2026-09-18 LLD link crash was memory-pressure sensitive |
| what is running on it | `ssh akuma "ps"` | herd, sshd and **kot** share the machine; a build on top of a busy box is a different experiment |

## 3. The attempt

`kbuild` lives at `/bin/kbuild` on the box; it sources `/etc/akuma-dev.env` and
`cd`s to `$AKUMA_SRC`. **`kbuild -c` cleans only `target/x86_64-unknown-none`** —
it leaves the host units (proc macros, build scripts) under `target/release`, so
a whole-graph gate wipes the target dir first.

```sh
# on the box (through a session that survives a few minutes — see the harness notes below)
rm -rf /root/ktarget                    # whole-graph: 95 `Compiling` lines, not 79
kbuild -c -j 4 > /root/kbuild-j4-1.out  # stdout only; see the redirect note below
```

* **Redirect on the laptop side of ssh** if you can (`ssh akuma 'kbuild -c -j 4' > local.out 2>&1`):
  `2>&1` inside the *Firecracker guest's* shell answers `Bad file descriptor`; the
  metal's shell was not tested for it, so do not rely on either behaviour.
* **A silent exec channel has been seen to die** (~240 s under load, per the AArch64 driver's notes) and take the build with it.
  Either poll from a second session, or use the pattern of
  [`scripts/benchmarks/ryzen_fc/jrun.sh`](../../scripts/benchmarks/ryzen_fc/README.md)
  (build in the background on the *driver's* side, poll every 10 s, call it a
  **WEDGE** — not a slow pass — after 300 s with no new output, and snapshot
  `ps` at that moment). It is written for the ryzen guest but the loop is the
  reusable part.
* **Count runs, not luck.** The metal's earlier rate was 3 failures in 4 builds; a
  single green run says almost nothing (the same document's own lesson — one clean
  `-j1` build was luck). Five clean runs bound the rate below ~45 %; ten below ~26 %.
* Do not change rustflags between runs (`--threads=1` etc.) — they feed cargo's
  `-C metadata` hash and change the output bytes.

Record per run: verdict, rc, elapsed, the number of `Compiling` lines, the kernel
md5 and `uname`, and the console counters below. What the ryzen guest printed on a
**passing** build, so you know what is normal:

| console line | ryzen, per passing build | note |
|---|---|---|
| `[BKL] stuck: owner=N waiter=M tag=11 …` | exactly 27 (21 of 21 runs) | `tag=11` = x86_64 `munmap`; count unexplained. A **storm** is thousands, not 27 |
| `[MM] fault race #1` | 1 | the §14 demand-fault race; one per build is normal |
| `[TRAMP-MISMATCH] tid=…` | 6–11 | stale `thread_id` rows (`AKUMA_AMD64_NO_SLOT_RECYCLER.md`); not a defect |
| `[TLB] stuck`, `[Fault]`, `signal: 11` | 0 | any of these on the box is news |

Timing to expect: ~1m 30s on the ryzen guest at `-j4`. The metal built the
95-crate graph in **632–640 s at `-j1`** (SMP=4, 1 GiB heap) — so `-j4` there is a
new number, not a known one.

## 4. Reading a failure

Capture **before** anything else: `ssh akuma "dmesg" > metal-jN.dmesg`, then
`ssh akuma "ps"`, then `free`. A ring-3 fault leaves **no `[Fault]` line** here by
design: `idt.rs` hands it to `deliver_fault_signal` first, and Rust's std installs
a `SIGSEGV` handler for stack-overflow detection, so a dying `cargo`/`rustc` is
killed silently. "No `[Fault]`" is not "the kernel killed nothing".

| what you see | it is (documented) | do this |
|---|---|---|
| `rc=139` at 20–35 crates, `rustc` gone | the 2026-09-18 metal signature; cause **not found** on metal | get the crate name, `dmesg`, and any `#PF`/`#GP` line with `rip`/`cr2` |
| `#PF` with an **ASCII** `cr2` (`0x00004d5f4e4f4964` = `dION_M`) | a pointer overwritten by string data | record it verbatim; same signature as the first metal build |
| a ring-3 `#GP` with `rip` inside `ld-musl` (`rip - 0x3000_0000`, `INTERP_BASE`) | almost certainly musl mallocng's `assert` → `hlt` (privileged) — **three "mysterious" rips were all `f4`** | disassemble `lib/ld-musl-x86_64.so.1` at `rip − INTERP_BASE` **before** theorising about wild pointers ([`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md) §14). Static-PIE programs load at `PIE_BASE = 0x1000_0000` |
| `#SS` from ring 3 | was fatal to the whole kernel until 2026-09-25 (now `SIGBUS`); only KVM or metal can exercise it | `AMD64_THREE_CRASHES_2026-09-25.md` §2 |
| build reaches N/N then stops, `cargo` has CPU, every `rustc` at `0:00` | the untimed-`FUTEX_WAIT` leader (fixed 2026-09-18); a lost wake-up if it recurs | `sched::dump_slot_table`, `futex::dump_waiters`, `scripts/benchmarks/amd64_slot_report.py` (diff two 30 s blocks — one dump misleads) |
| `[BKL] stuck` **storm** then `[TLB] stuck: N peer(s) unacked`, dead to ssh | shootdown never acked: a core in IRQ-masked xHCI polling with the BKL dropped | `AMD64_THREE_CRASHES_2026-09-25.md` §1 — **the fix exists in the tree and was never verified on the metal**. This is the one to expect if the USB disk stalls |
| `[xhci] transfer timeout` before any of the above | the drive, not the scheduler | `AKUMA_AMD64_USB_XHCI.md`; USB 3.0 socket rule in the bare-metal runbook |
| `rust-lld … signal: 11` at the final link | memory pressure (`[ALLOC FAIL]`) or the same defect by a cheaper route | replay the printed argv with `--threads=1` (the 20 s A/B in bare-metal §3) |
| zero-page / `cr2` reads all zero (not `0xFEEDFACE`) under `pmm-forensics` | a freshly zeroed page put where data was — the §14 double-populate shape | `--features mm-forensics` / `pmm-forensics` build; **as of 2026-09-18 the metal had never run under `pmm-forensics`** |

Instruments already in the tree, all of which keep working:
`idt::dump_user_registers_and_memory` (ring-3 registers plus 48 bytes around every
user-looking pointer), `[TRAMP-BAIL]`, the `[MM]` race counter,
`scripts/benchmarks/amd64_mtstress_run.py` (a **regression gate and eliminator**,
not a reproducer — it lives in one process and cannot reach a multi-process
race) and `scripts/benchmarks/amd64_fc_build_matrix.py` for the HP box's
Firecracker guest.

## 5. Left open

* **The metal `-j4` question itself** — this runbook's whole point; no run on the
  box has happened with the current tree.
* **The constant 27** `[BKL] stuck` lines: the reporter folds by episode
  (`STUCK_REPORTED`, `akuma-bkl/src/sync.rs`), it is not a hard cap I found, and an
  event driven by timing landing on one value 21 times deserves a look.
* **ryzen at 2 GiB and at `-j8`**, and a fixed-point compare (the ryzen guest's
  compiler is a newer musl nightly than the gnu one that built its kernel, so
  the md5s would differ for a non-bug reason).
* **Older docs' "137-crate build"** (`docs/README.md`, this family of runbooks)
  means cargo's **137 units**, which include build-script runs; a whole-graph clean
  prints **95** `Compiling` lines. Left unedited — they record what was
  measured — but do not compare the two numbers.

## Verify

A result from the box is only comparable if all of these hold — check them, do
not assume:

1. `uname -a` and `md5sum /boot/akuma-amd64` are the kernel you meant to test
   (and match the running one; a self-build overwrites the file at that path).
2. `rm -rf /root/ktarget` preceded the run, and the output has **95** `Compiling`
   lines (79 means host units were reused).
3. The run's console log was captured *before* anything else touched the box.
4. The same source tree has a ryzen result, or the difference is stated.

For the ryzen side: `ls /home/netoneko/akuma-selfhost/runs/*/summary` on ryzen, each
starting `run=N jobs=4 … verdict=PASS`, and `scripts/benchmarks/ryzen_fc/README.md`
for re-running it.

## Background

* [`selfhost-kernel-build-amd64.md`](selfhost-kernel-build-amd64.md) — the HP-box Firecracker gate and the ryzen section.
* [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, its rules, "The SMP=4 `-j4` build wedge — the cheap repro".
* [`../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md) — the metal's `-j4`/LLD failures and the heap fix.
* [`../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md) — §§12–14, the wedge, the double-populate race, the `hlt` trick.
* [`../archive/AMD64_THREE_CRASHES_2026-09-25.md`](../archive/AMD64_THREE_CRASHES_2026-09-25.md) — the shootdown wedge and the `#SS` fix, both unverified on metal.
