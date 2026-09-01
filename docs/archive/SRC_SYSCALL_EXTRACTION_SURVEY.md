# Moving `src/syscall/` out: a survey, not a move

**2026-09-01.** `src/syscall/` is 23 files, 16,661 lines, and already carries
`#![forbid(unsafe_code)]` as a module attribute — the most crate-ready thing left
in `src/`. It is also what 542 of the boot suite's `crate::` references point at,
so it is the prerequisite for any test crate.

The question asked was "can we just move it to `crates/akuma-syscalls-glue`? looks
like a clean move." **It is not clean.** This is what is in the way, measured, so
the next attempt starts from facts rather than from a look.

Nothing here was moved. The tree is unchanged apart from one blocker that fell
out cheaply on the way (`tprint!`, §4).

## The good news first: the inbound seam already exists

The hard direction is already solved. `akuma-exceptions` — a crate — reaches the
dispatcher through a function pointer:

```rust
// crates/akuma-exceptions/src/lib.rs
pub handle_syscall: fn(u64, &[u64; 6]) -> u64,   // in ExceptionHooks
...
let ret = (hooks.handle_syscall)(syscall_num, &args);
```

So the SVC path does not care where `handle_syscall` lives. Everything below is
about the **outbound** direction: what `src/syscall/` reaches back into.

## Blocker 1 — a dependency cycle, `src/syscall/` ↔ `src/vfs/`

This is the one that makes the move *impossible* rather than merely laborious.
Cargo crates cannot be mutually dependent.

```
src/syscall/  ──110 refs, 50 symbols──▶  src/vfs/
src/vfs/      ── 10 refs,  3 symbols──▶  src/syscall/
```

The back-edge is small and entirely in `src/vfs/proc.rs`:

| symbol | callers |
|---|---:|
| `crate::syscall::log::get_formatted` | 8 |
| `crate::syscall::log::list_pids_with_logs` | 1 |
| `crate::syscall::msgqueue::list_msg_queues` | 1 |

`/proc` exposes two registries that happen to live under `src/syscall/`:
per-pid log buffers (`src/syscall/log.rs`, 132 lines) and SysV message queues
(`src/syscall/msgqueue.rs`, 439 lines). Neither is syscall *dispatch* — they are
state that syscalls own and `/proc` reads.

**Cheapest break: extract those two files.** 571 lines with a tiny outbound
surface of their own (`crate::irq` ×20, `crate::config` ×7, `crate::timer` ×5,
`crate::tprint` ×4 — all of which §3/§4 already reduce to crate-available
symbols). Other consumers are `src/main.rs` (2) and `src/process_tests.rs` (8).
Extracting them cuts the cycle in one move and unblocks both directions.

The alternative — invert `/proc` onto a registration hook so a provider registers
itself — is more code and buys nothing here: the registries are not polymorphic
and have exactly one implementation.

## Blocker 2 — a second cycle, this one with the tests

`src/syscall/` is not purely production code. It carries **30 `cfg(kernel_tests)`
sites across 9 of its 23 files**, and two of them reach into the boot-test module:

```rust
// src/syscall/net.rs:1672, src/syscall/poll.rs:1072,1160,1230
register_process(pid, crate::process_tests::make_test_process(pid));
```

So `src/syscall/` depends on `src/process_tests.rs`, which depends on
`src/syscall/` (542 refs). A crate cannot reach the binary's test module, so the
move needs a decision first:

- **move the inline tests with the crate** — then `make_test_process` has to move
  down too (or be duplicated, which is worse); or
- **leave them in `src/`** — then those `cfg(kernel_tests)` blocks have to be
  lifted out of the 9 files first, which is a real edit to production files.

Neither is hard. Both have to be *chosen*, and neither is a file move.

## Blocker 3 — 19 outbound clusters, 160 distinct symbols

The raw reference count is misleading in both directions, so here it is split by
what a fix would actually cost. A hooks struct is priced in **distinct symbols**,
not references — and it cannot carry types at all.

| cluster | refs | distinct | what it would take |
|---|---:|---:|---|
| `crate::config` | 225 | 26 | consts → one `SyscallConfig` handed over at init. **Solved pattern**: `akuma-fpcache` already does exactly this, and `src/config.rs` stays the single source of truth |
| `crate::safe_print` | 165 | 1 | **free** — already `akuma_primitives::safe_print!` |
| `crate::vfs` | 110 | 50 | Blocker 1. 39 functions + **11 types**, so a hooks struct cannot cover it; `src/vfs/` (2,763 lines) has to move |
| `crate::irq` | 94 | **1** | **free** — all 94 are `with_irqs_disabled`, already `akuma_primitives::irq::with_irqs_disabled` |
| `crate::tprint` | 90 | 1 | **done** — see §4 |
| `crate::timer` | 47 | 2 | `akuma-timer` exists; `src/timer.rs` is a 374-line shim, so check which half each symbol is in |
| `crate::fs` | 30 | 13 | `src/fs.rs`, 298 lines, binary-local |
| `crate::pmm` | 27 | 11 | `akuma-pmm` exists; `src/pmm.rs` is 282 lines / 25 fns / **5 re-export lines**, so most of this is *not* a re-export |
| `crate::audio` | 10 | 10 | binary-local |
| `crate::block` | 9 | 5 | binary-local |
| `crate::smp_shared` | 5 | 5 | binary-local |
| `crate::rump_proxy` | 5 | 5 | binary-local |
| `rng`, `mmu`, `console`, `process_tests`, `nic_profile`, `bkl_profile` | 1–4 each | 1 each | small |

**The shape of it: 349 refs are 3 symbols.** `safe_print`, `irq::with_irqs_disabled`
and `tprint` together account for a fifth of all references and are (now) one
find-and-replace each. Another 225 refs are 26 consts behind a pattern this tree
already uses. What is left after those is `crate::vfs` and a long tail of
binary-local modules.

**Do not read "N lines, M refs" as difficulty.** `crate::irq` at 94 references
looked like the third-biggest problem and is a single function that already lives
in a crate; `crate::vfs` at 110 references is the thing that makes the move
impossible. Distinct symbols, and whether any of them are types, is the number
that matters.

## §4 — one blocker removed on the way: `tprint!`

`tprint!` was `safe_print!` plus a `[T<secs>.<cs>]` uptime prefix, defined in
`src/console.rs`. 116 call sites, **112 of them in `src/syscall/`** — it is
effectively that directory's trace macro, and it pinned all of them to the binary
crate.

Its doc comment explained why it stayed:

> Stays in this crate rather than moving to `akuma-primitives`: the timestamp
> comes from `crate::timer::uptime_us()`, and a leaf crate has no clock.

**That reason was already stale.** `akuma_primitives::clock` has held an uptime
hook since it was split out, registered by `akuma_exec::runtime::register` from
`ExecRuntime::uptime_us` — which `src/main.rs` sets to `timer::uptime_us`, *the
very function the macro was calling*. So the macro moved to
`akuma_primitives::console` beside `safe_print!` and now reads
`$crate::clock::uptime_us()`. Same clock, reached through a hook that already
existed.

A first pass added a **second** hook (`console::set_uptime_hook`) before someone
asked whether the runtime already had one. It did. The duplicate was removed
before it landed — worth recording, because "this leaf crate has no X" ages badly
in a tree that is actively moving X's into leaf crates, and the doc comment
asserting it was three refactors out of date.

`src/main.rs` re-exports the macro at its crate root (`pub use
akuma_primitives::{safe_print, tprint};`), so **all 93 `crate::tprint!` sites
resolve unchanged**; only four files calling it unqualified needed
`use crate::tprint;`.

Verified: full boot suite at `MEMORY=2048M`, **265 pass / 0 fail**, and **192
timestamped lines advancing `[T1.35]` → `[T390.04]`** — byte-identical format,
same clock.

## Suggested order, if this is picked up

1. **`src/syscall/log.rs` + `src/syscall/msgqueue.rs` → a crate.** Breaks the
   `/proc` cycle (Blocker 1's back-edge). 571 lines, small surface.
2. **Decide the `cfg(kernel_tests)` story** (Blocker 2). Cheapest is to lift the
   30 gated blocks out of the 9 files into `src/`, so the crate ships production
   code only.
3. **`crate::config` → `SyscallConfig`.** 225 refs, 26 consts, known pattern.
4. **Repoint `safe_print` / `irq::with_irqs_disabled`.** 259 refs, mechanical.
5. **`src/vfs/` → a crate**, or at least the 50 symbols `src/syscall/` needs from
   it. This is the large one and deserves its own survey.
6. **Then** the move, plus a hooks struct for the ~50-symbol tail (`fs`, `pmm`,
   `audio`, `block`, `smp_shared`, `rump_proxy`, …).

Steps 1–4 are worth doing on their own merits and leave the tree strictly tidier
whether or not the move ever happens. Step 5 is the real project.

## Deliberation: should `akuma-syscalls` and the glue crate later merge?

Asked explicitly, and the answer is **no** — but the reasoning is worth writing
down because the pull toward merging is real.

The family today:

```
akuma-syscalls-linux   2,274 lines   deps: none          forbid   (the ABI)
├─ akuma-syscalls      1,909 lines   deps: {linux}       forbid   (the shape) — 23 host tests
├─ akuma-syscalls-time    951 lines  deps: {linux, primitives, exec, timer}   forbid
├─ akuma-syscalls-sync  1,360 lines  deps: {linux, primitives}                forbid
├─ akuma-syscalls-poll  1,522 lines  deps: {linux, primitives}                forbid
└─ akuma-syscalls-mem     910 lines  deps: {linux, primitives}                forbid
```

**The case for merging is genuine.** `akuma-syscalls` has exactly one consumer —
the binary, and within it only `src/` and `src/syscall/`. The family crates do
**not** depend on it; they depend on the ABI crate directly. A crate with one
consumer, whose consumer is the crate you are about to create, is a 1:1
relationship and normally a smell. Threading shape types across the seam is
boilerplate that buys nothing if the two always ship together.

**The case against is stronger, and it is one number.** `akuma-syscalls` depends
on **one** crate. The glue would depend on roughly **fifteen**, plus a hooks
struct for the binary. Merging does not average those — it takes the shape crate's
dependency set from 1 to 15 and drags its 23 host tests, which today compile and
run in milliseconds against nothing but the ABI, behind the entire kernel graph.

That matters because of *what* those tests cover. The shape crate exists to make
"which counter bucket, which hooks run, where the epilogue's identity comes from"
answerable without booting. Fuse it into the dispatcher and that logic goes back
behind a VM boot — which is precisely the regression every extraction in
`docs/archive/` was undoing.

Both crates forbid `unsafe`, so no ban is at stake. That is **neutral**, not an
argument for merging; it just means the usual deciding factor does not apply here
and the dependency count does.

This is the same trade the tree has already made three times on a different axis
— `akuma-net` / `akuma-net-nic`, `akuma-boot` / `akuma-psci`, `akuma-locks-rw` /
`akuma-locks-rw-cell`: keep the half that can be checked cheaply separate from
the half that cannot. There the cheap property was `#![forbid(unsafe_code)]`;
here it is "compiles and tests without the kernel". Same shape, same answer.

**The test for whether the seam is drawn right** (borrowed from
`akuma-syscalls-mem`'s note about deliberately not depending on `akuma-mmap`): if
a change wants to put something in `akuma-syscalls` that needs a `Process`, a
mount table or an fd, the seam is in the wrong place — **move the seam, do not
add the dependency**, and do not merge the crates to make the problem go away.

**And not now regardless.** Merging is a pure refactor with no functional win, on
the path every syscall takes. Stability is worth more than crate-count tidiness;
revisit only if the seam starts costing real work, which one function-pointer
table does not.

## Background

- [`SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md`](SRC_BOOT_ENTRY_UNSAFE_CLEANUP.md) — the
  same day's work on `src/`'s own `unsafe`, and where `tprint!` fits.
- [`AKUMA_EXCEPTIONS_EXTRACTION.md`](AKUMA_EXCEPTIONS_EXTRACTION.md) — the
  `ExceptionHooks` precedent, and the crate that already reaches the dispatcher
  through it. It shed **8** `crate::` clusters before it could leave; this is 19.
- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §3 — "move the seam, do not add
  the dependency", stated first for `akuma-syscalls-mem`.
- [`../reference/subsystems/syscalls/`](../reference/subsystems/syscalls/) — the
  17 per-family syscall docs.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the census.
