# Moving `src/syscall/` out

**2026-09-01.** `src/syscall/` is 23 files, 16,661 lines, and already carries
`#![forbid(unsafe_code)]` as a module attribute — the most crate-ready thing left
in `src/`. It is also what 542 of the boot suite's `crate::` references point at,
so it is the prerequisite for any test crate.

The question asked was "can we just move it to `crates/akuma-syscalls-glue`? looks
like a clean move." **It is not**, and this doc is the survey that says why, plus
the first three steps of the answer, which are done.

## Status

| step | what | state |
|---|---|---|
| 0 | `tprint!` → `akuma-primitives` (§4) | **done** |
| 1a | `src/syscall/log.rs` → `akuma-syscalls-log` | **done** |
| 1b | `src/syscall/msgqueue.rs` → `akuma-syscalls-ipc` | **done** |
| 2 | `src/vfs/` → `akuma-vfs-glue` | **done** |
| 3 | decide the `cfg(kernel_tests)` story (Blocker 2) | open |
| 4 | `crate::config` → `SyscallConfig` (225 refs / 26 consts) | open |
| 5 | `src/syscall/` → `akuma-syscalls-glue` | open |

Steps 1a–2 are recorded in [§7](#7-what-was-actually-done). The cycle that made
the move impossible is **broken**: `src/vfs/` no longer exists, and nothing
outside `src/syscall/` points back into it.

Census movement across the whole run: **23 of 39 crates forbid → 26 of 42**, and
enforced-safe code under `crates/` went **25,658 / 49,706 (51.6%) → 28,135 /
52,202 (53.9%)**.

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

## Blocker 1 — a dependency cycle, `src/syscall/` ↔ `src/vfs/` — **RESOLVED**

This is the one that made the move *impossible* rather than merely laborious.
Cargo crates cannot be mutually dependent. Cut on 2026-09-01; the survey below is
what it looked like, and [§7](#7-what-was-actually-done) is what was done.

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

1. ~~`src/syscall/log.rs` + `src/syscall/msgqueue.rs` → crates.~~ **Done**, §7.1–7.2.
2. ~~`src/vfs/` → a crate.~~ **Done**, §7.3. This was billed as "the real
   project" and came in at 20 distinct symbols because four of the six clusters
   that looked binary-local were already re-exports of crates. **Price an
   extraction by resolving each symbol, not by counting references** — the two
   numbers were off by an order of magnitude here in the safe direction, and by
   an order of magnitude in the *unsafe* direction for `crate::irq` (94 refs, one
   function).
3. **Decide the `cfg(kernel_tests)` story** (Blocker 2). Cheapest is to lift the
   30 gated blocks out of the 9 files into `src/`, so the crate ships production
   code only. This is now the first blocker, and it is a choice rather than a
   measurement.
4. **`crate::config` → `SyscallConfig`.** 225 refs, 26 consts, known pattern —
   and watch the size floor per §7.4.
5. **Repoint `safe_print` / `irq::with_irqs_disabled`.** 259 refs, mechanical.
6. **Then** the move, plus a hooks struct for the tail (`fs`, `audio`,
   `smp_shared`, `rump_proxy`, …). `crate::vfs` (110 refs, 50 symbols) is now
   `akuma_vfs_glue::` and costs nothing.

## 7. What was actually done

Three crates, in dependency order. Each was built, clippied, host-tested and
booted at `SMP=4 MEMORY=2048M` before the next began.

### 7.1 `akuma-syscalls-log` — 132 lines

The per-pid syscall trace rings behind `/proc/<pid>/syscalls`. Moved for the
cycle, not for tests: it has no `unsafe` and no pure logic worth a host test.

Its outbound surface was six symbols, and **four of them were free because of
§4** — `crate::tprint` had just become `akuma_primitives::tprint`, and
`crate::irq::with_irqs_disabled` / `crate::timer::uptime_us` were already
`akuma_primitives::irq` / `akuma_primitives::clock`. Only the three
`crate::config` consts needed work, handed over as a `LogConfig` at `init`.

`src/syscall/log.rs` survives as a shim that re-exports `record`/`mark_exited`/
`get_formatted` and calls `init` — **because `src/config.rs` stays the single
source of truth** and the crate cannot read it. `/proc` was repointed at
`akuma_syscalls_log::` directly; routing it back through the shim is what the
cycle *was*.

### 7.2 `akuma-syscalls-ipc` — 439 lines

The SysV message-queue family (`msgget`/`msgctl`/`msgsnd`/`msgrcv`). The fifth
syscall family to leave `src/syscall/` after `-time`, `-sync`, `-poll` and
`-mem`, and the first to leave for a structural reason rather than for host
tests.

**`use super::*` hid its real dependencies.** The file's only visible imports
were `alloc` and three `akuma_exec::threading` items; everything else came
through the glob. Commenting the glob out and reading the compiler's complaints
enumerated it in one build: 7 errno constants, 5 `user_access` items, `Spinlock`,
`AtomicU32`, `BTreeMap`, `Vec`. All crate-available —
`akuma_primitives::errno::negated` and `akuma_exec::process::user_access` — plus
a local re-derivation of `validate_user_ptr`, which is a four-line forwarder over
`validate_user_range(.., Prefault::Yes)` in `src/syscall/mod.rs`. **If you are
about to move a file that opens with `use super::*`, do this first**; the import
list is not the dependency list.

Unlike its four siblings this crate is *not* "the pure logic with the effects
left behind" — it owns the queue table, does its own user copies, wakes its own
pollers. The seam is a dependency edge, not a purity boundary, and the header
says so rather than implying a cleanliness it does not have.

The dependency is `optional = true` behind `sc-sysv-ipc`, exactly as the module
it replaced was gated: **a crate split must not quietly re-add 440 lines to the
`extreme-size` floor.**

### 7.3 `akuma-vfs-glue` — 2,763 lines

The mount table, per-box namespaces, the ext2-over-virtio adapter and `/proc`.
`akuma-vfs` owns the *vocabulary* (`Filesystem`, `DirEntry`, `FsError`,
`MountTable`); this crate owns the kernel's single **instance** of it.

Its outbound surface priced out at 20 distinct symbols / 47 refs — an order of
magnitude cheaper than `src/syscall/`'s 160 / 900+ — and **checking each one
before writing a hook is what kept the hooks struct at four members**:

| looked binary-local | actually |
|---|---|
| `crate::block` (4 symbols) | a re-export of `akuma_virtio::block` |
| `crate::pmm::stats` | `akuma_pmm::stats` |
| `crate::file_page_cache::{invalidate_inode,len}` | `akuma_fpcache::` |
| `crate::timer::uptime_us` | `akuma_primitives::clock::uptime_us` |

What was left is genuinely the binary's: an inline `mod audio` in `main.rs`,
`fs::exists`, `smp_shared::probed_core_count`, and `timer::utc_time_us` (which
needs the binary's boot uptime to turn monotonic microseconds into UTC). Four
function pointers, on the `ExceptionHooks` model, unregistered-is-quiet.

Three things went wrong and are worth repeating:

- **A brace-form `use` slipped the symbol survey.** The regex counting
  `crate::config::NAME` never matched `use crate::config::{MAX_THREADS,
  PROC_STDOUT_MAX_SIZE}`. Two more consts, found by the compiler. Grep for
  `use crate::x::{` separately.
- **The crate needed a `build.rs`, and its absence would have been silent.**
  `proc.rs`'s `active_core_count` is `#[cfg(kernel_smp_shared)]` with a `1`
  fallback, and it sizes the per-core CPU-time accounting `/proc` reports.
  Without the cfg forwarded, the crate compiles the fallback **even under real
  SMP** — no build error, no runtime error, just `/proc` dividing by one core on
  a four-core machine. `akuma-exec` shipped this exact bug for its
  `kernel_profile_extreme` gates. Any crate carved out of `src/` inherits every
  `kernel_*` cfg its code reads, and cfgs do not travel with the code.
- **Four binary features gate the moved code** (`sc-containers` ×8, `sc-reboot`,
  `sc-sysv-ipc`) and had to be declared on the crate and forwarded. The symptom
  is `cannot find function … in module crate::vfs` against a `pub fn` that plainly
  exists — a feature-gated item, not a visibility problem.

### 7.4 What it cost

**`extreme-size` grew exactly 4,096 bytes** — 728,568 → 732,664, measured with
`cargo clean -p akuma` on both arms because an incremental rebuild reports no
change at all and will happily tell you there is no regression. Exactly one page
is an alignment boundary being crossed rather than 4 KB of new code, and against
a 3,070 KB image with a 4 MB floor it is 0.13% of the headroom.

Part of it was found and removed before measuring: `src/config.rs` forces
`PROC_SYSCALL_LOG_ENABLED` and `PROC_SYSVIPC_ENABLED` to `false` on
`kernel_profile_extreme`, and while they were `const` the `/proc` renderers
behind them were **const-folded out of the image**. Handing them over as runtime
config turned both into loads and retained both renderers. The crate's getters
are now `#[cfg(kernel_profile_extreme)] const fn … { false }`, which is a
deliberate duplication of a fact `src/config.rs` also states — the alternative is
paying for a renderer the profile can never reach.

**This is the general tax on config-by-handover** and the reason
`akuma-fpcache`'s extraction documented its own +304 bytes. Every const that
becomes runtime config stops const-folding whatever it gates. Check the size
floor on any extraction that moves a `bool` out of `src/config.rs`.

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
