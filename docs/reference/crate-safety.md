# Crate safety: which crates forbid `unsafe`

**Grade: A** — regenerated 2026-09-01 with
`python3 scripts/cloc_akuma.py src crates` after the `src/` boot-entry cleanup
([`SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md`](../archive/SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md)).
`src/` production fell **11 -> 3** and `crates/` rose only **411 -> 412**, so the
tree total fell **422 -> 415**: **seven production sites are genuinely gone**,
not relocated, and no crate was added. **23 of 39 crates** forbid, unchanged.

This is the run where the campaign's usual move — extract the obligation into a
crate that owns it — was tried, measured, and **reverted in favour of deleting
the operation**. An `akuma-qemu` crate briefly existed to hold the semihosting
exit; the measurement that justified it also showed the mechanism does not work
on the accelerator everybody runs, and PSCI already did the job from a crate the
tree had. See ["The one that did not need a crate"](#no-crate) below.

Six of the seven went away because an obligation was *discharged* rather than
moved, and the pattern is worth copying:

- **`mmu::rebuild_boot_device_table` was `unsafe` only to make the caller promise
  two derivations still agreed.** It took the device L3's physical address; the
  caller got it from `src/boot.rs`, which computed it from the `boot_page_tables`
  linker symbol ("the device L3 is page 5"). Two descriptions of one table. It
  now walks the boot TTBR0 for `L0[1] -> L1[0] -> L2[0]` — which *is* the callee's
  stated precondition — so there is no wrong table a caller can pass, the
  parameter is gone, and so is `boot::boot_device_l3_phys()` and the `unsafe
  extern` block inside it. Two `unsafe` blocks in `main.rs`, one in `boot.rs`.
- **`mmu::ensure_boot_identity_covers` / `rebuild_boot_device_table` are
  boot-phase-only, and the boot phase is observable.** Their obligation ("boot
  page table, single-threaded, before any other address space exists") is exactly
  the window that closes at `mmu::init`, so both now check `is_initialized()`
  themselves instead of asserting it in a `# Safety` block the caller re-copies
  at each site. This is the third time in the campaign a `# Safety` clause turned
  out to be a runtime-checkable predicate; see
  [`SYSCALL_UNSAFE_CLEANUP.md`](../archive/SYSCALL_UNSAFE_CLEANUP.md).
- **`boot_x0_at_entry` stopped being an extern static.** The boot assembly's
  `str x0, [x1]` is a plain aligned 64-bit store, so the storage can be a Rust
  `AtomicU64` that a relaxed load may read with no `unsafe` at all. It carries
  `#[unsafe(link_section = ".data.boot")]` deliberately: the store happens at
  `_boot + 4`, *before* the `.bss` clear at the top of `_boot_code`, so a `.bss`
  home would be zeroed back out — verify with `nm` that the symbol reads `D`.
- **`Waker::from_raw` became `Waker::noop()`.** The async main loop never parks
  on a waker, so the hand-rolled `RawWakerVTable` of four empty closures was
  reproducing a safe `const` stdlib item.

`src/` production `unsafe` is now **3 sites in 2 files**: `src/main.rs` (2 — the
`unsafe extern "C"` linker-symbol block and `akuma_fdt::locate`) and
`src/smp_shared.rs` (1, the same kind of declaration block). **Exactly one of the
three is a real operation**: `akuma_fdt::locate`, which dereferences a
firmware-supplied pointer and is irreducible for the reason `akuma-fdt` exists.
The `#![forbid(unsafe_code)]`-across-`src/` goal is now blocked only on the
`global_asm!` and `#[unsafe(no_mangle)]` attributes the boot and secondary
trampolines need — a lint question, not a soundness one.

<a id="no-crate"></a>
### The one that did not need a crate: the semihosting exit

`src/main.rs`'s last `asm!` was `hlt #0xf000` with `SYS_EXIT_EXTENDED` — ARM
semihosting, which ends the QEMU process **with an exit code**. That code is the
only thing it can do that PSCI `SYSTEM_OFF` cannot, so it looked like a crate:
27 lines, one instruction, on the `akuma-uart` model.

Measuring it before writing the doc is what killed it. The matrix, 2026-09-01,
with a temporary `halt()` probe at the top of `rust_start`:

| mechanism | `HVF=1` (the default on Apple silicon) | `HVF=0` (TCG) | carries an exit code |
|---|---|---|---|
| semihosting `hlt #0xf000` | **wedges the vCPU** | exits 42 | yes |
| PSCI `SYSTEM_OFF` | exits 0 | exits 0 | no |

Three things fall out of that table, in order of how badly each was assumed
wrong:

1. **Under HVF the `hlt` does not fall through — it wedges.** The instruction
   never retires, so the vCPU sits on it forever. The `wfi` loop written
   underneath it as a "fallback if semihosting is unavailable" was never reached
   on the default accelerator, and neither was anything else placed after it. A
   panic on a stock `cargo run` hung the VM instead of stopping it.
2. **Therefore ordering cannot rescue it.** The natural design — semihosting
   first for the exit code, PSCI second for everything else — was built and
   measured, and it hangs under HVF for exactly the reason above: step 2 is
   unreachable. Reversing the order makes PSCI unconditional and semihosting
   dead, because PSCI never fails where QEMU runs.
3. **Nothing reads the exit code.** Every harness under `scripts/` that judges a
   run detects a panic by grepping the log for `[PANIC]`; the `returncode == 0`
   checks in `forktest_smp_matrix.py` and `quick_forktest.py` are on *guest*
   binaries run over ssh, not on QEMU. `sched_audit_matrix.py` prints QEMU's rc
   and does not branch on it.

So the crate held a mechanism that works only on a non-default accelerator, buys
a signal nothing consumes, and — by being first in the chain — actively
prevented the mechanism that does work. `src/main.rs` now calls
`akuma_psci::call(SYSTEM_OFF, …)` and parks, `halt_with_code` is gone (it could
not honour a code any more, and a discarded parameter is a lie), and the crate
was deleted.

**The transferable lesson is about the extraction reflex, not about
semihosting.** "Which crate should own this `unsafe`?" is the second question.
The first is "does this operation still do anything?", and it is cheap to
answer: one probe, two accelerators, ten minutes. Had the crate landed without
it, the tree would carry a 39th crate whose sole function is to hang the default
build.

If exit-code fidelity is ever genuinely needed — a CI that wants a panic to be
`$?` rather than a log grep — the answer is semihosting **under `HVF=0` only**,
selected by the harness, not by the kernel. The kernel cannot tell the two
accelerators apart without reading `MIDR_EL1` and guessing.

The run before it, the same day, was
`python3 scripts/cloc_akuma.py src crates` after the exception path left `src/`
as `akuma-exceptions`
([`AKUMA_EXCEPTIONS_EXTRACTION.md`](../archive/AKUMA_EXCEPTIONS_EXTRACTION.md)).
This is the one the previous four runs were clearing the way for. `src/`
production fell **91 -> 11** while `crates/` rose **331 -> 411**, and the tree
total held at **422**: 80 sites moved, none added, none deleted. **23 of 39
crates** forbid — `akuma-exceptions` is the sixteenth that cannot, and unlike
most of them there is no version of it that could. A vector table and
register-restore trampolines are what the crate *is*.

`src/` production `unsafe` is now **11 sites in 3 files**: `src/main.rs` (9 —
the boot entry, DTB location, boot device-table rebuild), `src/boot.rs` (1) and
`src/smp_shared.rs` (1), the last two being `unsafe extern "C"` linker-symbol
declarations. The `#![forbid(unsafe_code)]`-across-`src/` goal has stopped being
a question of volume and become a question of the boot entry point — 80 of the
91 were one file.

The extraction also surfaced the `target_os`-not-`target_arch` trap for the
second time (`akuma-cpu`'s "Host builds" note is the first). Every crate under
`crates/` is clippied at `-D warnings` against the *host* by the pre-commit
hook, so a bare-metal crate that lives there must build for `aarch64-apple-darwin`.
Three EL1 control-flow register writes — `msr vbar_el1`, `msr tpidr_el1` and two
`msr elr_el1` — **assemble** on that host and would have gone green with live
EL1 instructions in them; only the vector table's Mach-O-invalid
`.section .text.exceptions` failed loudly enough to surface the rest. All are
now `target_os = "none"`-gated, at zero added `unsafe` sites.

The run before it, the same day, was
`python3 scripts/cloc_akuma.py src crates` after the PSCI conduit left `src/` as
`akuma-psci` ([`AKUMA_SMP_SHARED_SPLIT.md`](../archive/AKUMA_SMP_SHARED_SPLIT.md)
step 3). `src/` production fell **92 -> 91** while `crates/` rose **329 -> 331**:
splitting `psci_call(use_hvc, …)` into `smc_call`/`hvc_call` turns one `unsafe`
block into two, which is expected — each is now a single fixed instruction with
no branch inside the asm, and the win is that `src/` is one closer to zero, not
arithmetic. **23 of 38 crates** forbid. `akuma-boot` **kept** its ban and gained
`system_reset`/`system_off`: the conduit is a sibling crate precisely so the
`smc`/`hvc` did not cost it (the `akuma-net` / `akuma-net-nic` split, same
reason).

**`src/smp_shared.rs` now holds zero `unsafe` operations** — 8 in August, via
`akuma-fdt` (→4), the GIC consolidation (→1) and this (→0). It still **cannot**
carry `#![forbid(unsafe_code)]`: the lint also rejects `unsafe extern` blocks,
`global_asm!` and `#[unsafe(no_mangle)]`, and it needs all three for the
secondary trampoline. That distinction matters for the endgame — `src/exceptions.rs`
has two `global_asm!` blocks and five `#[unsafe(no_mangle)]` handlers, so cleaning
its blocks in place would still not let `src/` forbid. It has to move to a crate.

The run before it, the same day, was
`python3 scripts/cloc_akuma.py src crates` after the GIC consolidation
([`AKUMA_GIC_CONSOLIDATION.md`](../archive/AKUMA_GIC_CONSOLIDATION.md)): the
GICv3 driver, previously run from `src/gic.rs`, `src/gic_v3.rs` and the
redistributor half of `src/smp_shared.rs`, became `akuma-gic`. `src/` production
fell **104 -> 92** while `crates/` rose only **324 -> 329**: twelve blocks left,
five arrived, and the missing seven are genuinely gone — four were a GICv2
backend no profile enabled and HVF could not run, three were a byte-identical
second copy of `mmio_w32`/`mmio_r32` and the `GICR_WAKER_*` bits that the crate
already had. **23 of 37 crates** forbid; `akuma-gic` is one that cannot, by
construction, and its five blocks sit behind a single stated MMIO contract in the
shape `akuma-net-nic` uses for DMA. `src/smp_shared.rs` went **4 `unsafe` blocks
to 1** — the PSCI SMC/HVC conduit call, and nothing else. The largest remaining
holder is unchanged and is now essentially the whole problem:
`src/exceptions.rs` at **77 of the 92**.

The run before it, the same day, was
`python3 scripts/cloc_akuma.py src crates` after the file-page cache left `src/`
as `akuma-fpcache`
([`AKUMA_FPCACHE_EXTRACTION.md`](../archive/AKUMA_FPCACHE_EXTRACTION.md)):
500 lines holding **zero** `unsafe`, so neither production
count moved — `src/` stayed at **104** and `crates/` at **324**. **23 of 36
crates** forbid: numerator and denominator both rose by one, which is the shape
an extraction has when the thing extracted was already safe. What it bought is
one fewer file in `src/` for the `#![forbid(unsafe_code)]` goal and one fewer
`crate::` edge out of `src/exceptions.rs` — the file holding **77 of the 97**
`unsafe` blocks left in `src/` production code, and therefore the whole of that
goal. The one behavioural change is that `SHARED_FILE_PAGES_ENABLED` stopped
being a const-folded gate and became a relaxed atomic load, because the tunables
are `src/config.rs` `const`s and the crate sits below the module that owns them;
A/B'd on `extreme-size` at **+304 bytes `.text`, +16 bytes `.bss`, and 0 bytes of
image** (724,328 B on both arms).

Earlier the same day, after
[`AKUMA_FDT_EXTRACTION.md`](../archive/AKUMA_FDT_EXTRACTION.md): the six
`unsafe` operations that turned a boot pointer into a device tree — spread over
`src/main.rs`, `src/platform.rs` and `src/smp_shared.rs` — became one, in the new
`akuma-fdt`. `src/` production fell **113 -> 104** while `crates/` rose **321 ->
324**: nine sites removed, three relocated, which is the usual shape (see "The
allocator is a quarantine" below) except that here the count genuinely drops,
because five of the nine were duplicates of each other. **22 of 35 crates**
forbid — `akuma-fdt` is the crate that cannot, by construction and on purpose.
`src/smp_shared.rs` went 8 `unsafe` blocks to 4 in the same pass, the other two
being an `adrp` symbol load that `&raw const` expresses safely in edition 2024
and a `msr vbar_el1` that duplicated `exceptions::init`'s.

The run before it, 2026-08-31 (second run that day), used
`python3 scripts/cloc_akuma.py src crates` after
[`SYSCALL_UNSAFE_CLEANUP.md`](../archive/SYSCALL_UNSAFE_CLEANUP.md): `src/syscall/`
reached **zero** `unsafe` and took `#![forbid(unsafe_code)]` as a *module*
attribute — the first enforced ban outside `crates/`, which is why the crate
tally and the ban tally are no longer the same number (see
"The one enforced subtree in `src/`" below). The run before it, the same day, was
[`AKUMA_EXT2_CLEANUP.md`](../archive/AKUMA_EXT2_CLEANUP.md) §5 step 4: ext2
adopted the recoverable lock, its last three `unsafe` sites left, and the value
half landed as `akuma-locks-rw-cell`. **22 of 35 crates** forbid (the counter's figure; `akuma-alloc` and `akuma-uart` both joined 2026-08-31 as non-forbidding crates, so the denominator moved twice and the numerator did not). The three
runs before it were 2026-08-30 — the networking split
(`archive/AKUMA_NET_SPLIT.md`) in the morning, then steps 1–3 (the ext2 on-disk
codec) and `akuma-locks-rw` in the evening. Every number below comes from a run;
none was incremented by hand. Previously measured
2026-08-28 by grepping every
`crates/*/src/` tree and confirmed by building each crate with the ban in force
(`cargo clippy -p <crate> --target $HOST -- -D warnings`, 0 errors). `akuma-mmap`
was added 2026-08-29 and verified the same way; the `unsafe` counts in the second
table are unchanged from 2026-08-28.

**Correction (2026-08-29):** the count sentence below read "Ten of the eighteen"
while the two tables already listed 12 and 9 crates respectively. Both figures were
stale — re-derived here by counting `forbid(unsafe_code)` in every
`crates/*/src/lib.rs` (13 of 22, `akuma-mmap` included) rather than by incrementing
the old number.

**Twenty-two of the thirty-five** extracted crates are **unsafe-free and enforced so** (`enforced unsafe-free ... 22 of 35 crates`, straight from the counter). Each
carries a crate-level attribute in its `lib.rs`:

```rust
#![forbid(unsafe_code)]
```

`forbid`, not `deny`: `deny` can be switched back off by a module-local
`#[allow(unsafe_code)]`, `forbid` cannot. Adding an `unsafe` block to any crate
in the first table is a hard compile error, which is the point — these are the
crates whose whole value is that they are pure logic you can reason about and
host-test.

## Enforced unsafe-free

| crate | what it holds |
|---|---|
| `akuma-boot` | Linux `reboot(2)` ABI decode |
| `akuma-isolation` | box/namespace path confinement (`subdir_fs`) |
| `akuma-kacho` | the shared observe/decide/hysteresis layer for self-tuning policies |
| `akuma-locks-rw` | the recoverable reader/writer lock: release **is** abandon (orphaned-lock recovery as a CAS on the lock's own word), plus its host model checker |
| `akuma-ext2` | the ext2 driver: on-disk codec, block cache, directory and inode operations, deferred frees |
| `akuma-net-yarn` | the socket readiness wait loop as a pure state machine |
| `akuma-mmap` | `MmapRegion`, CoW-fork region inheritance, `munmap`'s clip-and-split, and the PTE permission vocabulary they speak |
| `akuma-syscalls-mem` | the memory family's decisions: mmap's mapping-kind plan and `MAP_FIXED` validation, mremap's move-vs-expand, madvise's advice decode and `MADV_DONTNEED`'s per-page rule, munmap's sizing, membarrier's command decode |
| `akuma-rump` | device-independent orchestration for the rump raw-L2 path |
| `akuma-scheduler` | discrete-event simulator for placement / netpoll wake policy |
| `akuma-syscalls` | the shape of a syscall excursion (prologue/epilogue decisions, identity-slot model) |
| `akuma-syscalls-sync` | the futex family: op decode, waiter table, deadline algebra, `WAKE_OP`, wait-loop outcome |
| `akuma-syscalls-poll` | the epoll/poll/select family: the fd-state → event-bits readiness map, the interest list and its `epoll_ctl` errno set, the `EPOLLET` armed-state decision, the `ppoll`/`pselect6` wire marshalling |
| `akuma-terminal` | terminal/line-discipline state |
| `akuma-syscalls-time` | time syscalls + the boot-time SNTP client |
| `akuma-vfs` | the `Filesystem` trait and common FS types |
| `akuma-net` | the smoltcp stack (`smoltcp_net/`, 14 modules), the AF_INET socket table, DNS |
| `akuma-net-unix` | the AF_UNIX state machine — codec, name table, rendezvous, framing, shutdown, credentials, datagram resolution |
| `akuma-syscalls-linux` | the Linux ABI: syscall numbers, `repr(C)` wire structs and their layout assertions, flag tables |
| `akuma-firecracker` | FDT parsing for the Firecracker microVM device map |
| `akuma-bkl` | the BKL protocol, the spinlocks under it, and `policy` — the decision to take it or skip it (the seven `no-bkl-*` phase toggles, the per-syscall opt-out bitmap) |
| `akuma-elf` | the ELF64 parser and program-header walk |
| `akuma-fpcache` | the shared file-page cache: physical frames for read-only file-backed mappings, keyed `(inode, mount id, offset)` and reference-counted through the CoW refcount |

`akuma-locks-rw` (2026-08-30) was born into the list, and it carries the list's
sharpest lesson: the plan (`AKUMA_EXT2_CLEANUP.md` §4.5) sketched a
*value-carrying* `RecoverableRwLock<T>` behind `forbid`, which stable Rust
cannot express — minting `&mut T` from `&self` needs an `UnsafeCell` deref. So
the crate locks **no value**: the recoverable protocol (flag word, owner cell,
per-tid reader holds, `abandon_tid`, the backstop kicker) is pure atomics and
`forbid` is real, and a consumer composes its own `UnsafeCell<T>` against the
tickets — the exclusivity proof the deref needs lives beside the value's owner.
It is also core-only: the plan's global reap registry became a per-lock
`abandon_tid` driven by whoever owns the locks (`src/vfs/ext2.rs`'s mount
registry, since step 4), which removed the crate's only reason to allocate.

`akuma-ext2` joined on **2026-08-31**, completing the plan, and the last step is
where the "who owns the thing being vouched for" law bit back. A consumer
composing `UnsafeCell<Ext2State>` *literally in ext2*, as the sentence above
prescribes, would have cost ext2 its own ban — so the composition went into
`akuma-locks-rw-cell`, **parametric over `T`**. That is the same trade `lock_api`
already makes for every `spinning_top` lock in this tree: the obligation is
about the lock word, not the payload, so it can be discharged once for all `T`
without the adapter ever naming `Ext2State` (which stays `pub(crate)`). Net
effect: two enforced crates and one 206-line unenforced one, instead of one
enforced crate and a 3,043-line unenforced driver.

`akuma-isolation` joined this list on 2026-08-28 rather than being born into it.
It had exactly one `unsafe`: a `core::str::from_utf8_unchecked` over a path
buffer built by concatenating two `&str`s. That is valid UTF-8 by construction,
so the unchecked call bought only the skipped validation pass — a walk over a
few tens of bytes. Replacing it with a checked `from_utf8(...).unwrap_or("")`
costs nothing measurable and removed the crate's last `unsafe`.

`akuma-mmap` (2026-08-29) was born into the list. It carries the stronger property
that `forbid(unsafe_code)` only implies: an **empty `[dependencies]` table**. It
cannot lock, allocate from the PMM, or name a `Process`, because there is nothing
to call — see [`AKUMA_EXTRACT_MMAP.md`](../archive/AKUMA_EXTRACT_MMAP.md) §3.

## How much of the tree is enforced-safe

Also generated by `scripts/cloc_akuma.py src crates` (2026-09-01, after
[`SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md`](../archive/SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md)):

| | |
|---|---|
| enforced unsafe-free crates | **23 of 39** |
| code in those crates | **25,658 of 49,706** lines under `crates/` (51.6%) |
| `unsafe` sites across `crates/` | **423** (412 production), of which **0** are inside an enforced crate |

Both percentages fell, and neither is a regression: 3,120 lines of exception
handling and 80 `unsafe` sites arrived under `crates/` in one move, enlarging
the denominator of the first ratio and the numerator of the second. The tree is
in exactly the state it was the day before, described more honestly — which is
the recurring hazard with these two numbers and the reason the run notes carry
the `src/` side as well. Read the ratios against the *tree* row of the scope
table below, not against `crates/` alone.

`cloc_akuma.py` also reports a second, different safety number: **95.6% of
production code under `crates/` sits outside any `unsafe` block.** The two answer
different questions and quoting only one misleads — the first is a *guarantee*
(the compiler refuses `unsafe` in that crate at all), the second a *measurement*
(lines that happen not to be in a block, in crates where one still could be
added). A crate with a single three-line block in three thousand lines scores 0%
on the first and 99.9% on the second.

### Production vs test `unsafe`

The counter splits the two, because a kernel test that pokes a page table is not
the same liability as a site on a live syscall path:

| scope | total | production | test |
|---|---:|---:|---:|
| `crates/` | 422 | 411 | 11 |
| `src/` | 88 | **11** | **77** |
| ├─ `src/syscall/` (enforced) | **0** | **0** | 0 |
| ├─ `src/console.rs` (enforced) | **0** | **0** | 0 |
| tree | **510** | **422** | **88** (17%) |

Regenerated 2026-09-01
([`AKUMA_EXCEPTIONS_EXTRACTION.md`](../archive/AKUMA_EXCEPTIONS_EXTRACTION.md)).
`src/` production **91 -> 11**, `crates/` **331 -> 411**, tree production
unchanged at **422**. The largest single relocation this table has recorded, and
the cleanest: the counts on the two sides sum exactly, so nothing was quietly
rewritten under cover of a move. `src/`'s remaining 11 are listed in the head
entry; its 77 test sites are untouched and are still the boot suite forging trap
frames by hand, which is the job.

The run before it, 2026-09-01 (`AKUMA_FDT_EXTRACTION.md`), read `crates/`
335/324/11 and `src/` 181/104/77, tree **516/428/88**. `src/` production 113 ->
**104**, `crates/` 321 -> **324**. Unlike every earlier row in this section the
tree total actually *falls* (434 -> 428) rather than holding while sites
relocate: three of the nine `src/` sites removed were the same
`Fdt::from_ptr` call written out three times, and two more were the same
speculative magic-check written twice. Deduplication, not just relocation — the
only kind of `unsafe` reduction that costs nothing to reason about.

This whole table is now emitted by the counter (`unsafe sites by scope`) rather
than assembled by hand from two runs, and **`src/` is in the per-crate table
above too**, marked `bin`. It had been omitted on the grounds that a bin crate
can never be `forbid`-enforced — which is true, and which left the tree's single
largest concentration of `unsafe` off the one table that measures `unsafe`.
Added 2026-08-31 (`INLINE_ASM_CLEANUP.md` §6).

Regenerated 2026-08-31 (third run that day) after `src/allocator.rs` became
[`akuma-alloc`](../archive/AKUMA_ALLOC_EXTRACTION.md) — see "The allocator is a
quarantine, not a cleanup" below. `src/` production fell 137 -> **116** while `crates/` rose
300 -> **320**: a move, not a removal, and that is the whole intent. The run
before it, the same day, read `crates/` 311/300/11 and `src/` 214/137/77 after
the framebuffer removal
([`FRAMEBUFFER_REMOVED.md`](../archive/FRAMEBUFFER_REMOVED.md)).

The run before that (earlier on 2026-08-31) read `crates/` 304/293/11 and
`src/` 239/162/77, tree **543/455/88**. `src/` fell 17 while `crates/` rose 7,
and the asymmetry is the point: the operations did not disappear, they moved
behind named functions in the crate that owns the thing being poked. Seven
crate-side sites replaced seventeen call-site ones, and the obligation is now
stated once instead of at every call.

The run before *that* (2026-08-30) read `crates/` 330/319/11 and `src/`
315/199/116, tree **645/518/127**. That drop was the `akuma-cpu` migration: 183
`asm!` sites that each carried a trivially-dischargeable `unsafe` block became
safe calls.

`src/`'s boot suite still holds most of its test share — the in-kernel tests
build page tables and forge trap frames by hand, which is the job. Production
density is **7.7 sites per kloc** of production code (was 8.6, and 9.8 before
that).

### The enforced subtrees in `src/`

There are two, as of 2026-08-31. `src/console.rs` joined `src/syscall/` when its
three PL011 register accesses became one `unsafe` in `crates/akuma-uart`
([`AKUMA_UART_EXTRACTION.md`](../archive/AKUMA_UART_EXTRACTION.md)). It is a
single file rather than a directory, but the mechanism is identical — a
module-level `#![forbid(unsafe_code)]` — and the motive is sharper: it is the
file that has to keep working when the allocator is what broke.


`src/syscall/` (23 files, 11,443 lines) carries `#![forbid(unsafe_code)]` in its
`mod.rs` since 2026-08-31 and reads **0 sites, 100.0% safe**. A bin crate can
never be `forbid`-enforced as a whole — page-table and trap-frame work is the
job there — but a *module* can, and this is the module that runs with
userspace-controlled arguments on every call.

*Written when `src/exceptions.rs` held 87 of the bin crate's sites and was
offered as the reason a bin crate can never carry the ban. It left for
`akuma-exceptions` on 2026-09-01 and `src/` production is now 11 sites; the
claim about bin crates still holds, but it now rests on `src/main.rs`'s boot
entry and two `unsafe extern "C"` blocks rather than on volume.*

The ban means no `unsafe` is written there. It does **not** mean the syscall
layer is proven sound: the genuinely-unsafe operations moved into
`akuma-cpu`/`akuma-mmu`/`akuma-pmm`/`akuma-exec`, three of them gaining a real
runtime check on the way (a PMM-managed-range bounds check, an installed-TTBR0
check, an SPSR-targets-EL0 check) and one — `with_own_process_exclusive` —
discharging two of its three clauses and resting on an enumerated call site for
the third. Full accounting:
[`SYSCALL_UNSAFE_CLEANUP.md`](../archive/SYSCALL_UNSAFE_CLEANUP.md).

`crates/`'s *test* `unsafe` fell 29 -> 16 on 2026-08-30: 12 of those were
`akuma-syscalls-linux`'s layout transmutes, rewritten as `offset_of!`.

**Resolved 2026-08-30 — and the option recorded here was the wrong one.** This
said `akuma-syscalls-linux` could not take a plain `#![forbid(unsafe_code)]`
because its 12 `transmute` layout assertions lived in `#[test]` bodies, and
proposed `#![cfg_attr(not(test), forbid(unsafe_code))]` to ban only production
`unsafe`. That would have preserved the tests as written. Rewriting them was
better: `offset_of!` + `size_of_val` state the same facts *directly*, and a
failure names the field instead of reporting a byte mismatch at an index. The
crate now carries a plain `forbid`.

Rewriting them also surfaced a real defect the transmutes were hiding — `MsgHdr`
was the one struct in the crate with *implicit* padding (56 bytes, 52 named), so
`transmute`ing it to `[u8; 56]` read four uninitialised bytes. See
[`AKUMA_NET_SPLIT.md`](../archive/AKUMA_NET_SPLIT.md) §6.5.

The last row is the one worth re-reading after any change: `forbid` makes it a
compile error, so a non-zero value there would mean the *counter* is wrong, not
that the ban was bypassed. The script says so itself if it ever prints one.

### The 55.1% is the flattering denominator

It is measured against `crates/` only, and **this document has never counted
`src/` at all** — which is where the kernel actually is. Including it:

| scope | total code | in enforced-safe crates | |
|---|---:|---:|---:|
| `crates/` | 45,709 | 25,194 | 55.1% |
| `crates/` + `src/` | 88,987 | 25,194 | **28.3%** |
| production only (no test code) | 52,871 | 15,069 | **28.5%** |

Refreshed 2026-09-01 from the same run as the tables above; the enforced-crate
production subtotal is the sum of the `prod code` column over the 22 crates the
counter marks `forbid`.

And `src/` carries **181 `unsafe` sites** — none of which appear in either table
above. The second table's 324 is the `crates/` production subtotal, not the
kernel's.

Two things follow. The enforced-safe crates are numerous but *small*, because the
property is easiest to keep in a leaf. And the honest headline for the tree is
**28.3% enforced-safe**, not 55.1% — the extraction programme moves that number
up one leaf at a time, but it starts from a bin crate that is a large share of
the codebase.

The heading and the headline both name a figure that moves; both were stale by
one run when this was refreshed on 2026-09-01 (they read 45.4% / 22.3%, from
2026-08-30). If you edit either number, edit both, and take them from a run.

Two jumps in one day, all on **2026-08-30**:

- Emptying `akuma-net` of `unsafe` (the device layer left for `akuma-net-nic`,
  and the two `SocketHandle` transmutes were deleted outright) took `crates/`
  from 31.2% to 36.8%, and `akuma-firecracker` carried it to 37.9% — 23.3% ->
  37.9% in one branch, tree-wide 11.3% -> 18.5%.
- Splitting `akuma-exec` (`AKUMA_EXEC_SPLIT_AGAIN.md`) took it 37.9% -> **45.4%**,
  tree-wide 18.5% -> **22.3%**. That crate went from 209 `unsafe` sites to 128,
  and two of the crates carved out of it — `akuma-bkl` and `akuma-elf` — reached
  `forbid` after first being judged irreducible.
- The ext2 on-disk codec (`AKUMA_EXT2_CLEANUP.md` §5 steps 1–2) emptied
  `akuma-ext2`'s blit and symlink families — 16 of its 18 production sites — and
  the orphaned-lock protocol landed as `akuma-locks-rw` (§5 step 3): enforced
  crates 20 -> 21, `crates/` 45.4% -> **45.9%**, tree-wide 22.3% -> **22.9%**.
  ext2's residual sites are the lock-recovery ones §5 step 4 removes.
- §5 step 4 removed them (2026-08-31): `akuma-ext2` swapped
  `spinning_top::RwSpinlock<Ext2State>` for `akuma-locks-rw-cell`'s
  `RecoverableCell<Ext2State>`, the three `force_unlock_write` sites and the
  thread hooks that fed them were deleted, and the crate took `forbid`. Enforced
  crates 21 -> **22 of 32**; `akuma-ext2`'s 3,043 production lines moved from
  unenforced to enforced, the largest single crate to make the move so far.
  Cost, measured (`scripts/benchmarks/locks_rw_ab.sh`): **+1.2 ns** per
  uncontended write acquire and **+0.8 ns** per read, against filesystem
  operations that cost microseconds.

## Not enforceable, and why

These crates contain `unsafe` that is not an artifact of convenience — removing
it would mean removing the crate's reason to exist.

**Counts are generated, not grepped** (2026-08-30):

```bash
python3 scripts/cloc_akuma.py src crates      # "Unsafe by crate" section
python3 scripts/cloc_akuma.py --self-test     # before trusting a surprising number
```

**Pass exactly `src crates`, and no narrower path alongside them.** Until
2026-08-31 the walker appended every file under every root argument with no
deduplication, so overlapping roots (`src crates src/syscall`) or a repeated one
double-counted the shared files — every column doubling in lockstep, which reads
as a plausible number rather than an obvious fault: `src/syscall` reported 46
files / 22,886 code against a true 23 / 11,443. The walker deduplicates now and
prints a `note:` when arguments overlap, and `--self-test` pins it. The
git-revision path (`walk_rev`) never had the bug — it filters one `git ls-tree`
listing, which names each path once.

`cloc_akuma.py` counts `unsafe` tokens lexed in **code** context, so a mention in
a doc comment, a string literal or an `asm!` body does not count — which a grep
cannot distinguish. The check that this works: every crate in the first table
reports **0 sites** despite several of them containing the word (in their own
`#![forbid(unsafe_code)]` line, if nothing else), and `forbid` makes a real
`unsafe` there a hard compile error.

The one figure that changed when the counts became generated is `akuma-exec`:
~216 (grep, 2026-08-28) → 221 (lexer, 2026-08-29). The old number came from
`grep -c`, which counts *lines containing* `unsafe`; 232 lines in that crate
mention it, 221 are real sites, and the two errors happened to partly cancel.

**Regenerated 2026-09-01** (after
[`AKUMA_EXCEPTIONS_EXTRACTION.md`](../archive/AKUMA_EXCEPTIONS_EXTRACTION.md)).
The whole `sites` column is from that one run, not just the new rows. Three rows
were **missing entirely** until this regeneration: `akuma-gic` and `akuma-psci`
had been created later on the day of the previous run, and `src (bin)` had been
promised by the "Production vs test `unsafe`" section above and never added.
A hand-maintained column hides drift in both directions — the previous run
caught `akuma-cpu` 19 -> 32 and `akuma-primitives` 14 -> 4 moving opposite ways;
this one caught three rows that simply were not there. Sorted by site count, so
the shape of the list is visible: a short head of genuinely hardware-facing
crates and a long tail holding one obligation each.

| crate | sites | why it is irreducible |
|---|---:|---|
| `akuma-exec` | 123 | trap frames, the thread-identity map, context switch, `user_access` |
| `akuma-exceptions` | 80 | the vector table, the EL0/EL1 trap handlers and the fault repair behind them. Irreducible in the strongest sense on this list: `forbid` rejects `global_asm!` and `#[unsafe(no_mangle)]` outright, and this crate is a vector table plus the five handlers it `bl`s. One stated contract at the top of `lib.rs` covers all 80, the shape `akuma-net-nic` and `akuma-gic` use |
| `akuma-mmu` | 64 | page tables, `UserAddressSpace`, ASIDs, the per-core TTBR free gate. Gained one: `boot_device_l3_phys`, the boot-table walk that let `rebuild_boot_device_table` stop being `unsafe` for its callers |
| `akuma-virtio` | 38 | MMIO and DMA by definition |
| `akuma-cpu` | 32 | `asm!` is unconditionally unsafe; the crate exists so ~160 tree-wide sites don't each have to say so |
| `akuma-net-nic` | 23 | DMA-visible frame arenas, virtio descriptor rings, the NIC MMIO doorbell, and smoltcp's `Device` impls |
| `akuma-alloc` | 20 | the `GlobalAlloc` impl, raw span claiming into Talc, and the canary reads/writes either side of every user pointer. **Deliberately quarantined rather than reduced** — see below |
| `src` (bin) | 3 | not a crate and never `forbid`-able as a whole: `akuma_fdt::locate` plus one `unsafe extern "C"` linker-symbol block each in `src/main.rs` and `src/smp_shared.rs`. **Only the first is an operation.** Was 91 before the exception path left, 11 before the boot-entry cleanup. Its two enforced *subtrees* (`src/syscall/`, `src/console.rs`) read 0 |
| `akuma-locks-rw-cell` | 8 | the `UnsafeCell<T>` derefs that turn an `akuma-locks-rw` ticket into `&T` / `&mut T`, plus the two `Send`/`Sync` impls that let the cell cross cores. Stable Rust cannot mint `&mut T` from `&self` without them, which is exactly why `akuma-locks-rw` carries no value and this crate exists. Irreducible *and* deliberately tiny: 206 lines, generic over `T`, so the obligation is discharged once for every consumer — the same bargain `lock_api` makes |
| `akuma-gic` | 5 | the whole GICv3 driver — distributor, redistributor, the `ICC_*_EL1` CPU interface. Every address passed to `mmio_w32`/`mmio_r32` is a device-mapped GIC register; do not lower those to `write_volatile`, which may emit a writeback form and make QEMU's HVF backend assert |
| `akuma-not-even-once` | 5 | `UnsafeCell` boot-registration cells; the safe alternative (`Spinlock<Option<T>>`) is a lock on the hottest indirection in the kernel |
| `akuma-primitives` | 4 | IRQ masking, per-CPU registers, the console writer |
| `akuma-fdt` | 3 | materialising the boot DTB: the eight-byte header probe and the `from_raw_parts` that follows it. **The whole crate exists to be these three**, so that `main.rs`, `platform.rs` and `smp_shared.rs` need none — see below |
| `akuma-pmm` | 3 | the physical frame allocator's own bookkeeping — the invariant that justifies them is this crate's own bitmap state, so they cannot move |
| `akuma-psci` | 2 | `smc_call` and `hvc_call`, one fixed instruction each with no branch inside the asm. **Deliberately not in `akuma-cpu`**: `smc` with `SYSTEM_OFF` halts the machine and with `CPU_ON` starts a core at an address you supply, so putting it in the crate every module depends on would let any safe code in the tree power the box off. Note the distinction — the *crate's own* `call`/`hvc_call`/`smc_call` are safe `pub fn`s, because the obligation being discharged is the `asm!` keyword; what stays out of `akuma-cpu` is the instruction, not the safety. Since 2026-09-01 this is also the kernel's **only** way to stop a machine: `halt()` calls `SYSTEM_OFF` |
| `akuma-timer` | 1 | CNTV/PL031 register access |
| `akuma-uart` | 1 | the single statement that `DEV_UART_VA` is a mapped PL011 window. The crate exists to hold exactly this |

**`akuma-fdt` was born into this table on 2026-09-01**, and it is the clearest
case in it of a crate whose *only* product is a discharged obligation. Before it,
"this pointer holds a complete FDT" was asserted six times in three files — twice
as a speculative `read_volatile` looking for the magic, and three times as
`Fdt::from_ptr` — because `fdt::Fdt::new(&[u8])` is safe and only the
pointer-to-slice step is not. Finding the blob's length once turns every consumer
into safe code: `platform::install_fdt_device_map` stopped being an `unsafe fn`
in the same change, and `smp_shared::probe_dtb` lost both of its blocks.

Two things came out of it that the duplication had been hiding. The replaced
sites disagreed about validation — two did none at all when the bootloader
supplied a pointer, one checked the magic but not the declared size — and
`Fdt::from_ptr` itself bounds `totalsize` not at all, so a wild pointer yields a
multi-gigabyte slice. And `fdt::Fdt::new` accepts any blob with a good header,
while `Fdt::memory()` and `Fdt::cpus()` **panic** on a missing node; this kernel
calls both and builds `panic = "abort"`. `Dtb::parse` checks for both nodes, so
the fallbacks the consumers already had ("using default 256MB", "staying
single-core") are what a malformed tree gets instead of a dead kernel.

**Three crates left this table on 2026-08-30**, all from the `akuma-exec` split:

- **`akuma-bkl`** (would have been listed at 4). Three sites were a hand-rolled
  `RawRwSpinlock` with **no consumers at all** — it shadowed
  `spinning_top::RwSpinlock` by name, so every grep for call sites found the
  *other* type's. A `#[deprecated]` probe settled it in one build. The fourth,
  `current_core_id`'s `mrs mpidr_el1`, moved to `akuma-primitives`.
- **`akuma-elf`** (would have been listed at 6). All six were "put these bytes in
  that frame" through `phys_to_virt`; they became
  `UserAddressSpace::write_page_bytes`, where `&mut self` is a real exclusivity
  proof.
- **`akuma-net`**, below.

The pattern in both new cases: the first analysis asked whether the *operation*
could be made safe and concluded no. The question that worked was **who owns the
thing being vouched for** — and in both cases the owner was already in scope.

**`akuma-net` left this table on 2026-08-30.** It was listed at ~43 sites for
"DMA-visible buffers and virtio descriptor rings" — a correct description of the
code, but of code that did not have to live in the same crate as the TCP/IP
stack. Splitting `akuma-net-nic` out moved every one of those sites into a crate
that is *only* the device, and `akuma-net` now carries `forbid`. The lesson
generalises: "irreducible" is a property of a body of code, not of a crate, and
a crate is irreducible only until someone draws the seam in the right place.
See [`AKUMA_NET_SPLIT.md`](../archive/AKUMA_NET_SPLIT.md) §5.1c.
| `akuma-syscalls-linux` | 12 | `transmute` layout assertions that pin `repr(C)` ABI types against Linux headers — **all 12 are in `#[test]` bodies**; the production half is unsafe-free (see above) |
| `akuma-timer` | ~8 | `mrs`/`msr` on CNTV/PL031 |
| `akuma-pmm` | ~6 | volatile reads/writes to physical frames |
| `akuma-firecracker` | ~2 | `pub unsafe fn describe_ptr` takes a raw FDT pointer from the caller — the unsafety is the *contract*, not the body |

**Two of these are worth re-checking as they shrink**, but neither is close
today: `akuma-firecracker`'s two sites are one genuinely-unsafe public signature
plus its own body, and `akuma-syscalls-linux`'s are layout assertions that could
in principle move to a checked byte-comparison.

### The allocator is a quarantine, not a cleanup

Full record: [`AKUMA_ALLOC_EXTRACTION.md`](../archive/AKUMA_ALLOC_EXTRACTION.md).

`akuma-alloc` (2026-08-31) is the first crate extracted with **no intention of
ever making it `forbid`**, and it is worth being explicit about why, because
every other entry in this document is about driving `unsafe` down.

It reports the lowest safe percentage in the tree — **57.0%**, 20 sites in 605
production lines — and that is the successful outcome. The goal was not to
reduce the kernel's total `unsafe`; the counter above shows it did not (`src/`
production 137 -> 116, `crates/` 300 -> 320). The goal was to get 20 trusted but
genuinely difficult sites — the `GlobalAlloc` impl, raw span claiming into Talc,
canary reads and writes either side of every user pointer — **out of the bin
crate**, where they sat amongst code that has no business being near them, and
into something with a name, a stated contract and a dependency list you can
read in one screen.

The dependency list is the part that actually earns the move. The pre-move file
reached into `akuma-exec` and, worse, into `crate::syscall` — an allocator
depending on the syscall layer *that allocates*. What is left depends only on
`akuma-primitives`, `akuma-pmm`, `talc` and `spinning_top`.

**How that inversion was removed matters, because the first attempt got it
wrong.** All five offending call sites were initially preserved as `OnceCopy`
hooks the bin registered at boot. That is worth recording as an anti-pattern: it
kept the inverted dependency — merely inverted *again*, through a function
pointer — and charged a registration step and 62 lines of machinery for the
privilege. Hooks are the right tool when a lower layer genuinely must call back
into a higher one (`PmmHooks`' reclaim ladder). They are the wrong tool when the
code simply belongs somewhere else.

None of the five was part of allocating. Four went where they belonged and the
fifth was deleted:

| was in the allocator | now |
|---|---|
| `#[global_allocator]` | `src/main.rs`. A **binary-level declaration**: a library that makes it decides the allocator for everything linking it. The crate exports `KernelAllocator`; the bin installs it |
| `#[alloc_error_handler]` + `current_process_shared` + `return_to_kernel` | `src/main.rs`. Also binary-level, and its body is OOM *policy* — "kill the process, not the kernel" — which needs the process table the heap should not know about |
| `syscall_counters::dump()` on allocation failure | that same handler. Returning null from `alloc` reaches it immediately, so the dump lost nothing by moving to where whole-kernel diagnostics belong |
| `current_syscall_nr()` + `current_thread_id()` on the `[HEAP]` line | **deleted.** Attribution on a 5 MB-boundary progress print did not justify an allocator knowing about syscalls or threads. The line still reports the size that drove the growth |

Zero hooks, and the crate needs no unstable feature. It is a plain `no_std`
library, so it is in `default-members` and builds for the host — which the hooked
version could not do (`#[alloc_error_handler]` "conflicts with allocation error
handler in: std"). Getting the layering right was what made it host-buildable;
that was a consequence, not the goal.

Expect more crates shaped like this one. "Isolate the trusted-but-difficult
`unsafe` so most of the kernel can stay safe" is a different objective from the
`forbid` tables above, and a crate that scores badly on those tables can still
be the right answer.

## Why the ban lives in `lib.rs` and not `Cargo.toml`

Cargo's `[lints]` table cannot express it per-crate here. Every crate inherits
the workspace lint set with:

```toml
[lints]
workspace = true
```

and Cargo rejects mixing that with crate-local lints outright:

```
cannot override `workspace.lints` in `lints`, either remove the overrides
or `lints.workspace = true` and manually specify the lints
```

So spelling the ban in `Cargo.toml` would mean dropping the inherit and copying
the whole ~45-entry `[workspace.lints]` table into each of ten manifests — ten
copies to drift out of step, which is the exact class of duplication
[`../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
exists to remove. The crate-level attribute is one line, is scoped to the crate,
and composes with the inherited lints.

If a future Cargo grows additive crate lints, moving the ban into the manifests
is mechanical.

## Adding a crate to the list

1. `grep -rn '\bunsafe\b' crates/<name>/src/ --include='*.rs'` — check the hits
   are code, not doc comments (`akuma-rump`'s only hit was a doc comment).
2. Add `#![forbid(unsafe_code)]` after any existing `#![...]` attributes.
3. `cargo clippy -p <name> --target $HOST -- -D warnings` and
   `cargo test -p <name> --target $HOST`.
4. Add a row above.
