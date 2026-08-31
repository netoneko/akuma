# `src/console.rs`'s MMIO → `crates/akuma-uart` (2026-08-31)

The PL011's three raw register accesses became one `unsafe` in a crate, and
**`src/console.rs` now carries `#![forbid(unsafe_code)]`** — the second enforced
subtree in `src/` after `src/syscall/`.

Two steps, and the second is the interesting one:

1. Three `unsafe` blocks (one per register access) → one, via
   [`akuma_primitives::mmio::MmioReg`]. `TRIM_FAT_MMIO_NEWTYPE.md` had named
   `src/console.rs` as a conversion candidate; this is it.
2. That one moved into `crates/akuma-uart`, so the console file holds none.

## 1. Numbers

Regenerated with `python3 scripts/cloc_akuma.py src crates`.

| | before | after |
|---|---:|---:|
| `src/console.rs` `unsafe` sites | 3 | **0** (`forbid`) |
| `src/` production `unsafe` | 116 | **113** |
| `crates/` production | 320 | **321** |
| tree production | 436 | **434** |
| crates | 33 | **34** |

`akuma-uart` is **53 production lines, 1 unsafe site, 88.7% safe**. The tree lost
2 production sites net: three call-site vouches collapsed into one, which is the
whole `MmioReg` argument — the fact needing a human's word is *"this window is
the UART"*, true once per device, not once per read.

## 2. Codegen is identical, and that was checked rather than assumed

`MmioReg`'s doc states the requirement plainly: *"Converting a driver to
`MmioReg` must not change a single instruction it emits."* Verified by
disassembling `akuma::console::print` in the ELF before the first step and after
the second, normalising addresses and the crate-metadata hash in the symbol name:

```
instr counts: old=65 new=65
--- normalized diff: IDENTICAL INSTRUCTION SEQUENCE
```

Both builds materialise `0x80_0001_1000` with the same `mov`/`movk`/`movk` triple
and store with the same `strb`. Worth recording how easy it was to get a *false*
pass here: the first attempt grepped for a legacy-mangled symbol name, found
nothing in either ELF, and `diff` cheerfully reported the two empty files as
identical. Check that an extraction captured something before believing its diff.

## 3. Where the `unsafe` should live — three candidates, one right answer

The question asked was whether the remaining `unsafe` should move to
`akuma-cpu`, `akuma-mmu`, or a crate of its own.

**`akuma-cpu` — no.** Its charter is every AArch64 *instruction* that is safe to
execute: barriers, cache/TLB maintenance, core parking, `DAIF`, the virtual-timer
comparator, read-only system registers. MMIO is not an instruction category; it
is a memory access to a mapped window. A UART register there dissolves the one
sentence that makes the crate reviewable.

**`akuma-mmu` — no, and not for the obvious reason.** It builds the device window
and could plausibly vouch for windows it maps. But **it does not map this one**:
the boot assembly installs the UART's L3 entry before any Rust runs, which is
exactly why `src/boot.rs`'s `UART_L3_SLOT` exists and why its doc says the boot
assembly and `akuma_exec::mmu` "must agree on it". The console prints from the
first Rust instruction; a mapping `akuma-mmu` had to establish first would be too
late for its earliest callers.

There is a second, more general reason to refuse, worth stating because a
`device_reg(va)` helper looks so appealing: the obligation has **two halves** —
"the window is mapped" and "this machine has the device behind it". A mapping
crate can vouch for the first and knows nothing of the second. Handing out a
working-looking register for an absent device is precisely how `ramfb::init` took
`EC=0x25` with `FAR=0x8000012008` on the first Firecracker boot
(`AKUMA_FIRECRACKER_KVM.md`). A helper that vouches for the half it knows and
silently assumes the other is *worse* than the call site it replaces — the
grant-records-vs-deny-records trap in a new dress.

**A crate of its own — yes.** `akuma-uart` owns the PL011 and states both halves
itself. Same shape as `akuma-alloc` (`AKUMA_ALLOC_EXTRACTION.md`): isolate the
trusted-but-difficult `unsafe` behind a named boundary so the callers can be
enforced-safe.

## 4. The seam: device, not console

The crate is called `akuma-uart`, not `akuma-console`, and holds only ~53 lines:
two registers and three byte-level operations. Everything else stayed in
`src/console.rs`. Two concrete reasons, neither of them taste:

1. **The cross-core lock is gated on `cfg(kernel_console_lock)`**, and a crate
   only sees a cfg its **own** `build.rs` emits. Moving it would mean a build
   script plus a forwarded feature — or a silently dead gate, which is how
   `akuma-exec` once shipped a family of dormant `kernel_profile_extreme` gates
   (`akuma_exec_missing_buildrs_cfg`). `akuma-uart` has **no cfgs and no
   `build.rs`**, and adding one later means adding a build script first.
2. **It needs `akuma_exec::bkl::current_core_id`**, which would drag the
   23k-line execution crate underneath the console.

So the split is *device* below, *policy* above: IRQ masking, the opt-in
`Spinlock` with its per-core reentrancy guard, the `MULTICORE` runtime gate, and
all formatting stay in the bin. One dependency (`akuma-primitives`), and it is in
`default-members` — a plain `no_std` library with no binary-level items builds
for the host fine (host suites 66 → 68).

## 5. Why `const UART`, not `static UART`

`MmioReg` is deliberately `!Sync`, so parking one in a `static` obliges the
driver to write `unsafe impl Sync` and say what serialises access. A `const` is
materialised at each use rather than having one address, so the question never
arises — and the honest answer would have been awkward, because **nothing**
serialises this by default.

That is intentional, not an omission. The console has to work from a panic
handler, from an IRQ, and from a core holding no locks, so cross-core
serialisation is opt-in (`CONSOLE_LOCK=1`) and concurrent writers merely
interleave bytes at the shared data register. Interleaved bytes are a legibility
problem, not memory unsafety — the register is device memory, not Rust-visible
storage. Using a `const` records that reasoning in the type system instead of
asserting `Sync` and hoping a reader finds the comment.

Inherited behaviour worth naming while it is being documented: `write_byte` does
**not** wait for space in the transmit FIFO. That is deliberate — the console's
value is that it still emits while the kernel is failing, and a `TXFF` spin is a
place to hang while trying to report why. A full FIFO drops the byte.

## 6. Verification

Clippy clean on the workspace, on `-p akuma-uart --target aarch64-unknown-none`,
and on `platform-firecracker`. Built: `extreme-size` (707K, unchanged),
`devbox-smoltcp` (2.6M), `devbox` rump (2.3M). Host: 68 suites. `cloc_akuma.py
--self-test` PASS.

`forbid` proven to bite: adding a throwaway `unsafe` block to `src/console.rs`
produces `usage of an 'unsafe' block`, then reverted.

Boots, SMP=4, three runs: **313/1, 314/0, 313/1**. The single failure is
`test_epoll_multi_poller_pipe: woken=1 (expected 2)` — a **documented test-harness
defect**, not a kernel bug, listed in `docs/README.md`'s symptom matrix as failing
"~1 boot in 3" with the standing warning *"do not accept/reject a change on one
boot"* (`EPOLL_MULTI_POLLER_PIPE_FLAKE.md`: a 2 ms "assume both threads are
scheduled" delay with no handshake, and a wake budget exactly equal to the 10 ms
poll-interval fallback). Observed 2-in-3 here, above the recorded rate because the
host was running repeated VMs — the `[BKL] stuck ... tag=511` lines alongside it
are the same load-driven pre-existing noise (`bkl_tag511_storm_is_load_driven`).
It was the **only** failing test in every run, and never a new one.

Also live-checked over SSH, which is the part a boot suite does not cover: the
console serves a real login session at SMP=4.

## Background

- `TRIM_FAT_MMIO_NEWTYPE.md` — `MmioReg`, which named this file as a candidate.
- `AKUMA_ALLOC_EXTRACTION.md` — the same quarantine pattern, one day earlier, and
  the anti-pattern (`OnceCopy` hooks) it discarded on the way.
- `UART_SMP_INTERLEAVE_FIX.md` — why the cross-core lock exists at all, i.e. what
  §5's "nothing serialises this" costs.
- `AKUMA_FIRECRACKER_KVM.md` — the `FAR=0x8000012008` abort that §3 cites as the
  reason a general `device_reg(va)` helper would be a regression.
- `FRAMEBUFFER_REMOVED.md` — the fw_cfg driver whose `const REGS` this file's
  `const UART` copies; removed the same day.
