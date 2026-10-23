# Line-count statistics, post-amd64 edition

**Measurement: 2026-10-23**, working tree (branch `kvm-part-1`, HEAD `e0f77ceb`).
`scripts/cloc_akuma.py src crates amd64/src` — the script's default paths now
include `amd64/src`, which the 2026-08-23 measurement
([`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md)) did not count. That doc
stays the living document for the `src` + `crates` series; this one is a dated
snapshot for the whole tree after the amd64 bare-metal self-hosting milestone,
so the two series are given separately below and never mixed.

Every Akuma number here is measured on this date by the commands in
"Reproducing". Every number about another project is carried over from the prior
doc's public-source figures and should be re-checked before external citation.

---

## The headline

| | 2026-08-23 (`v0.0.7-akuma-on-aws`) | 2026-10-23 | delta |
|---|---:|---:|---:|
| production code (`src` + `crates`) | 43,700 | **87,778** | **+101%** |
| test code (`src` + `crates`) | 29,302 | **61,523** | +110% |
| production code (`amd64/src`) | — (not counted) | **27,126** | new |
| production code, tree total | 43,700 | **114,904** | +163% |
| test-to-production ratio | 0.67× | 0.70× (`src`+`crates`) | flat |
| production `unsafe` sites (`src` + `crates`) | 535 | **261** | **−51%** |
| production `unsafe` sites (`amd64/src`) | — | 58 | new |
| production `unsafe` / kLoC, tree total | 10.5 | **2.8** | −73% |
| crates | 22 | **98** | +76 |
| crates with `#![forbid(unsafe_code)]` | 13 of 22 | **72 of 98** | — |
| documented fixes (bugfix ledger) | ~699 | **1,451** | +752 |
| investigation docs (`docs/archive/*.md`) | ~355 | **594** | +239 |
| commits | 2,039 | **3,183** | +1,144 |

Two months, and the tree doubled. Both halves of that sentence deserve the same
attention: the doubling is real new capability (an entire second kernel port,
now on bare metal, plus a hypervisor), and the doubling is the cost of saying
yes to amd64, KVM, WiFi, USB, NVMe and audio. Neither half is an accident.

---

## Stat 1: `src/` production code is 136 lines

The extraction programme finished. The AArch64 kernel's production logic no
longer lives in the bin crate at all: `src/` holds a **136-line production
footprint** (boot entry and the shim that hands off to crates) plus a 22,212-line
in-kernel boot test suite that runs on bare metal instead of under `cargo test`.
Everything else — syscall glue, memory management, process management, the
network stack — is in `crates/` as host-testable units.

This is the inversion-of-control endpoint the pre-port refactor aimed at
(`AKUMA_AMD64_STREAMLINING.md`): the kernel is a composition of libraries with
empty or near-empty dependency tables, and `src/` is the smallest possible
thing that wires them to hardware. The same shape made the amd64 port possible
at all, because `x86_64-unknown-none` cannot compile the glue and never had to.

## Stat 2: production `unsafe` halved while code doubled

535 → 261 sites in `src`+`crates` (−51%) on +101% more production code; with
`amd64/src` included, 319 sites over 114,904 lines, **2.8 per kLoC where the
prior measurement read 10.5**.

The decomposition matters more than the headline:

- **Waves, not drift.** Each drop corresponds to a documented sweep, not
  attrition: the inline-asm migration (218 → 35 `asm!` sites,
  `INLINE_ASM_CLEANUP.md`), the syscall subtree ban (`SYSCALL_UNSAFE_CLEANUP.md`),
  `akuma-exec` reaching forbid at 9,311 lines (`AKUMA_EXEC_FORBID_UNSAFE.md`),
  the trap/MSR round that took `amd64/src/idt.rs` from 38 sites to 4
  (`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`), and the MMU safe facade
  (`AKUMA_MMU_SAFE_FACADE.md`).
- **The ratchet holds.** `docs/reference/unsafe-budget.json` +
  `scripts/check_unsafe_budget.py` gate every component; 72 of 98 crates now
  carry `#![forbid(unsafe_code)]`. On 2026-08-29 that was 13 of 22. The
  enforcement grew from a sweep into the default for new code: a new crate's
  budget is 0, so `forbid` is the path of least resistance.
- **What survived is deliberate.** The remaining sites concentrate in the
  irreducible leaves (`akuma-mmu` page-table walks, `akuma-dma` cell access,
  `akuma-gic` MMIO) with stated contracts, plus `akuma-syscalls-linux`'s
  `transmute` layout assertions — `unsafe` spent to *pin* ABI safety at compile
  time.

## Stat 3: 76 new crates in eight weeks — and what they are

22 → 98 crates sounds like bloat until the list is read: `akuma-trap-amd64`,
`akuma-idt-amd64`, `akuma-pte-amd64`, `akuma-svm-amd64`, `akuma-kvm-abi`,
`akuma-vm-spec` (the hypervisor and its ABI), `akuma-xhci`, `akuma-usb`,
`akuma-nvme`, `akuma-hda` (bare-metal drivers, the first three pure and
host-tested), `akuma-wifi`, `akuma-wpa` (the rtw89/WPA2 stack, the tree's
messiest corner), `akuma-multiboot2`, `akuma-cmos-rtc`, `akuma-ryzen-amd64`,
`akuma-exec-core`, `akuma-kernel-core`, `akuma-not-even-once`,
`akuma-slot-table`, `akuma-pagetable`, `akuma-pipe`, `akuma-elf`,
`akuma-fpcache-rw`, `akuma-fbcon`/`akuma-fbdev`, `akuma-dwi2c`, `akuma-i2c-hid`,
`akuma-hid`, `akuma-config`, `akuma-uart`, `akuma-el0-entry`, `akuma-trap-aarch64`,
`akuma-sntp`, `akuma-syscalls-log`, `akuma-fdt`.

The pattern is the same one the first 22 established: small, named, host-tested,
most forbidden. Extraction continues to cost single-digit percent of lines per
crate and buys the test surface back several times over — the
`akuma-syscalls-mem` extraction caught a userspace-reachable integer overflow on
its **first** host-test run, before any VM booted
(`AKUMA_EXTRACT_MMAP.md` §10.1).

## Stat 4: the bug ledger at 1,451

The itemized ledger's own maintained count
([`BUG_FIX_LIST.md`](BUG_FIX_LIST.md), counting rule in its header: one item
per distinct dated/named confirmed fix):

- **1,451 distinct fixes**, up from ~699 on 2026-08-25 — +752 in eight weeks.
- 401 investigation docs contribute at least one fix.
- Subsystem split, per the ledger's own table:

| Subsystem | Fixes | % |
|---|---:|---:|
| Syscall / ABI compatibility audits | 242 | 16.7% |
| Memory & virtual memory | 176 | 12.1% |
| Scheduler & process management | 139 | 9.6% |
| SMP & locking | 133 | 9.2% |
| VFS & filesystem | 134 | 9.2% |
| Boot & drivers | 129 | 8.9% |
| Networking | 94 | 6.5% |
| Toolchain & self-hosting | 91 | 6.3% |
| Console & terminal | 58 | 4.0% |
| Misc / cross-cutting | 55 | 3.8% |
| Containers | 55 | 3.8% |
| Signals & exceptions | 40 | 2.8% |
| Userspace apps & libraries | 40 | 2.8% |
| SSH | 39 | 2.7% |
| Rump kernel & syscall proxy | 26 | 1.8% |

Two readings, both honest:

- **The rate is the point, not the total.** ~94 fixes/week sustained across two
  months is what AI-assisted development with mechanical checking looks like in
  volume terms (see `600_BUGS_ANNIVERSARY.md`). The number is not a quality
  verdict in either direction — it is the *size of the checking problem* that
  the compiler gates, host tests, and A/B harnesses had to absorb.
- **"Toolchain & self-hosting" is 6.3% — and it was the best money spent.**
  The 91 self-host-attributable fixes are the ones that had no cheaper trigger:
  each needed a live parallel build inside the OS to even reproduce. The
  remaining ~94% would have been found by something eventually; these would not.

## Stat 5: comment density went *up* — 67.5% on production

Production comments / production code, tree total: 77,609 / 114,904 =
**67.5%** (2026-08-29: 47.7%). The rise is not style drift. `amd64/src` alone
carries 23,188 comments on 27,126 production lines (85%) — a second kernel
written *after* the lessons of the first, with the contracts stated at the seam
rather than re-learned from incidents. `LINE_COUNT_ANALYSIS.md` Reading B said
high comment density is a complexity tell; the post-amd64 reading is that here
it is mostly a *one-time porting tax* — writing down what the types cannot
carry, in the tree that needed it most. Whether it stays that way is a question
for the next measurement.

---

## The peer-group rows, re-measured per platform

The prior doc's comparison table is reproduced there with public-source figures
for the other projects; only the Akuma rows change. Split per platform, since
"the kernel" is now two kernels sharing the crate tree:

**AArch64 kernel** (`src` + `crates` — the crates are shared with amd64, so
counting them per platform double-counts; the platform totals below include
them for both, and are not additive):

| | 2026-08-23 | 2026-10-23 |
|---|---|---|
| production code, own tree (`src/`) | 8,579 | 136 |
| production code, shared crates | 35,121 | 87,642 |
| **production code, platform total** | **43,700** | **87,778** |
| production `unsafe` sites | 535 | 261 |
| self-build | reliable in-VM (`-j4`, 20/20 first-try, `AKUMA_SELF_HOSTING.md`) | unchanged |
| hosts | QEMU, AWS Graviton2 (`m6g.metal`, Firecracker/KVM) | unchanged |

**amd64 kernel** (`amd64/src` + the shared crates):

| | 2026-08-23 | 2026-10-23 |
|---|---|---|
| production code, own tree (`amd64/src/`) | — (not counted) | 27,126 |
| production code, shared crates | 35,121 | 87,778 |
| **production code, platform total** | — | **114,904** |
| production `unsafe` sites | — | 58 |
| self-build | — | **bare metal, byte-identical fixed point** (`AKUMA_AMD64_BARE_METAL_SELFHOST.md`) |
| hypervisor | — | **in-tree SVM/KVM vocabulary (`akuma-svm-amd64`), runs Linux guests and Akuma itself as a guest (`kvmakuma`)** |
| hosts | — | HP 500-502nj and Ryzen laptops, bare metal |

What the re-measurement adds is that the self-host claim is now made on **real
hardware with a real bootloader**, where "install the kernel" previously meant
Ubuntu-owned GRUB — the loop this repo calls self-hosting now excludes every
other operating system (`AKUMA_AMD64_BARE_METAL_SELFHOST.md` §1).

The "two-project club" framing from the prior doc stands, with the caveat it
carried: the route is easier than Redox's in one concrete way (unmodified musl
binaries instead of porting rustc onto a new libc), and breadth remains
one maintainer behind.

---

## Caveats

- **Comparability.** The cloc script gained features between measurements
  (unsafe counting matured, path defaults now include `amd64/src`). The
  `src`+`crates` series above is computed with the same path arguments for both
  dates and should be comparable; the tree-total row has no prior. When in
  doubt, re-run with `--rev`.
- **Working tree, not a tag.** This snapshot includes uncommitted changes on
  `kvm-part-1`. Reproduce against a tag for anything citable.
- **Fix count method.** 1,451 is the ledger's maintained count with its own
  counting rule (see `BUG_FIX_LIST.md` header), not a grep of bullet points —
  a naive `grep -c '^- '` overcounts ~2% by including table-of-contents rows.
- **One sample per date.** Every "−73% unsafe density" style claim is two
  points, not a trend line. The next snapshot should be taken on a release tag.

---

## Reproducing

```bash
python3 scripts/cloc_akuma.py --json                      # tree total (default paths)
python3 scripts/cloc_akuma.py --json src crates           # continuity series
python3 scripts/cloc_akuma.py --json amd64/src            # the amd64 kernel
python3 scripts/cloc_akuma.py --vs v0.0.7-akuma-on-aws    # delta vs the prior snapshot
git rev-list --count HEAD
ls docs/archive/*.md | wc -l
python3 -c "import json; print(len(json.load(open('docs/reference/unsafe-budget.json'))['forbids']))"
```

The bug-ledger statistics are maintained by hand in `BUG_FIX_LIST.md` — update
that file (procedure: `docs/runbooks/update-bug-fix-list.md`), never this one.

---

## Background

- [`LINE_COUNT_ANALYSIS.md`](LINE_COUNT_ANALYSIS.md) — the pre-amd64 living
  document; this snapshot's methodology, peer table and per-stat reasoning.
- [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md) —
  the milestone this measurement follows.
- [`600_BUGS_ANNIVERSARY.md`](600_BUGS_ANNIVERSARY.md) — what the fix volume
  means for AI-assisted development.
- [`docs/reference/crate-safety.md`](../reference/crate-safety.md) — the
  per-crate forbid/unsafe accounting referenced by Stat 2.
