# Crate safety: which crates forbid `unsafe`

**Grade: A** — regenerated 2026-08-31 (second run that day) with
`python3 scripts/cloc_akuma.py src crates` after
[`SYSCALL_UNSAFE_CLEANUP.md`](../archive/SYSCALL_UNSAFE_CLEANUP.md): `src/syscall/`
reached **zero** `unsafe` and took `#![forbid(unsafe_code)]` as a *module*
attribute — the first enforced ban outside `crates/`, which is why the crate
tally and the ban tally are no longer the same number (see
"The one enforced subtree in `src/`" below). The run before it, the same day, was
[`AKUMA_EXT2_CLEANUP.md`](../archive/AKUMA_EXT2_CLEANUP.md) §5 step 4: ext2
adopted the recoverable lock, its last three `unsafe` sites left, and the value
half landed as `akuma-locks-rw-cell`. **22 of 34 crates** forbid (the counter's figure; `akuma-alloc` and `akuma-uart` both joined 2026-08-31 as non-forbidding crates, so the denominator moved twice and the numerator did not). The three
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

**Twenty-two of the thirty-four** extracted crates are **unsafe-free and enforced so** (`enforced unsafe-free ... 22 of 34 crates`, straight from the counter). Each
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

Also generated by `scripts/cloc_akuma.py src crates` (2026-08-31, after the
`akuma-cpu` migration — [`INLINE_ASM_CLEANUP.md`](../archive/INLINE_ASM_CLEANUP.md)):

| | |
|---|---|
| enforced unsafe-free crates | **22 of 34** |
| code in those crates | **25,067 of 44,617** lines under `crates/` (56.2%) |
| `unsafe` sites across `crates/` | **311** (300 production), of which **0** are inside an enforced crate |

`cloc_akuma.py` also reports a second, different safety number: **96.5% of
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
| `crates/` | 332 | 321 | 11 |
| `src/` | 190 | 113 | **77** |
| ├─ `src/syscall/` (enforced) | **0** | **0** | 0 |
| ├─ `src/console.rs` (enforced) | **0** | **0** | 0 |
| tree | **522** | **434** | **88** (17%) |

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
never be `forbid`-enforced as a whole — `src/exceptions.rs` alone has 87 sites,
and page-table and trap-frame work is the job there — but a *module* can, and
this is the module that runs with userspace-controlled arguments on every call.

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

### The 45.4% is the flattering denominator

It is measured against `crates/` only, and **this document has never counted
`src/` at all** — which is where the kernel actually is. Including it:

| scope | total code | in enforced-safe crates | |
|---|---:|---:|---:|
| `crates/` | 44,205 | 20,296 | 45.9% |
| `crates/` + `src/` | 88,492 | 20,296 | **22.9%** |
| production only (no test code) | 52,759 | 11,966 | **22.7%** |

And `src/` carries **315 `unsafe` sites** — none of which appear in either table
above. The second table's 312 is the `crates/` production subtotal, not the
kernel's.

Two things follow. The enforced-safe crates are numerous but *small*, because the
property is easiest to keep in a leaf. And the honest headline for the tree is
**22.3% enforced-safe**, not 45.4% — the extraction programme moves that number
up one leaf at a time, but it starts from a bin crate that is a large share of
the codebase.

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

**Regenerated 2026-08-30** after the `akuma-exec` split
([`AKUMA_EXEC_SPLIT_AGAIN.md`](../archive/AKUMA_EXEC_SPLIT_AGAIN.md)).

| crate | sites | why it is irreducible |
|---|---:|---|
| `akuma-exec` | 128 | trap frames, the thread-identity map, context switch, `user_access` |
| `akuma-mmu` | 72 | page tables, `UserAddressSpace`, ASIDs, the per-core TTBR free gate |
| `akuma-virtio` | 38 | MMIO and DMA by definition |
| `akuma-net-nic` | 23 | DMA-visible frame arenas, virtio descriptor rings, the NIC MMIO doorbell, and smoltcp's `Device` impls |
| `akuma-alloc` | 20 | the `GlobalAlloc` impl, raw span claiming into Talc, and the canary reads/writes either side of every user pointer. **Deliberately quarantined rather than reduced** — see below |
| `akuma-cpu` | 19 | `asm!` is unconditionally unsafe; the crate exists so ~160 tree-wide sites don't each have to say so |
| `akuma-primitives` | 14 | IRQ masking, per-CPU registers, the console writer |
| `akuma-timer` | 8 | CNTV/PL031 register access |
| `akuma-uart` | 1 | the single statement that `DEV_UART_VA` is a mapped PL011 window. The crate exists to hold exactly this |
| `akuma-not-even-once` | 5 | `UnsafeCell` boot-registration cells; the safe alternative (`Spinlock<Option<T>>`) is a lock on the hottest indirection in the kernel |
| `akuma-locks-rw-cell` | 8 | the `UnsafeCell<T>` derefs that turn an `akuma-locks-rw` ticket into `&T` / `&mut T`, plus the two `Send`/`Sync` impls that let the cell cross cores. Stable Rust cannot mint `&mut T` from `&self` without them, which is exactly why `akuma-locks-rw` carries no value and this crate exists. Irreducible *and* deliberately tiny: 206 lines, generic over `T`, so the obligation is discharged once for every consumer — the same bargain `lock_api` makes |
| `akuma-pmm` | 3 | the physical frame allocator's own bookkeeping — the invariant that justifies them is this crate's own bitmap state, so they cannot move |

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
