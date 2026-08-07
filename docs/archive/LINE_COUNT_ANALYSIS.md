# Line-count statistics and what they might mean (2026-08-07)

An exploration, not a conclusion. `src/` + `crates/` was counted, split into
production vs test code, and then compared against other kernels to see which
readings of the numbers survive contact with context. Several don't.

Companion doc: the profile/image-size half of this investigation — what a
profile's *bytes* cost, which is a different question with a different answer —
lives in [`reference/build-profiles.md`](../reference/build-profiles.md).

**Measured at commit `d3f28d6`, branch `another-smp-attempt-0`.**

> Every Akuma number here is measured. Every number about *another* kernel is a
> recalled approximation, marked `~`, and should be re-measured before being
> cited anywhere. No other kernel's source was available in this session.

---

## Method

`scripts/cloc_akuma.py src crates`. It lexes Rust (string/raw-string literals,
char-vs-lifetime, nested block comments, `asm!` interiors) and classifies each
line as production or test. A line is test code if its file is a test file
(`*_tests.rs`, `tests/`, …), if it sits under `#[test]`/`#[bench]`, or if its
`#[cfg(…)]` can only hold in a tests-enabled build — including Akuma's own
`#[cfg(not(any(feature = "no-tests", …)))]` boot-suite gate, since the in-kernel
suite runs on bare metal rather than under `cargo test`. Full rules are in the
script's docstring.

Trust check: per-file diff against `cloc 2.08` over all 172 Rust files —
**171 match exactly** on blank/comment/code. The one disagreement is a literal
blank line inside a multi-line string (`src/sync_tests.rs:2531`), which this
counter calls code (it is part of a string token) and cloc calls blank.

---

## The numbers

```
Language                   files     blank   comment      code    % test
Rust                         172     12074     23809     74535     34.9%
Markdown                       5       143         0       281      0.0%
TOML                          12        31        86       161      0.0%
SUM                          189     12248     23895     74977     34.7%
```

| bucket | files | blank | comment | code |
|---|---|---|---|---|
| Production | 169 | 7,150 | 17,342 | **48,942** |
| Tests | 20 | 5,098 | 6,553 | **26,035** |

- comment / code = **31.9%**
- test code / production code = **0.53x**
- 111,120 physical lines

Production code by area (48,500 Rust lines; the other 442 are TOML + Markdown):

| area | prod code | share |
|---|---|---|
| Process / threads / MM | 13,997 | 28.9% |
| Syscall layer | 9,226 | 19.0% |
| CPU / exceptions / SMP | 7,369 | 15.2% |
| Networking | 4,289 | 8.8% |
| Filesystems / VFS | 3,846 | 7.9% |
| Shell (in kernel) | 3,425 | 7.1% |
| Boot / drivers / misc | 3,081 | 6.4% |
| SSH server (in kernel) | 2,427 | 5.0% |
| Editor + terminal | 840 | 1.7% |

Grouping: `akuma-exec` + `allocator.rs`/`pmm.rs`/`syscall/mem.rs` +
`akuma-isolation` → process/MM; `src/syscall/` → syscall layer;
`exceptions.rs`/`smp*`/`daif*`/`irq*`/`gic*`/`timer*`/`akuma-smp` → CPU;
`akuma-net`/`akuma-rump`/`rump_proxy.rs` → networking;
`akuma-ext2`/`akuma-vfs`/`src/vfs/` → filesystems.

---

## Stat 1: 49k lines of production code

**Reading A — "that's a lot for one kernel."** True against the teaching-OS
reference points most people carry:

| kernel | ~prod lines | runs a Linux userspace? |
|---|---|---|
| xv6-riscv | ~6–7k C (kernel) | no |
| seL4 (verified core) | ~10k C | no (microkernel; needs a userland OS personality) |
| Akuma | 48,942 Rust | yes |
| Linux | ~30M+ | it *is* the reference |

**Reading B — "it's small for what it does," and this is the one that holds.**
The comparison to xv6 is unsound, and an earlier draft of this analysis made it
before catching itself. xv6 has no `mmap`, no threads or `clone`, no signal
delivery, no networking, no dynamic linking, and no real libc; base xv6 `fork`
copies eagerly, with CoW left as a lab exercise. It runs ~21 syscalls and its own
handful of C utilities cross-compiled on the host — it cannot host rustc, or
llama.cpp, or apk, or anything else needing `mmap` + threads + musl.

Those omissions are *precisely* Akuma's two largest areas. Process/threads/MM
(13,997 lines, 28.9%) is CoW fork, `CLONE_VM`, real address spaces, lazy mmap,
demand paging, thread groups, signals. The syscall layer (9,226 lines, 19.0%) is
17 syscall families — `src/syscall/fs.rs` alone is 2,201 lines. Nearly half the
kernel is the cost of the Linux ABI, and the ABI is the entire point: it's why
unmodified musl binaries run.

So the honest framing is that **line count tracks ABI surface, not feature
count**, and a fair yardstick has to be a kernel that runs an unmodified Linux
userspace. Against xv6 the number looks bloated; against anything that hosts a
real toolchain it doesn't.

**Reading C — the distribution is the interesting part, not the total.**
`src/exceptions.rs` (3,011 prod lines) and `src/smp.rs` (2,629) are the two
largest production files, and they are also where most of this project's
debugging history lives — the ON_CPU scheduler race, the ESR-snapshot fix, the
BKL dropped-window ledger, the phantom-SVC guards. Two competing interpretations,
both plausible:

- *Inherent:* exception and SMP entry paths are irreducibly hairy, and the lines
  are hard-won correctness.
- *Structural:* files that large are where the next bug hides, and the
  concentration of past fixes is evidence of under-decomposition rather than of
  inherent difficulty.

Nothing in a line count distinguishes these. Defect density per file over git
history would.

---

## Stat 2: 0.53x test-to-code — the most misleading number here

26,035 test lines against 48,942 production lines looks like strong discipline.
Three things complicate it.

**It is ~25x Linux's in-tree ratio, which means almost nothing.** Linux's in-tree
tests — `tools/testing/selftests/`, `lib/test_*.c`, KUnit behind
`CONFIG_*_KUNIT_TEST`, scattered driver selftests — are on the order of **~1–2%
of its code (~0.02x)**. But Linux's real coverage isn't in the tree: LTP,
syzkaller, xfstests, KernelCI, the 0-day bot, and distro QA are. Akuma's tests
are all in-tree because **there is no external ecosystem pointed at it** — the
boot suite *is* the harness.

So the in-tree ratio measures *where tests live*, not how much testing exists. A
mature kernel at 0.02x can be better tested than a young one at 0.53x by orders
of magnitude. Comparing the two as quality signals is a category error.

**A different tradition replaces tests entirely.** seL4 has a famously small test
suite and ~20x its code size in Isabelle/HOL proof (~10k lines C, ~200k+ lines of
proof). Under a "verification instead of testing" model the test ratio approaches
zero while confidence goes up. The ratio is not a quality axis at all — it's an
artifact of methodology.

**The distribution undercuts the aggregate.** 19,781 of the 26,035 test lines are
in three files:

| file | test code |
|---|---|
| `src/process_tests.rs` | 9,644 |
| `src/tests.rs` | 6,480 |
| `src/sync_tests.rs` | 1,679 |

Meanwhile, by component:

| component | prod code | test code | ratio |
|---|---|---|---|
| `src/syscall` | 9,931 | 117 | 0.01x |
| `src/vfs` | 1,144 | 0 | — |
| `crates/akuma-isolation` | 477 | 0 | — |
| `src/shell` | 2,571 | 32 | 0.01x |
| `src/ssh` | 1,342 | 13 | 0.01x |

The syscall layer — the second-largest area of production code, and the surface
every musl binary hits — sits at Linux-like in-tree ratios *without* Linux's
external safety net. This is the one number in this document that suggests an
action rather than an interpretation, and it doesn't depend on any cross-kernel
comparison to be worth acting on.

---

## Stat 3: 31.9% comment-to-code

High for a systems codebase; `~15–20%` is the usual range quoted for Linux.

**Reading A — a debugging-history artifact.** Much of Akuma's commentary records
*why* an invariant exists, often citing the archived investigation that
established it (`Cargo.toml`'s feature block is an extreme case: several hundred
words per feature, with measured A/B results inline). That is unusually valuable
and unusually verbose.

**Reading B — a complexity tell.** Code needing that much explanation may be code
whose invariants aren't expressible in its structure. The BKL carve-out comments
are the test case: they exist because "which lock protects this, under which
feature combination" cannot currently be read off the types.

Both readings are consistent with the same measurement.

---

## Stat 4: 14% of the kernel is userspace-shaped

6,692 production lines — shell (3,425), SSH (2,427), editor + terminal (840) —
implement services other kernels put in userspace. Monolithic by choice, not by
accident: on the 4 MB `extreme-size` profile these make the box reachable with no
disk and no userspace process at all.

**The trap in this stat:** it invites "move them out and the kernel shrinks by
14%." Lines and bytes don't agree here. SSH is 5.0% of production *lines* but its
measured symbol footprint is ~34 KB of an ~882 KB image (~3.9%), while the
userspace `sshd` that would replace it is a 142 KB loadable image before any
runtime cost. Whether the move is a win is a profile question, measured in
[`reference/build-profiles.md`](../reference/build-profiles.md), and the answer
there points the opposite way from the line count.

---

## Stat 5: 1.2% dead code — and two-thirds of it is tests

Measured, so this is no longer a gap in the analysis. `dead_code` is **`deny`
workspace-wide** (`Cargo.toml` `[workspace.lints.rust]`), so dead code cannot
accumulate unnoticed: everything dead is behind one of **76 explicit
`#[allow(dead_code)]`** sites. `RUSTFLAGS="--force-warn dead_code"` overrides
those attributes without editing source, which is what produced these numbers
(third-party crates filtered out; run in an isolated `CARGO_TARGET_DIR`).

| config | dead items | dead lines |
|---|---|---|
| default features | 64 | 879 |
| `size` feature set | 59 | 325 |
| `smp-shared` | 66 | 897 |

879 lines is **1.2% of all code**. Production-only: **270 lines, 0.55% of
48,942**. By area, default features:

| area | items | lines |
|---|---|---|
| orphaned tests | 17 | **609** |
| `src/syscall` | 11 | 86 |
| `crates/akuma-exec` | 6 | 81 |
| `src/*` (top level) | 26 | 78 |
| `crates/akuma-ext2` | 2 | 16 |
| `src/vfs` | 2 | 9 |

**Reading A — "0.55% dead production code is excellent," and it is.** A
`deny`-by-default lint plus 76 deliberate exemptions is why. There is no rot here
to clean up; the number is a property of the lint configuration more than of the
code.

**Reading B — the interesting 69% is test code that never runs**, which the
0.53x ratio in Stat 2 counts as coverage. Two clusters, different causes:

- **`src/tests.rs`** — 6 allocator pattern tests (296 lines) plus `run_all`
  itself. The entry point is dead: `src/tests.rs:3` documents "Run with
  `tests::run_all()` after scheduler initialization" and nothing calls it
  (`src/main.rs:1015` calls `async_tests::run_all()`, a different module). The
  file's whole suite is unwired, which is *why* its tests are unreachable.
- **`src/syscall/msgqueue.rs`** — 9 dead functions, plus the 5 msgqueue tests in
  `process_tests.rs` written against them (277 lines). The syscall family is
  wired (`MSGGET`/`MSGCTL`/`MSGRCV`/`MSGSND`, `src/syscall/mod.rs:960-966`), but
  the poller layer on top of it is not: `msgqueue_add_recv_poller`,
  `add_send_poller`, `recv_pollers_count`, `send_pollers_count`,
  `is_recv_poller`, `push_direct`, `pop_direct`, `message_count`,
  `cleanup_box_queues`. Poller support and its tests landed without being hooked
  up.

Also dead: `src/process_tests.rs:4636 test_forktest_parent_mmap` (52 lines) —
mmap-under-fork being a path with its own bug history.

This sharpens Stat 2 rather than contradicting it: 609 of 26,035 test lines
(2.3%) are counted as tests but cannot run. Small, but it is exactly the kind of
error the aggregate ratio is blind to.

**The remainder is deliberate or benign:**

- `src/console.rs` (10 items) — the entire UART *input* path (`has_char`,
  `getchar`, `getchar_blocking`, `read_line`, `read`/`flags`/`has_data`,
  `FR_OFFSET`/`RXFE`/`TXFF`/`BUFFER_SIZE`). Expected: SSH is the console.
- `src/config.rs` — 12 unused constants under default, 17 under the `size` set.
- `crates/akuma-ext2` `hold` fields (×2) — RAII guards, where never-being-read is
  the point. False positives in spirit.
- 12 items are dead **only** under the `size` feature set — test hooks that
  `no-tests` orphans (`futex_wait_at_tgid_for_test`, `spurious_svc_count`,
  `DISABLE_ALL_TESTS`, …). Live in the default build; not a leak.

**Two limits on these numbers.** `pub` items in `crates/` are exempt from
`dead_code` (rustc assumes public API is used), so the crates are under-reported —
the 6 `akuma-exec` hits are only the non-`pub` ones. And this is *compile-time
reachability*, not execution: code that is reachable but never actually runs on
any boot would need coverage instrumentation, which means booting a VM.

---

## What none of these numbers can tell you

- **Whether the code is correct.** The areas with the most lines and the most
  comments are also the areas with the longest bug histories; the correlation is
  real and uninformative as to cause.
- **What actually executes.** Stat 5 measures compile-time reachability, not
  runtime coverage. Some of the 48,942 production lines may never run on any
  boot, and nothing here would show it.
- **How much would survive a Linux-ABI conformance suite.** 17 syscall families
  exist; how completely each implements its family is unmeasured.
- **Anything about size.** A 30 KB precomputed table is one line of Rust
  (`ED25519_BASEPOINT_TABLE` is exactly that). Lines are a proxy for maintenance
  burden, never for image size.

---

## Reproducing

```bash
scripts/cloc_akuma.py src crates
scripts/cloc_akuma.py src crates --json
scripts/cloc_akuma.py src crates --by-file --top 25
scripts/cloc_akuma.py src crates --no-kernel-test-gate   # boot suite → production (+234 lines)

cloc --quiet --by-file --csv --include-lang=Rust src crates   # cross-check
```

Dead code (Stat 5) — `--force-warn` overrides the `#[allow(dead_code)]` sites
without editing source, and a separate `CARGO_TARGET_DIR` keeps it from
invalidating anyone else's build cache:

```bash
export CARGO_TARGET_DIR=/tmp/dc-target
export RUSTFLAGS="--force-warn dead_code --force-warn unused_imports --force-warn unused_variables"
cargo check --message-format=short 2>&1 | grep -E 'never (used|read|constructed)'
# repeat with --features smp-shared, or the size feature set, to see config-specific dead code
```

Filter out `~/.cargo` paths — third-party crates ship unused API by design and
swamp the first-party signal (the `size` run emits ~40 dep warnings).

To make the cross-kernel rows real rather than recalled: `git clone --depth 1`
the tree, then adapt the classifier's test rules for C (`tools/testing/`,
`lib/test_*.c`, `#ifdef CONFIG_*_KUNIT_TEST` in place of `#[cfg(test)]`).

---

## Background

- [`reference/build-profiles.md`](../reference/build-profiles.md) — the image-size
  half: per-profile bytes, symbol attribution, in-kernel vs userspace SSH.
- `scripts/cloc_akuma.py`, `scripts/symbol_sizes.py` — the two tools, with their
  rules and caveats in their docstrings.
