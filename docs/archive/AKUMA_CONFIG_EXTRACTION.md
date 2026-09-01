# `src/config.rs` → `akuma-config`

**2026-09-01.** The kernel's tunables became a crate. `src/config.rs` went from
**1,216 lines to 11** — a pure re-export — and 94 consts are now visible to every
crate as `const`s rather than being handed over at runtime.

| | before | after |
|---|---:|---:|
| crates that forbid `unsafe` | 26 of 42 | **27 of 43** |
| enforced-safe code under `crates/` | 53.9% | **54.0%** |
| `…Config` structs in crates | 4 | **0** |
| dead consts | 14 | **0** |
| `extreme-size` | 732,664 B | 732,664 B (**unchanged**) |
| host test suites | 84 | 86 |

## Why: config-by-handover does not scale

Four crates had already grown a config struct and a shim in the binary to fill
it — `FpcacheConfig`, `VfsGlueConfig`, `LogConfig`, and `akuma-syscalls-ipc`'s
`init(bool)`. `src/syscall/` was about to need a fifth **with 26 fields**.

That pattern has a cost that compounds: **a `const` that becomes runtime config
stops const-folding whatever it gates.** Thirteen of the flags `src/syscall/`
reads are unconditionally `false` and guard trace blocks on the syscall path;
handing those over would have retained all of that code in *every* profile, not
just `extreme-size`. Two such flags in `akuma-vfs-glue` had already cost a page
of image ([`AKUMA_VFS_GLUE_EXTRACTION.md`](AKUMA_VFS_GLUE_EXTRACTION.md) §3).

A crate of `pub const`s folds across the LTO boundary exactly as it did in-crate,
and there is still exactly one definition of each value.

## Why not `akuma-primitives`

That was the other candidate, and the answer is size and charter.
`akuma-primitives` is ~860 code lines of *mechanism* — IRQ masking, per-CPU
registers, the console writer, the clock, errno, addresses, MMIO. This is ~1,150
lines of *policy*. Merging would make policy roughly half of that crate and
dilute a charter `CLAUDE.md` states explicitly.

It is also a rebuild-blast-radius question: **everything** depends on
`akuma-primitives`, so editing a tunable would rebuild the tree. A separate crate
rebuilds only actual consumers.

Dependency-wise the two were equivalent, which is what made the charter argument
decisive. `src/config.rs` had exactly **one** real code dependency —
`pub use akuma_exec::threading::types::MAX_THREADS` — and that chains down to
`akuma_primitives::preempt::MAX_THREADS`, where it is defined because every
per-slot static is indexed by it. So `akuma-config` depends on
`akuma-primitives` for that one re-export and on nothing else.

**Seven of the eight `akuma_*`/`crate::` mentions in the file were in doc
comments.** Grep the *code*, not the file, before concluding a module is coupled.

## The `build.rs` is load-bearing and its absence is silent

Sixteen consts are `#[cfg(kernel_profile_extreme)]` / `#[cfg(not(…))]` pairs:
`MAX_ARG_STRLEN` is 128 KiB or 4 KiB, `PROC_SYSCALL_LOG_ENABLED` is true or
false, and so on. **Cfgs do not travel with code.** Without forwarding, every one
of those pairs compiles the *non*-extreme arm even in an `extreme-size` build —
no build error, no runtime error, just the wrong numbers everywhere in the
profile whose entire purpose is being small.

Detected the way `akuma-exec` does it: `size` and `extreme-size` are the only
profiles at opt-level `z`, and the `extreme` feature discriminates. Three Cargo
features are forwarded too (`platform-firecracker`, `syscall-debug-info`,
`userspace-sshd`) because consts read them via `cfg!()`.

**Verified rather than assumed**, with a temporary `compile_error!` under the cfg:

| build | probe trips? | correct |
|---|---|---|
| `extreme-size` | yes | ✅ cfg arrives |
| `--release` | no (0 hits) | ✅ no leak |

Do this on any crate carved out of `src/`. This is the third time in two days a
missing cfg forward would have shipped silently — `akuma-vfs-glue`'s `/proc`
per-core accounting, `akuma-exec`'s `kernel_tests` gates, and now sixteen
tunables.

## The 14 dead consts, and why nobody noticed

```
ASYNC_THREAD_STACK_SIZE   ENABLE_TX_QUEUE        RUN_CONTAINER_TESTS
DEBUG_EXT2                ENABLE_USERSPACE_SSHD  SHELL_PS_DEBUG
ENABLE_IRQ_DEBUG_PRINTS   KERNEL_STACK_SIZE      SSH_BUILT_INS_FIRST
ENABLE_SSH_ASYNC_EXEC     MAIN_THREAD_PRIORITY_BOOST  SSH_PORT
                          TX_PACKET_BUFFER_SIZE  TX_QUEUE_SLOTS
```

The provenance is legible: `SSH_PORT`, `ENABLE_USERSPACE_SSHD`,
`SSH_BUILT_INS_FIRST`, `ENABLE_SSH_ASYNC_EXEC` and `SHELL_PS_DEBUG` are tombstones
of the **built-in SSH server and `ps` builtin removed 2026-08-10**
([`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md)); the `TX_*` trio predates the
`akuma-net-nic` rings.

**The mechanism was `#![allow(dead_code)]` at `src/config.rs:15`.** This tree
builds with `-D dead-code`, so every one of these would have failed the build the
moment its last reader disappeared. The allow is gone and **must not come back**
— it is the reason a removal can take the code and leave the tunable, three weeks
running.

Beware the measurement, too: a first pass reported "126 consts, 53 read", which
was wrong in both terms. 126 counted `pub use` lines as definitions, and 53
counted only the `crate::config::X` spelling, missing `use crate::config::X` plus
a bare `X`. The real figures are **108 definitions, 94 referenced, 14 dead** —
found by stripping comments from every other `.rs` in the tree and matching each
name as a whole token.

## What the four crates look like now

| crate | was | is |
|---|---|---|
| `akuma-fpcache` | `init(ram, FpcacheConfig)`, 2 tunables in atomics | `init(ram)`, tunables are `const fn`s |
| `akuma-vfs-glue` | `set_config(VfsGlueConfig)` + 7-field struct | 7 `const fn`s reading `akuma_config` |
| `akuma-syscalls-log` | `init(LogConfig)` + 2 atomics | 2 `const fn`s |
| `akuma-syscalls-ipc` | `init(bool)` + `AtomicBool` | 1 `const fn` |

`akuma-vfs-glue` also **shed the `#[cfg(kernel_profile_extreme)] const fn …
{ false }` arms** it needed to claw back const-folding — with the real consts
visible, no duplication is required at all. That is the clearest sign the seam was
in the wrong place before.

### One thing deliberately left as an atomic

`akuma-fpcache`'s `ENABLED` is **not** a const, and the reason is worth stating
because it looks like an oversight. It is doing double duty: the kill switch
*and* "`init` has run", and every `enabled()` reader goes on to use the caps
`init` computes from detected RAM. Those caps are genuinely runtime state. Only
the two tunables it used to be ordered against became consts — which is why the
old "armed last of the three" comment no longer describes a window that exists.

## Fallout worth knowing

**`clippy::too_long_first_doc_paragraph` fires on public library items but not on
a binary's.** Moving 1,200 lines of documented consts into a lib surfaced **22**
of them that had never warned. They are fixed, not suppressed — adding a blanket
allow here would have been the same mistake as the `allow(dead_code)` this
extraction just cleaned up.

**A mechanical doc reflow damaged one comment.** Re-wrapping first paragraphs
broke `DEBUG_SIGSEGV_SYSCALL_STUB_ELIR_MAX` across a line *inside* a backtick
span, which renders with a space in the middle of the identifier. Backtick counts
still balanced, so a parity check did not catch it; `grep '///.*_$'` did. If you
reflow doc comments programmatically, check for identifiers split mid-token.

## Verification

- Builds: `--release`, `extreme-size` (**byte-identical**, 732,664 B),
  `devbox-smoltcp`, `devbox` (rump).
- Clippy clean; 86 host suites, 0 failures; `cloc_akuma.py --self-test` passes.
- QEMU `SMP=4 MEMORY=2048M`: **265 pass / 0 fail**, `Procfs mounted at /proc`,
  `smp_shared_cores_online PASSED (3/3 secondaries)`.
- cfg forwarding probed in both directions (table above).

## Background

- [`SRC_SYSCALL_EXTRACTION.md`](SRC_SYSCALL_EXTRACTION.md) — why this was needed:
  `crate::config` was `src/syscall/`'s largest remaining dependency at 220 refs.
- [`AKUMA_VFS_GLUE_EXTRACTION.md`](AKUMA_VFS_GLUE_EXTRACTION.md) §3 — where the
  const-folding cost was first measured.
- [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md) — the first
  handover struct, and the first +bytes it cost.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the census.
