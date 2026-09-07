# C1 steps 1–2: one syscall vocabulary, one dispatch table

**Date:** 2026-09-07
**Scope:** the first two steps of item **C1** in
`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — deciding how the amd64 kernel and
`akuma-syscalls-glue` agree on what a syscall number *means*, and collapsing
`amd64/src/usermode.rs`'s two dispatch matches into one neutral table plus a
named list of x86-only legacy spellings.
**Status:** landed. No arm has been folded into glue yet; that is step 3.
**Prompt:** `proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`.

## The blocker, restated

`akuma-syscalls-glue::handle_syscall` is a `match syscall_num` over 190
`akuma_syscalls_linux::nr::` constants, and that table is **asm-generic**
(`nr::WRITE = 64`). The amd64 kernel is handed **x86_64** numbers (`write` is
`1`). Handing glue an x86_64 number is not a compile error and not a missing
handler — it is the **wrong** handler. `1` is `write` on x86_64 and lands in
`io_setup`-adjacent territory under asm-generic.

So nothing below C1 is verifiable until the two ends agree on a vocabulary.

## The decision

The prompt offered three shapes. What landed is a fourth — the honest half of
(3) with the cost of (2) confined to one place:

> **Widen `akuma-syscalls-abi::Syscall` and translate at the amd64 boundary.
> `akuma-syscalls-glue` is not touched.**

`Syscall::from_x86_64(nr)` decodes what userspace passed; `to_aarch64()` is what
a folded arm will hand glue. One table, generated four ways, round-trip tested.

### Why not the other two

| shape | why not |
|---|---|
| **`cfg` the `nr` table** (191 `pub const WRITE = if cfg!(x86_64) {1} else {64}`) | Two reasons, and the second is fatal. `akuma-syscalls-linux`'s header says it *is* the aarch64 ABI, and `akuma-syscalls-abi`'s header already argues against a second table living inside it. And **`cfg!(target_arch)` resolves to the *host* under `cargo test`** — on an x86_64 developer machine `nr::WRITE` would silently become `1` and every host test of the AArch64 tables would be testing the other architecture. The mechanism meant to catch a wrong-answer bug would itself be answering wrongly. |
| **Widen the enum *and* make glue dispatch on it** | The most honest shape and still the eventual one, but it rewrites the AArch64 kernel's syscall hot path, which nothing in C1 needs — and it destroys the cheapest proof that the port has not touched the other kernel (a byte-identical `.text`). |

### The cost, pinned rather than hidden

**Inside glue the number is the asm-generic one, even on x86_64.**
`CURRENT_SYSCALL_NR`, glue's `[SYSCALL]` traces and any future `/proc` syscall
field will read `64` where an x86_64 `strace` would say `1`. The amd64 kernel's
own trace (`usermode.rs`, `[sc] … nr=`) keeps the number userspace actually
passed, so the hop is visible in a log rather than silent.

Translating back at every reporting site was the alternative. It buys one
familiar number in a trace and costs a second place for the mapping to be wrong.

## What the table is, and what it deliberately is not

`crates/akuma-syscalls-abi` went **36 variants to 80**. Every row is one line of
a `syscall_table!` macro that generates the enum, `from_x86_64`, `from_aarch64`,
`to_x86_64`, `to_aarch64` and `Syscall::ALL` together:

```rust
Read       => READ       = 0,   nr::READ;
Uname      => UNAME      = 63,  nr::UNAME;
```

The x86_64 side is a literal because this crate owns that table; the aarch64
side is a **path into** `akuma-syscalls-linux`, so the two can never drift.
Both appear in *pattern* position as well as expression position, which makes a
number used twice an `unreachable_patterns` warning rather than a silent wrong
answer. That is the whole reason for the macro: the prompt's worry was "191
chances to transpose two digits", and one table is one chance.

**Rule 2 is what keeps it honest:** a row means the call exists on *both*
architectures with the same meaning. The x86-only legacy spellings — `open`(2),
`stat`(4), `lstat`(6), `poll`(7), `access`(21), `pipe`(22), `select`(23),
`dup2`(33), `fork`(57), `vfork`(58), `rename`(82), `mkdir`(83), `rmdir`(84),
`unlink`(87), `symlink`(88), `readlink`(89), `gettimeofday`(96), `getpgrp`(111),
`arch_prctl`(158), `settimeofday`(164), `time`(201) — have **no** asm-generic
number, and giving one an invented number would be manufacturing a fact about
Linux. They stay as `AT_FDCWD` shims in the amd64 kernel, each narrowing to an
`*at` call the table *does* name.

Two constants were added to `akuma-syscalls-linux::nr` to complete pairs:
`GETSID = 156` and `SYSLOG = 116`. Both are calls the AArch64 kernel does not
dispatch; both are facts about Linux either way.

## What the dispatcher looks like now

`amd64/src/usermode.rs::syscall_dispatch` was **two matches in two styles** — 67
raw x86_64 numbers written as `return`, then 31 typed arms after
`Syscall::from_x86_64` — with no principle separating them. The prompt reads
that as arbitrary; measuring it showed the split was *almost* principled and the
enum was simply too small to hold the rest. (It also corrects the prompt on one
detail: `openat` is not "in both". `2` in the raw match is `open`, the legacy
spelling, and `Syscall::Openat` is `257`. That is a shim, not a duplicate.)

```
   BEFORE                                  AFTER

   nr (x86_64)                             nr (x86_64)
      │                                       │
      ├─ 0x1000+N → Akuma-private (9)         ├─ 0x1000+N → Akuma-private (9)
      │                                       │
      ├─ match nr → 67 RAW arms               ├─ match nr → 21 x86-only LEGACY
      │     └─► amd64 impls                   │     └─► narrows to an *at call
      │                                       │
      └─ Syscall::from_x86_64                 └─ Syscall::from_x86_64
            └─► 31 TYPED arms                       └─► 77 TYPED arms
                                                          │
                                                          └─► steps 3–6:
                                                              glue::handle_syscall(
                                                                s.to_aarch64(), …)
```

The **set of numbers the kernel answers is identical** — 105 before, 105 after,
verified by diffing the two dispatch tables statically rather than by eye.
Every arm body moved verbatim.

## The bug this found

Reading the raw match to classify it turned up one arm that was simply wrong:

```rust
// `utimensat` (280) / `futimens` (88) — timestamp preservation for
// `apk add`'s post-extract pass. NULL times = both set to now.
88 => return crate::fd::sys_utimensat((-100i64) as u64, a1, a2, 0),
```

**x86_64 88 is `symlink(target, linkpath)`.** There is no `futimens` syscall in
Linux at all — libc spells it `utimensat(fd, NULL, times, 0)`. The arm sits
between `87 unlink` and `89 readlink`, both of which *are* correct legacy shims,
so this is a transposition into the middle of a correct run: `ln -s` handed its
link path to `utimensat` as a `struct timespec[2]` pointer.

The failure mode is the one this whole step exists to eliminate. `utimensat` on
those arguments **returns 0**, so `ln -s` reported success and created nothing —
exit 0, no error, no symlink. Fixed as
`88 => sys_symlinkat(a1, AT_FDCWD, a2)` (note `symlinkat` takes the dirfd
*second*).

It is worth naming what class this is. It is the same shape as the
aarch64/x86_64 crossing one layer up — a number decoded against the wrong table
— and it had survived because nothing in the suite ever read a symlink back.

## Verification

**A boot self-test was added** (`usermode::dispatch_smoke_test`, run from
`boot::self_tests` right after `init_syscall`), because both failures here are
silent:

- **No number is in both tables.** All 21 legacy numbers must decode to `None`
  through `Syscall`; one that decodes is handled twice, and which match wins is
  an accident of ordering.
- **The two ABIs must keep disagreeing.** `write` arrives as 1 and reaches glue
  as 64; `63` decodes as `uname` here and is `read` there.
- **A real `symlink` round trip through `syscall_dispatch(88, …)`** — create,
  `readlink` it back, compare, unlink.

That last one was checked against a **negative control**: restoring the old arm
and rebooting gives

```
dispatch: x86_64 88 is symlink(2) and succeeds   [OK]      ← utimensat returns 0
dispatch: the link reads back its target length  [FAIL] got 0xfffffffffffffffe want 0xa
dispatch: and the target itself                  [FAIL]
```

Only the round trip catches it. A test that had stopped at the return value
would have passed against the bug — which is exactly why the bug lasted.

| | before | after |
|---|---:|---:|
| QEMU/TCG `SMP=1` | 405 / 0 | **413 / 0** |
| QEMU/TCG `SMP=4` | 414 / 0 | **422 / 0** |
| Firecracker (the box) `SMP=1` | — | **403 / 0** |
| host tests | 1355 | **1359** |

+8 on every amd64 rig, which is exactly the eight new checks; nothing else
moved. There is **no matched pre-change Firecracker baseline** — the last one
recorded (347/356) predates B3 — so 403/0 is reported as zero failures rather
than as a delta.

**AArch64 proven unchanged**: `akuma` built from this tree and from `HEAD` have
byte-identical `.text`, `.rodata` and `.data`
(`.text` sha256 `054533e6…6c705` both sides). The only shared-crate edit is two
unused `pub const`s in `nr.rs`; `akuma-syscalls-abi` is not a dependency of the
AArch64 kernel.

### One method trap, paid for here

`llvm-objcopy` is **not installed** on this machine — only `rust-objcopy`
(`~/.cargo/bin`). A `--dump-section` invocation that names `llvm-objcopy` and
swallows stderr produces no file at all, and `cmp` against a missing file
reports a *difference*. That turned a byte-identical result into a momentary
"the AArch64 kernel changed" scare. Dump with `rust-objcopy` and `test -s` every
output before comparing; a section compare that cannot fail loudly is not
evidence.

The same run established that the AArch64 kernel build is **reproducible**: two
builds of identical source minutes apart give byte-identical sections, which is
what makes this comparison meaningful in the first place.

Bare metal was **not** run for this step — the box was left on Ubuntu, and the
change has no machine dependency (it is a dispatch table). Worth folding into
the next metal pass rather than spending a reboot cycle on its own.

## What steps 3–6 inherit

- The typed match is now the single list to fold, arm by arm, into
  `akuma_syscalls_glue::handle_syscall(call.to_aarch64(), &args)`.
- The legacy list is **not** folded. Each of its arms narrows to a neutral call
  first; when that neutral arm moves to glue, the shim follows it for free.
- Before the first arm can move, glue's prologue needs what
  `proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md` § "What glue needs" lists:
  the identity cache, the excursion hooks, and an `sc-*` feature selection this
  target has never made.
- The pinned amd64 decisions are unchanged and still need carrying over
  explicitly when their arm moves: `getpid` returns a literal `1`, the uid/gid
  family returns `0`, `setuid`/`setgid` accept unconditionally, `rt_sigaction`
  accepts and does nothing (there is no delivery — that is A2), and `flock`
  accepts and does nothing.

## Background

- `proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md` — the prompt; § "The thing
  the chart does not say" is the blocker this doc resolves.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; C1 is its box,
  and caution 2 ("diff the dispatch arms while folding — the one thing it must
  not be is silent") is what turned up the `88` bug.
- `docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md` — the gate going green, and the
  section-compare method used here to prove AArch64 untouched.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §5 — why
  `akuma-syscalls-abi` exists at all.
