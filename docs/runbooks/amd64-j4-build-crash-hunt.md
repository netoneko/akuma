# amd64 `-j4` self-host build: what is known, and how to attempt it on the trashcan

**Stability: B.** Both rigs now have measured `-j4` numbers on the same source
(2026-09-30, §1a). The metal half was a checklist until that day; §3 is still the
pre-flight for repeating it.

The question: does `kbuild -c -j 4` at SMP=4 finish on the machine, or does it die
the way [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)
§3 recorded (3 of 4 builds `rc=139`, at 22–35 crates)? Two rigs have data.

## 1a. Latest result (2026-09-30, `main` = `e6d8657b`): good, not parity

**Short version.** Self-hosting on amd64 is now at AArch64's *stability* under a
hypervisor and **not yet** on the metal. The 2026-09-18 metal signature — `rc=139`
SIGSEGV at 20–35 crates, 3 builds in 4 — did **not** recur in 10 builds (0
SIGSEGV, 0 `[Fault]`, 0 `[TLB] stuck`). But **3 of the 10 failed**, all the same
way: `execve` of `rust-lld` returned `EIO`, early in the build. The metal is also
slower at `-j4` than it was at `-j1`. Nothing here is parity with AArch64.

| rig | kernel | what ran | result |
|---|---|---|---|
| **ryzen**, Firecracker, 4 vCPU / 4 GiB | `e6d8657` built on ryzen, md5 `9791d5a02e2e` | 10 fresh-boot `kbuild -c -j 4`, whole `/root/ktarget` wiped each time (`CLEAN_ALL=1`) | **10 of 10 PASS**, 90 s each, 0 `[Fault]`, 0 SIGSEGV, 0 `[TLB] stuck`; 27 `[BKL] stuck` per run, unchanged. **31 of 31** with the 21 from 2026-09-29 |
| **HP box, bare metal**, SMP=4, 16 GiB, USB root | `0b800634` (one commit behind `main`: the HDA stream-release fix, `amd64/src/hda.rs` only; it contains `2d073ca0` and `0e331f5`, so §1's "known hang" gap is closed) | 10 x `rm -rf /root/ktarget; kbuild -c -j 4` via `scripts/benchmarks/amd64_metal_j4_loop.sh` | **7 PASS, 3 FAIL** (all three `EIO` at `execve`); 0 SIGSEGV |

Metal runs (96 `Compiling` lines on a pass — the graph is 96 since `akuma-hda`
joined it, not the 95 of the older numbers):

| run | verdict | wall | crates | `[BKL] stuck` | note |
|---|---|---|---|---|---|
| 1 | PASS | 874 s | 96 | 458 | |
| 2 | PASS | 836 s | 96 | 494 | |
| 3 | PASS | 843 s | 96 | 555 | |
| 4 | **FAIL** `rc=101` | 337 s | 33 | 271 | `could not exec rust-lld: I/O error (os error 5)`, `akuma-config` build script |
| 5 | PASS | 905 s | 96 | 652 | |
| 6 | PASS | 872 s | 96 | 581 | |
| 7 | **FAIL** `rc=101` | 291 s | 29 | 202 | same, `zerocopy-derive` (proc macro) |
| 8 | **FAIL** `rc=101` | 380 s | 33 | 281 | same, two links (`akuma-config`, `zerocopy-derive`) |
| 9 | PASS | 885 s | 96 | 594 | |
| 10 | PASS | 881 s | 96 | 574 | |

**Reading the metal failures.** All three are `lld-wrapper: could not exec
rust-lld: I/O error (os error 5)`, all linking a **host unit** (build script or
proc macro) in the first ~30 crates, at 291–380 s. No `rc=139`, no `[Fault]`. It is
`EIO` from `execve`: `amd64/src/usermode.rs`'s exec path reads the whole image with
`fs::read_image` under `bkl_free_io`, and a failed read of a cache-cold binary
comes back as `IoError` and is reported as `EIO` — the layer the call-site comment
ties to the USB-stall `[BKL] stuck` storm of 2026-09-11.

What is and is not established:

* **Established:** the signature is identical in 3 of 3 and always at the same
  phase — the first links of the build, when `rust-lld` (used once per link, and
  large) is cache-cold and up to four `rustc`s exec it at once. `-j1` builds never
  showed it (2026-09-19: 12m 16s, gen-2/gen-3 clean), and ryzen (virtio-blk) never
  does.
* **Hypotheses, none tested:** (a) concurrent cold reads of the same image through
  the BKL-free `read_image` path race, and the USB disk's latency widens the
  window; (b) a USB transfer error surfacing as `IoError`. (b) is **not excluded**
  — the batch that produced these ten runs sampled dmesg for
  `Fault|SIGSEGV|BKL|TLB|...` but **not** `xhci`/`usb`, so its `signals` files
  cannot say either way, and the one full `dmesg` read (during run 5) held no
  `xhci`/`usb` line after the ring had washed. The script in the tree now also
  matches `xhci|usb|transfer|EIO|I/O err` (the copy at `/root/j4loop.sh` on the box
  is the old one — re-copy it before the next batch).
* **Not established:** whether it is one bug; its rate (3 of 10, 95 % interval
  roughly 7–65 %); whether it is reachable on ryzen at all.

What this changes, and what it does not:

* **Changes:** `-j4` on the metal is no longer a compile-phase SIGSEGV coin-flip —
  0 of 10 against 2 of 2 on 2026-09-18 (95 % upper bound on a SIGSEGV rate at 10
  clean runs ≈ 26 %, so it is bounded, not removed).
* **Does not change:** a build that loses 3 of 10 to a storage-path `EIO` is not at
  AArch64's standard, where the self-host gate passes without a device caveat.
* **Speed:** passing runs took **836–905 s** (mean ≈ 871 s) at `-j4` against
  **632–640 s** at `-j1` (2026-09-19). More jobs made it *slower*. The guest does
  the same graph in 90 s. The likely cost is the contended BKL and the USB disk
  (hundreds of `[BKL] stuck` episodes per pass against 27 on ryzen); not profiled.
* **The two rigs' counts are not comparable:** `[BKL] stuck` was exactly 27 on ryzen
  and 202–652 on the metal; the USB root is the obvious difference.

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

> **Resolved 2026-09-30.** The box now runs `0b800634`, which contains `2d073ca0`
> and `0e331f5` (checked: `git merge-base --is-ancestor`, and the box's
> `amd64/src/sched.rs` has the `REMOVED 2026-09-26` text and no
> `bkl_free_by_choice`). The section below records the 2026-09-29 state and why
> `97fd2d10` was not a fair `-j4` subject; it is kept as history.

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
rm -rf /root/ktarget                    # whole-graph: 96 `Compiling` lines (95 before akuma-hda), not 79
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
95-crate graph in **632–640 s at `-j1`** (SMP=4, 1 GiB heap) and, measured
2026-09-30, the 96-crate graph in **836–874 s at `-j4`** (§1a) — slower, not
faster. A `-j4` metal run under ~10 min would be news.

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
| `#SS` from ring 3 | was fatal to the whole kernel until 2026-09-25 (now `SIGBUS`); only KVM or metal can exercise it | `AMD64_THREE_CRASHES.md` §2 |
| build reaches N/N then stops, `cargo` has CPU, every `rustc` at `0:00` | the untimed-`FUTEX_WAIT` leader (fixed 2026-09-18); a lost wake-up if it recurs | `sched::dump_slot_table`, `futex::dump_waiters`, `scripts/benchmarks/amd64_slot_report.py` (diff two 30 s blocks — one dump misleads) |
| `[BKL] stuck` **storm** then `[TLB] stuck: N peer(s) unacked`, dead to ssh | shootdown never acked: a core in IRQ-masked xHCI polling with the BKL dropped | `AMD64_THREE_CRASHES.md` §1 — **the fix exists in the tree and was never verified on the metal**. This is the one to expect if the USB disk stalls |
| `[xhci] transfer timeout` before any of the above | the drive, not the scheduler | `AKUMA_AMD64_USB_XHCI.md`; USB 3.0 socket rule in the bare-metal runbook |
| `lld-wrapper: could not exec rust-lld: I/O error (os error 5)`, `rc=101`, no SIGSEGV | `execve`'s `read_image` returned `IoError` → `EIO` on a cache-cold binary; 3 of 10 metal runs on 2026-09-30 (§1a), all linking a host unit in the first ~30 crates. **Cause not confirmed**; the code comment at the exec site ties this layer to USB transfer stalls | capture `dmesg` *immediately* (the ring washes in ~3.5 min — `amd64_metal_j4_loop.sh` samples it every 15 s for this), look for `[xhci] transfer timeout` (add `xhci`/`transfer` to the sampler's pattern first), check whether the same binary `execve`s again at once |
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

* **The metal `-j4` question, partly answered (2026-09-30, §1a).** The `rc=139`
  signature did not recur in 10 builds; 3 of 10 died of `EIO` at `execve` of
  `rust-lld`, always in the first ~30 crates. Open: concurrent cold-read race vs
  xHCI/USB error (look for `[xhci] transfer timeout` the moment it happens; try
  pre-warming `rust-lld` with `cat > /dev/null` before the build — if that removes
  the failures it is the cold read, and it is also a workaround), why `-j4` is
  slower than `-j1` there, and a larger sample.
* **Metal under a whole-graph wipe with the serial/`dmesg` captured per run** —
  done by `scripts/benchmarks/amd64_metal_j4_loop.sh`; it exists because the
  ring washes before anyone reads it.
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
2. `rm -rf /root/ktarget` preceded the run, and the output has **96** `Compiling`
   lines since `akuma-hda` joined the graph (95 before; 79/80 means host units
   were reused).
3. The run's console log was captured *before* anything else touched the box.
4. The same source tree has a ryzen result, or the difference is stated.

For the ryzen side: `ls /home/netoneko/akuma-selfhost/runs/*/summary` on ryzen, each
starting `run=N jobs=4 … verdict=PASS`, and `scripts/benchmarks/ryzen_fc/README.md`
for re-running it.

## Background

* [`selfhost-kernel-build-amd64.md`](selfhost-kernel-build-amd64.md) — the HP-box Firecracker gate and the ryzen section.
* `scripts/benchmarks/amd64_metal_j4_loop.sh` — the on-box batch driver (copy to `/root/j4loop.sh`, run with `sh`, detached); `scripts/benchmarks/ryzen_fc/` — the ryzen one.
* [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, its rules, "The SMP=4 `-j4` build wedge — the cheap repro".
* [`../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md) — the metal's `-j4`/LLD failures and the heap fix.
* [`../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md) — §§12–14, the wedge, the double-populate race, the `hlt` trick.
* [`../archive/AMD64_THREE_CRASHES.md`](../archive/AMD64_THREE_CRASHES.md) — the shootdown wedge and the `#SS` fix, both unverified on metal.
