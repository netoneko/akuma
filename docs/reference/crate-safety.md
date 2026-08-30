# Crate safety: which crates forbid `unsafe`

**Grade: A** — regenerated 2026-08-30 with `python3 scripts/cloc_akuma.py src crates`
twice in one day: the morning run followed the networking split
(`archive/AKUMA_NET_SPLIT.md`); the evening run followed
[`AKUMA_EXT2_CLEANUP.md`](../archive/AKUMA_EXT2_CLEANUP.md) steps 1–3 (the ext2
on-disk codec) and the new `akuma-locks-rw`. Every number below comes from that
run; none was incremented by hand. Previously measured
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

**Twenty-one of the thirty-one** extracted crates are **unsafe-free and enforced so**. Each
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
`abandon_tid` driven by whoever owns the locks (the mount table, at wiring
time), which removed the crate's only reason to allocate.

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

Also generated by `scripts/cloc_akuma.py src crates` (2026-08-30, evening run):

| | |
|---|---|
| enforced unsafe-free crates | **21 of 31** |
| code in those crates | **20,296 of 44,205** lines under `crates/` (45.9%) |
| `unsafe` sites across `crates/` | **324** (312 production), of which **0** are inside an enforced crate |

`cloc_akuma.py` also reports a second, different safety number: **96.3% of
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
| `crates/` | 324 | 312 | 12 |
| `src/` | 315 | 199 | **116** |
| tree | **639** | **511** | **128** (20%) |

(`src/` is untouched by the ext2 codec change, so its row is the morning run's;
the `crates/` row and the tree sum are the evening run's.)

`src/`'s boot suite holds 113 of those 145 on its own (`tests.rs` 79,
`process_tests.rs` 27, `sync_tests.rs` 7) — the in-kernel tests build page
tables and forge trap frames by hand, which is the job. Production density is
**9.7 sites per kloc of production code**.

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

## Not enforceable, and why

These crates contain `unsafe` that is not an artifact of convenience — removing
it would mean removing the crate's reason to exist.

**Counts are generated, not grepped** (2026-08-30):

```bash
python3 scripts/cloc_akuma.py src crates      # "Unsafe by crate" section
```

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
| `akuma-cpu` | 19 | `asm!` is unconditionally unsafe; the crate exists so ~160 tree-wide sites don't each have to say so |
| `akuma-ext2` | 2 (prod; +1 in `cfg(test)`) | the orphaned-lock recovery: `force_unlock_write` on a third-party `RwSpinlock`. The on-disk blits (14) and the symlink pair (2) left on 2026-08-30 for the explicit codec (`AKUMA_EXT2_CLEANUP.md` §5 steps 1–2); the lock sites leave at step 4, when `akuma-locks-rw` is adopted and the three poll loops collapse into it |
| `akuma-primitives` | 14 | IRQ masking, per-CPU registers, the console writer |
| `akuma-timer` | 8 | CNTV/PL031 register access |
| `akuma-not-even-once` | 5 | `UnsafeCell` boot-registration cells; the safe alternative (`Spinlock<Option<T>>`) is a lock on the hottest indirection in the kernel |
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
