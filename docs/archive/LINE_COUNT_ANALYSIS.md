# Line-count statistics and what they might mean

**This is a living document, not a historical record.** Unlike most of
`docs/archive/`, the numbers and analysis below are kept current — re-measured
and rewritten in place whenever the tree changes enough to matter, most
recently 2026-08-10. It's used as a real competitive comparison against other
kernels, so stale numbers here aren't "history," they're just wrong. Past
snapshots (the original 2026-08-07 measurement at commit `d3f28d6`, the
2026-08-10 in-kernel-SSH/shell/editor removal, the 2026-08-10 multikernel
removal) live in git history and in
[`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md) /
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) if you need the delta at a
specific commit.

An exploration, not a conclusion. `src/` + `crates/` was counted, split into
production vs test code, and then compared against other kernels to see which
readings of the numbers survive contact with context. Several don't.

Companion doc: the profile/image-size half of this investigation — what a
profile's *bytes* cost, which is a different question with a different answer —
lives in [`reference/build-profiles.md`](../reference/build-profiles.md).

**Measured 2026-08-10, branch `better-sshd-and-networking`, commit `ebfb73f`**
(`scripts/cloc_akuma.py src crates`).

**Cumulative reduction since the original 2026-08-07 measurement (48,942
production lines at commit `d3f28d6`): −10,363 lines of production code
(−21.2%)**, in two cuts on the same day:
1. **In-kernel SSH server, shell, editor, `async_fs`, and all kernel-side
   TLS/cryptography deleted** (48,942 → 42,320; −6,622, −13.5%) — see
   [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md). Four crates went with
   it (`akuma-ssh`, `akuma-shell`, `akuma-editor` deleted; `akuma-ssh-crypto`
   moved to `userspace/`, out of this doc's scope).
2. **The multikernel (one-kernel-per-core, `smp`/`cfg(kernel_smp)`) deleted in
   full** (42,320 → 38,579; −3,741, −8.8%) — `src/smp.rs`, the `akuma-smp`
   crate, `FileDescriptor::RemoteFd`/`RemoteKind`, the
   `prepare_user_address_space`/`remote_fd_close` runtime hooks, the two
   `spawn_process_from_image*` entry points, and every `cfg(kernel_smp)` guard
   in the syscall layer. See
   [`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md) for the full
   accounting and rationale.

Every number in the body below is the **current** measurement (post both
cuts). If you're trying to reconstruct a prior snapshot, use `git log -p` on
this file rather than looking for it inline.

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

Trust check: per-file diff against `cloc 2.08` over all 119 Rust files —
**118 match exactly** on blank/comment/code. The one disagreement is a literal
blank line inside a multi-line string (`src/sync_tests.rs`), which this
counter calls code (it is part of a string token) and cloc calls blank.

### Scope limit: first-party only

Every count below covers `src/` + `crates/` and **nothing else**. Akuma links
third-party Rust — none of it appears in the 38,579 figure while all of it
ships in the image — but the dependency footprint itself is now small enough
to enumerate in full, not just gesture at: **10 unique external crates across
the whole workspace** (`smoltcp`, `virtio-drivers`, `talc`, `fdt`, `arm_pl031`,
`spinning_top`, `embedded-io-async`, `lock_api`, `log`, `elf` — checked
directly against every crate's `Cargo.toml`, 2026-08-11). That's down from a
noticeably larger list before 2026-08-10: the in-kernel TLS/crypto stack
(`embedded-tls`, `curve25519-dalek`, `ed25519-dalek`, `sha2`, `aes`,
`crypto-bigint`, and others) is gone along with the code that used it (see
the SSH-removal cut above), and nothing the multikernel removal touched added
a dependency back. For a Linux-ABI-compatible monolithic kernel, this is an
unusually short list — most of what Akuma needs (allocator, IPC primitives,
scheduling) it implements itself rather than pulling in.

The byte measurements show how large the remaining gap is. In the `size`
image, the `smoltcp` group is entirely dependency code contributing zero lines
to this count while shipping in the image, with more dependency code (talc,
virtio-drivers, fdt) folded into the unattributed remainder. So "38.6k lines"
describes *the code this project maintains*, not the code it ships. Both are
legitimate numbers; they answer different questions, and only the first one is
measured here. See [Planned: Linked Code Size](#planned-linked-code-size-lcs).

---

## The numbers

```
Language                   files     blank   comment      code    % test
Rust                         119     10377     21995     62884     39.2%
Markdown                       5       143         0       281      0.0%
TOML                           7        22        78        94      0.0%
SUM                          131     10542     22073     63259     39.0%
```

| bucket | files | blank | comment | code |
|---|---|---|---|---|
| Production | 116 | 5,643 | 15,410 | **38,579** |
| Tests | 15 | 4,899 | 6,663 | **24,680** |

- comment / code = **34.9%**
- test code / production code = **0.64x**
- 95,874 physical lines

Production code by area (all 38,579 production lines accounted for — unlike
the original 2026-08-07 table, this one folds each crate's `Cargo.toml`/
`README.md` into its own area rather than leaving them unattributed):

| area | prod code | share |
|---|---|---|
| Process / threads / MM | 14,720 | 38.2% |
| Syscall layer | 9,022 | 23.4% |
| CPU / exceptions / SMP | 4,406 | 11.4% |
| Networking | 3,580 | 9.3% |
| Filesystems / VFS | 3,534 | 9.2% |
| Boot / drivers / misc | 3,053 | 7.9% |
| Editor + terminal | 264 | 0.7% |

Grouping: `akuma-exec` + `allocator.rs`/`pmm.rs`/`syscall/mem.rs` +
`akuma-isolation` → process/MM; `src/syscall/` (minus `mem.rs`) → syscall
layer; `exceptions.rs`/`smp_shared.rs`/`irq.rs`/`gic*`/`timer*`/
`kernel_timer.rs` → CPU; `akuma-net`/`akuma-rump`/`rump_proxy.rs` →
networking; `akuma-ext2`/`akuma-vfs`/`src/vfs/` → filesystems. **Shell** and
**SSH server (in kernel)** were their own rows through 2026-08-09 (3,425 and
2,427 lines); both are now **0** and dropped from the table — see
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md). **`akuma-smp`** fed the
CPU row through 2026-08-10; also **0** now — see
[`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md).

---

## Stat 1: 38.6k lines of production code

**Reading A — "that's a lot for one kernel."** True against the teaching-OS
reference points most people carry:

| kernel | ~prod lines | runs a Linux userspace? |
|---|---|---|
| xv6-riscv | ~6–7k C (kernel) | no |
| seL4 (verified core) | ~10k C | no (microkernel; needs a userland OS personality) |
| Akuma | 38,579 Rust (first-party) | yes |
| Linux | ~30M+ | it *is* the reference |

**Reading B — "it's small for what it does," and this is the one that holds.**
The comparison to xv6 is unsound, and an earlier draft of this analysis made it
before catching itself. xv6 has no `mmap`, no threads or `clone`, no signal
delivery, no networking, no dynamic linking, and no real libc; base xv6 `fork`
copies eagerly, with CoW left as a lab exercise. It runs ~21 syscalls and its own
handful of C utilities cross-compiled on the host — it cannot host rustc, or
llama.cpp, or apk, or anything else needing `mmap` + threads + musl.

Those omissions are *precisely* Akuma's two largest areas. Process/threads/MM
(14,720 lines, 38.2%) is CoW fork, `CLONE_VM`, real address spaces, lazy mmap,
demand paging, thread groups, signals. The syscall layer (9,022 lines, 23.4%) is
17 syscall families — `src/syscall/fs.rs` alone is 2,063 lines. Well over half
the kernel is the cost of the Linux ABI, and the ABI is the entire point: it's
why unmodified musl binaries run.

So the honest framing is that **line count tracks ABI surface, not feature
count**, and a fair yardstick has to be a kernel that runs an unmodified Linux
userspace. Against xv6 the number looks bloated; against anything that hosts a
real toolchain it doesn't.

### The actual peer group

Projects in the same territory — an independent kernel with a POSIX-ish or
Linux-compatible userspace, capable enough to run real third-party software.
Figures below are from public sources (linked at the end of this section);
**only the Akuma row is measured here.**

| Project | Started | Language / shape | Size | Capability high-water mark | Hosts a Rust toolchain? |
|---|---|---|---|---|---|
| **Redox** | 2015 | Rust, microkernel | kernel <30k–50k lines (own docs vary) | `relibc`; Linux-compatible at API *and* syscall-ABI level; COSMIC desktop | **Yes — Jan 2026.** rustc + cargo run natively; can build Rust CLI/TUI programs; first merge request submitted from inside Redox. Third attempt; ~10.5 years from project start |
| **Asterinas** | ~2022 | Rust, framekernel (monolithic address space, safe-Rust services) | **>100K lines Rust, 50+ contributors** | 210+ Linux syscalls (230+ by 0.18); Ext2/exFAT32/overlay, TCP/UDP/Unix; Nginx 1.26.2, Redis 7.0.15, SQLite 3.46.1 at ~Linux parity (Nginx *faster*: 22,912 vs 19,227 rps); TCB 14.0%. As of 0.18.0 (2026-06-09), 100+ NixOS packages verified including **Firefox** (needed new kernel support: `ARCH_GET_GS`/`ARCH_SET_GS`) and QEMU | **Not established either way.** The ATC'25 paper says no compiler ran on it then; the 0.18.0 release notes (checked 2026-08-10) confirm Firefox now runs but say nothing about rustc/cargo/gcc — Linux-userspace breadth has grown a lot since the paper, compiler-hosting specifically is unconfirmed |
| **Sortix** | 2011 | C | — | POSIX; installable on real hardware | Self-hosting **C** toolchain at 1.0 (Mar 2016) — ~5 years |
| **ToaruOS** | Jan 2011 | C, from scratch | — | own libc, compositing GUI, dynamic linker, network stack; replaced all third-party runtime deps in 2018 (1.6) | Not established |
| **Aero** | ~2021 | Rust, monolithic | — | Unix-like, Linux-inspired, SMP, 5-level paging | No evidence found either way |
| **Maestro** | ~2018 | Rust | — | Linux-compatible; own init (Solfège), utils, package manager | No evidence found either way |
| **Akuma** | 2026 | Rust, monolithic | 38,579 first-party lines | 17 syscall families; CoW fork, threads, lazy mmap; ext2, TCP/IP, userspace SSH (`/bin/sshd`, in-kernel SSH removed 2026-08-10); runs apk, rustc, llama.cpp | **Yes — builds its own kernel.** 147 units, 8m29s, self-built ELF boots (2026-06-19); `release-smp-shared` in-VM build reaches the ELF (2026-08-05); a full build has since completed **in one go** under SMP=4 `-j4` (9m43s, EXIT=0, 108 crates, ELF emitted) — at least 2 clean runs so far |

**What this comparison actually shows:**

**Hosting your own build is close to a two-project club, and the two got there
differently.** Redox's January 2026 milestone was *running* rustc and cargo and
compiling Rust programs — not building the OS itself. Akuma builds its own kernel
and the result boots. On that specific axis Akuma is further along, having reached
it at 38.6k lines — squarely inside the 30–50k range Redox's own docs cite for
its kernel alone — with one maintainer, where Redox took a decade, a team, and
three attempts.

**Three caveats keep that from being a brag.** (1) Akuma's route is easier in one
concrete way: Linux ABI + musl means *unmodified* rustc binaries, whereas Redox
had to port rustc onto `relibc` and upstream a target triple — different work, not
less. (2) Redox has vastly more breadth — desktop, driver coverage, package
ecosystem, multiple architectures. (3) Reliability is improving but not settled:
earlier full builds needed retry rounds (an intermittent rayon-worker rustc
SIGSEGV) and `-j1` for the final crate; a full SMP=4 `-j4` build has since
completed in one go with no retries (9m43s, EXIT=0, 108 crates, ELF emitted), and
at least one more clean run followed it — still a small sample, not a reliability
claim yet, but "self-hosting" is achieved and getting steadier.

**Asterinas is the sharpest lesson, because it optimized for the opposite thing.**
Twice the code, 50+ contributors, three years — and it beats Linux on Nginx
throughput while not running a compiler at all. Capability is not one axis, and
"lines of code" predicts position on none of them. A project can be larger, faster,
more rigorously verified *and* less self-sufficient simultaneously.

**Size comparisons across kernel architectures are close to meaningless.** Redox's
30–50k is a *microkernel*: drivers, much of POSIX, and the network stack live in
userspace and are excluded from that count, while Akuma's 38.6k includes smoltcp
and VFS (SSH and the shell moved to userspace 2026-08-10, so they no longer
inflate this side of the comparison either). Comparing the two numbers without
adjusting for architecture would still flatter or damn this project arbitrarily —
the same trap as Reading A's xv6 row.
And per [Scope limit](#scope-limit-first-party-only), the Akuma figure omits every
linked crate, which is exactly what
[LCS](#planned-linked-code-size-lcs) would fix.

Sources: [Phoronix — rustc/Cargo on Redox](https://www.phoronix.com/news/Redox-OS-January-2026) ·
[heise — Redox compiles code on itself](https://www.heise.de/en/news/Redox-OS-compiles-code-on-itself-for-the-first-time-11173992.html) ·
[The Register (2019) — nearly self-hosting after four years](https://www.theregister.com/2019/11/29/after_four_years_rusty_os_nearly_selfhosting/) ·
[Redox book — microkernels](https://doc.redox-os.org/book/microkernels.html) ·
[Asterinas, USENIX ATC'25](https://arxiv.org/abs/2506.03876) (local copy: `atc25-peng-yuke.pdf`) ·
[Announcing Asterinas 0.18.0](https://asterinas.github.io/2026/06/04/announcing-asterinas-0.18.0.html) ·
[Phoronix — Asterinas 0.18 released](https://www.phoronix.com/news/Asterinas-0.18) ·
[asterinas/asterinas](https://github.com/asterinas/asterinas) ·
[Sortix](https://sortix.org/) · [ToaruOS at 5 Years](https://toaruos.org/toaruos-at-5-years.html) ·
[Aero](https://github.com/Andy-Python-Programmer/aero) ·
[Maestro](https://github.com/maestro-os/maestro).
Akuma row: `docs/archive/AKUMA_SELF_HOSTING.md` §7j,
`docs/runbooks/selfhost-kernel-build.md`.

### License, funding, and AI-contribution policy

Three more axes the size/capability table doesn't touch, checked directly against
each project's repo (GitHub license API, `CONTRIBUTING`/README text, code search
over each tree) rather than recalled — the AI-policy column marks "not found"
where that search came up empty, which is an absence-of-evidence result, not a
confirmed "no policy."

| Project | License | Funded? | AI-contribution policy |
|---|---|---|---|
| **Redox** | MIT | **Yes.** Colorado 501(c)(4) nonprofit; NLnet/NGI Zero Commons + Core grants under the EU's Next Generation Internet initiative (one, "Virtualized Redox," is €50k for four part-time devs); ~$17k community donations plus a $390k anonymous crypto donation; self-reported ~$3k/month costs against <$1k/month revenue | **Banned, Feb 2026, enforced.** Any contribution "clearly labelled as LLM-generated" is closed immediately; bypassing it is a project ban. Paired with a new Certificate-of-Origin requirement |
| **Asterinas** | MPL-2.0 | **Yes.** Sponsored by Ant Group and Intel; most commits come from PhD students at SUSTech, Peking University, and Fudan University | **Explicitly welcomed, and built into the repo.** Stated policy: *"AI is welcome, but the human is responsible."* Ships `@boterinas codex` (OpenAI-Codex-powered inline PR review) and a `.agents/skills/aster-code-review/` package — a benchmark-driven review skill running under both Claude Code and Codex, used by maintainers and designed as the review step of an autonomous agent write→test→review loop |
| **Sortix** | ISC | **Yes, modestly.** One NLnet NGI0 Commons Fund grant; otherwise a solo project | Not found (`CONTRIBUTING`/README/code search came up empty) |
| **ToaruOS** | NCSA | **No.** Personal project; no sponsors surfaced | Not found |
| **Aero** | GPL-3.0 | **No.** Solo/community project | Not found |
| **Maestro** | AGPL-3.0 | **No.** Solo project | Not found |
| **Akuma** | **BSD-2-Clause** — was undeclared at the time the row above was first measured (no `LICENSE` file, no `license` field in `Cargo.toml`); added 2026-08-07 (`LICENSE`, `Cargo.toml` `license =`) | **No.** One maintainer | No formal policy — a question this project hasn't had to face at Redox/Asterinas's contributor scale |

**What this adds to the peer-group reading.** Funding and code size move together
here: the two projects with institutional money (Asterinas: Ant Group + Intel +
three universities; Redox: an EU-grant-funded nonprofit) are also the two with an
order of magnitude more contributors, and they are the two that had to write an
explicit AI policy at all — Sortix, ToaruOS, Aero, Maestro, and Akuma are all
solo-or-near-solo efforts where the question apparently never came up formally.
And the two funded projects went to **opposite poles**: Redox bans LLM-generated
contributions outright and enforces it with expulsion; Asterinas's policy is the
inverse of that stance, shipping a Codex-backed review bot and framing AI review
as the mechanism that lets human maintainers keep up with AI-accelerated
contribution volume, and floating a fully autonomous review-driven agent loop as
the next step. Same underlying pressure — AI-accelerated contribution throughput
outrunning maintainer review capacity — two structurally opposite answers, both
from funded, multi-institution projects. Akuma sits outside that pressure
entirely: single maintainer, so there's no review bottleneck to legislate around
yet, and this very document is one data point on how it's actually built.

Sources: [Redox donate page](https://www.redox-os.org/donate/) ·
[OSnews — Redox bans code regurgitated by "AI"](https://www.osnews.com/story/144574/redox-bans-code-regurgitated-by-ai/) ·
[HN — Redox's no-LLM + Certificate of Origin policy](https://news.ycombinator.com/item?id=47320661) ·
[Ant Open Source Projects](https://opensource.antgroup.com/en/projects) ·
[asterinas/asterinas: `book/src/to-contribute/boterinas.md`](https://github.com/asterinas/asterinas/blob/main/book/src/to-contribute/boterinas.md),
[`.agents/skills/aster-code-review/`](https://github.com/asterinas/asterinas/tree/main/.agents/skills/aster-code-review),
[`.agents/skills/aster-code-review/spec/motivation.md`](https://github.com/asterinas/asterinas/blob/main/.agents/skills/aster-code-review/spec/motivation.md) ·
[sortix.org — license](https://sortix.org/license/) · [NLnet — Sortix](https://nlnet.nl/project/) ·
GitHub license API for Redox/Asterinas/ToaruOS/Aero/Maestro (`api.github.com/repos/<org>/<repo>/license`).

---

**Reading C — the distribution is the interesting part, not the total.**
`src/exceptions.rs` (3,119 prod lines) and `crates/akuma-exec/src/threading/mod.rs`
(2,549) are the two largest production files, and they are also where most of
this project's SMP/scheduler debugging history lives — the ON_CPU scheduler
race, the ESR-snapshot fix, the BKL dropped-window ledger, the phantom-SVC
guards. (`src/smp.rs`, the former #2 at 4,174 lines, was the one-kernel-per-core
multikernel; it's gone — see
[`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md) — and its debugging
history was almost entirely separate from the exceptions.rs/threading.rs
fixes named here, which are all `smp_shared`/BKL issues.) Two competing
interpretations of why the remaining two files are still this large, both
plausible:

- *Inherent:* exception and scheduler entry paths are irreducibly hairy, and the
  lines are hard-won correctness.
- *Structural:* files that large are where the next bug hides, and the
  concentration of past fixes is evidence of under-decomposition rather than of
  inherent difficulty.

Nothing in a line count distinguishes these. Defect density per file over git
history would.

---

## Stat 2: 0.64x test-to-code — the most misleading number here

24,680 test lines against 38,579 production lines looks like strong discipline.
Three things complicate it.

**It is ~25x Linux's in-tree ratio, which means almost nothing.** Linux's in-tree
tests — `tools/testing/selftests/`, `lib/test_*.c`, KUnit behind
`CONFIG_*_KUNIT_TEST`, scattered driver selftests — are on the order of **~1–2%
of its code (~0.02x)**. But Linux's real coverage isn't in the tree: LTP,
syzkaller, xfstests, KernelCI, the 0-day bot, and distro QA are. Akuma's tests
are all in-tree because **there is no external ecosystem pointed at it** — the
boot suite *is* the harness.

So the in-tree ratio measures *where tests live*, not how much testing exists. A
mature kernel at 0.02x can be better tested than a young one at 0.64x by orders
of magnitude. Comparing the two as quality signals is a category error.

**A different tradition replaces tests entirely.** seL4 has a famously small test
suite and ~20x its code size in Isabelle/HOL proof (~10k lines C, ~200k+ lines of
proof). Under a "verification instead of testing" model the test ratio approaches
zero while confidence goes up. The ratio is not a quality axis at all — it's an
artifact of methodology.

**The distribution undercuts the aggregate.** 18,382 of the 24,680 test lines
(74.5%) are in three files, unchanged by either 2026-08-10 removal:

| file | test code |
|---|---|
| `src/process_tests.rs` | 10,220 |
| `src/tests.rs` | 6,483 |
| `src/sync_tests.rs` | 1,679 |

Meanwhile, by component:

| component | prod code | test code | ratio |
|---|---|---|---|
| `src/syscall` | 9,746 | 0 | — |
| `src/vfs` | 1,107 | 0 | — |
| `crates/akuma-isolation` | 481 | 159 | 0.33x |

(`src/shell` and `src/ssh` were the two worst-tested areas in the original
2026-08-07 measurement — 0.01x each. Both directories are gone entirely as of
2026-08-10; see [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md).)

The syscall layer — the second-largest area of production code, and the surface
every musl binary hits — has **zero** lines classified as tests by this
component boundary (its test coverage runs through `process_tests.rs`/
`tests.rs` instead, which aren't attributed to `src/syscall` by directory).
That's a measurement-boundary artifact, not a claim that the syscall layer is
untested — but it does mean this specific component-ratio view can't
distinguish "well-tested from elsewhere" from "untested," which is itself
worth flagging rather than smoothing over.

---

## Stat 3: 34.9% comment-to-code

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

## Stat 4: the userspace-shaped code got moved out (2026-08-10), and lines
tracked the prediction closely — bytes didn't

The original version of this stat measured 6,692 production lines — shell
(3,425), SSH (2,427), editor + terminal (840) — implementing services other
kernels put in userspace, monolithic by choice: on the 4 MB `extreme-size`
profile these made the box reachable with no disk and no userspace process at
all. It predicted a trap: "move them out and the kernel shrinks by 14%" looks
right on lines but SSH's measured symbol footprint (~34 KB of an ~882 KB
image, ~3.9%) was much smaller than its *line* share (5.0%), while the
userspace `sshd` that would replace it was a 142 KB loadable image before any
runtime cost — so the byte-level trade looked like a wash or a loss, not a win.

**What actually happened:** the shell, SSH server, and editor were deleted
outright the same day this doc was last re-measured — production code fell
48,942 → 42,320 (−13.5%), almost exactly the ~14% the line-share predicted.
So the *lines* prediction held. Only `akuma-terminal` (264 lines, the PTY/
termios layer) survived, because the *userspace* `sshd` still needs it. This
wasn't purely a size decision, though — see
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) for the actual measured
byte deltas and the full rationale (security surface, maintenance cost, not
only image size). Whether the byte-level trade-off in the original caveat
played out as predicted is answered there, not here.

---

## Stat 5: dead code — and two-thirds of it is tests

`dead_code` is **`deny` workspace-wide** (`Cargo.toml` `[workspace.lints.rust]`),
so dead code cannot accumulate unnoticed: everything dead is behind one of
**61 explicit `#[allow(dead_code)]`** sites (down from 76 — the multikernel
removal deleted several, e.g. `src/smp.rs` and the `akuma-smp` crate carried
their own). `RUSTFLAGS="--force-warn dead_code"` overrides those attributes
without editing source (third-party crates filtered out; run in an isolated
`CARGO_TARGET_DIR`).

**Re-measured 2026-08-10 at the item-count level only.** The default-feature
build now shows **72 dead items** (up from 64) — re-checked directly against
the diagnostics rather than recalled, but the specific line-count-per-item
audit below (879 lines, the 1.2%/0.55% figures, the by-area table) was a
manual cross-reference done for the original 2026-08-07 measurement and
**has not been redone at that granularity** for the current tree. Spot-checked
the new items: they're `src/config.rs` constants, `crates/akuma-exec`
threading/process functions, and `akuma-ext2` RAII `hold` fields — the same
*kinds* of finding as the original audit, not multikernel debris (nothing
under `src/smp.rs`/`akuma-smp` shows up, since that whole tree is gone, not
merely unreferenced). Treat the item count as current and the line/percentage
figures immediately below as the last full audit's numbers, not this
session's — a proper re-audit is the honest next step, not a quick rescale.

By area, default features (2026-08-07 audit, at 64 items / 879 lines):

| area | items | lines |
|---|---|---|
| orphaned tests | 17 | **609** |
| `src/syscall` | 11 | 86 |
| `crates/akuma-exec` | 6 | 81 |
| `src/*` (top level) | 26 | 78 |
| `crates/akuma-ext2` | 2 | 16 |
| `src/vfs` | 2 | 9 |

**Reading A — "0.55% dead production code is excellent," and it is.** A
`deny`-by-default lint plus 61 deliberate exemptions is why. There is no rot here
to clean up; the number is a property of the lint configuration more than of the
code.

**Reading B — the interesting 69% (of the last full audit's 879 dead lines)
is test code that never runs**, which the 0.64x ratio in Stat 2 counts as
coverage. Two clusters, different causes (both files are untouched by either
2026-08-10 removal, so these specific findings still hold):

- **`src/tests.rs` — 6 allocator pattern tests (296 lines), silently dropped.**
  Zero call sites anywhere; no comment explains it. The rest of the file *does*
  run (`src/main.rs:1007` `run_memory_tests()`, `:1062` `run_threading_tests()`,
  `:1099` `run_benchmarks()`), and the dead `run_all` aggregate (`src/tests.rs:492`)
  doesn't call these six either — so they are orphaned individually, not by a dead
  entry point.
- **`src/syscall/msgqueue.rs` — 5 waker tests deliberately disabled (277 lines),
  with the reason recorded at `src/process_tests.rs:570-579`**: they drive real
  thread slots into WAITING/READY without valid context, crashing the scheduler
  with `sp=0`; the TODO is to rework them onto mock tids ≥ `MAX_THREADS`. The 9
  msgqueue functions that show up dead alongside them are **external test seams**
  for poller state, dead because those tests are off. The poller layer itself is
  live in production (`sys_msgsnd:190`, `sys_msgrcv:250` register inline;
  `sys_msgctl:88` wakes on RMID) — it is the *wake path's tests* that are missing,
  not the wake path.

Also disabled, with a stated reason: `test_forktest_parent_mmap`
(`src/process_tests.rs:4636`, 52 lines) — "runs for up to 60s" (`:627`).

One of the nine msgqueue functions is **not** a test seam and is a real defect:
`cleanup_box_queues` documents "Called from sys_kill_box" and has no callers. See
[`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) §1.

This sharpens Stat 2 rather than contradicting it: 609 of the current 24,680
test lines (2.5%) are counted as tests but cannot run. Small, but it is exactly the kind of
error the aggregate ratio is blind to — and note that 6 of the 7 disabled tests
carry documented reasons, so the *conduct* here is better than the raw number
suggests. Only the six allocator tests were dropped silently.

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

## Conclusion: what the stats say about engineering discipline

The most useful reading of all five stats together is not about size or coverage.
It is that **the discipline here is real, mechanised where it was mechanised, and
tracks pain almost perfectly** — which is both a compliment and a prediction.

### What the numbers show as genuinely strong

- **Lint configuration is doing real work.** `dead_code = "deny"` workspace-wide,
  clippy `all`/`pedantic`/`nursery` at warn, exemptions enumerated per-lint rather
  than blanket-suppressed. 0.55% dead production code at the last full audit
  (Stat 5) is that config working, not luck. 61 targeted `#[allow(dead_code)]`
  against 1 crate-level allow is the right ratio; most codebases invert it.
- **The pre-commit hook is stricter than typical CI**: clippy `-D warnings` across
  every crate on the host target, then the `release` *and* `size` profiles, then
  host tests.
- **Disabled tests carry root causes.** 6 of the 7 name a reason, and the five
  msgqueue ones name the failure mode (`sp=0` scheduler crash) *and* the fix (mock
  tids ≥ `MAX_THREADS`). The common alternatives — delete it, or leave it red —
  are both worse.
- **Claims are backed by measurement.** The `Cargo.toml` feature blocks carry A/B
  numbers, the configs they were validated at, dates, commit refs, and revert
  instructions. That is why this analysis was possible at all.
- **Testability is architectural**, not bolted on: `crates/` exists so subsystems
  can be host-tested outside QEMU.

### What the numbers show as weak — and it is mostly one mechanism

- **Hand-wired test registration accounts for most of what was found.** 282 manual
  call sites in `process_tests.rs`, 52 in `sync_tests.rs`. A test absent from the
  list is indistinguishable from a test that does not exist, and nothing checks.
  That is one missing mechanism, not 17 mistakes — and it is why the six allocator
  tests vanished silently while everything else got documented. A boot-suite
  assertion that every `fn test_*` in a file appears in some call list would catch
  the whole class.
- **`#[allow(dead_code)]` doubles as a parking space, and that is what cost the one
  real bug.** `cleanup_box_queues` was dead *and annotated as acceptable*: the lint
  was configured correctly, then locally overridden at precisely the site where it
  carried signal. Nothing re-audits the allow list, so the exemption is permanent.
  61 sites is small enough to audit once.
- **The pre-commit gap is specific: profiles are checked, feature sets are not.**
  `clippy --profile size` runs with default features, so `#[cfg(feature = …)]`
  gating bugs are invisible to it — exactly the shape of the `extreme-size`
  breakage (see `reference/build-profiles.md`). The 4 MB floor is a stated goal
  whose build nothing verifies automatically.
- **Comments assert intent that stopped being true.** `cleanup_box_queues`'
  "Called from sys_kill_box" is false in-source; `src/tests.rs:3` points readers at
  a dead entry point. At 34.9% comment density (Stat 3) comments are load-bearing,
  so wrong ones actively mislead rather than merely age.
- **Invariants are enforced per-site rather than derived.** The
  `caller_box != 0 → EPERM` rule appears at 3 sites in `src/syscall/container.rs`
  and is missing at the 2 that create and destroy box identity
  ([`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) §6).

### The pattern that explains both columns

Scheduler, SMP, BKL, fork/exec: measured, A/B'd, documented across 200+ archive
docs, and carrying 9,644 lines of self-tests. Those bugs cost weeks, so they got
instrumentation.

Everything this sweep found sits in the **quiet** areas instead — dead msgqueue
seams, a missing box-teardown call, orphaned allocator tests, inconsistent box
authorization. Stat 2 says the same thing structurally: `src/syscall` carries
9,931 production lines against 117 test lines, and its bugs have been silent so
far.

That is rational triage for one maintainer, not a defect of character. But it is
predictive: the next expensive bug comes from an area with no test pressure, and
per Stat 2 there is no external fuzzer aiming at those areas the way syzkaller
aims at Linux. **The quiet areas are quiet because nothing is listening.**

The cheapest way to change that is not more tests — it is the three mechanisms
above: test-registration checking, an allow-list audit, and feature-set coverage
in pre-commit. Each converts a class of silent failure into a loud one.

---

## What none of these numbers can tell you

- **Whether the code is correct.** The areas with the most lines and the most
  comments are also the areas with the longest bug histories; the correlation is
  real and uninformative as to cause.
- **What actually executes.** Stat 5 measures compile-time reachability, not
  runtime coverage. Some of the 38,579 production lines may never run on any
  boot, and nothing here would show it.
- **Test quality.** Nothing here measures assertion density, or whether the three
  *running* msgqueue tests check anything meaningful. "Wired into the suite" and
  "actually covering the behaviour" are different properties, and only the first
  was measured.
- **Defect density over history.** This would settle Stat 1's "inherently hairy vs
  under-decomposed" question for `exceptions.rs`/`threading/mod.rs`; it needs a
  git-history survey, not a line count.
- **How much would survive a Linux-ABI conformance suite.** 17 syscall families
  exist; how completely each implements its family is unmeasured.
- **Anything about size.** A 30 KB precomputed table is one line of Rust
  (`ED25519_BASEPOINT_TABLE` is exactly that). Lines are a proxy for maintenance
  burden, never for image size. [Linked Code Size](#planned-linked-code-size-lcs)
  is the metric that would close this gap; it is not measured yet.
- **How much code actually ships.** The counts are first-party only, and this
  project links many crates — see [Scope limit](#scope-limit-first-party-only).

---

## Planned: Linked Code Size (LCS)

**To be measured and added to this doc.** Not done yet.

Both metrics in this analysis are flawed in the same direction, and the flaw is
the one that matters most for this codebase: **it links a lot of crates.**

- The **line count** covers only first-party `src/` + `crates/`, so it silently
  omits every dependency — while >22% of the image's sized symbols are dependency
  code (see [Scope limit](#scope-limit-first-party-only)).
- The **symbol attribution** in [`reference/build-profiles.md`](../reference/build-profiles.md)
  does include dependencies, but LTO + `codegen-units = 1` charge inlined code to
  whatever it was inlined into, so per-group totals are floors rather than
  measurements.

The Asterinas ATC'25 paper introduces the metric that fixes both, for exactly the
reason that applies here — quoting it:

> directly comparing lines of code across crates is not ideal, as not all code
> within a crate is necessarily utilized […] we introduce a metric called Linked
> Code Size (LCS), which measures the number of lines of code that are ultimately
> compiled and linked during the OS build. We leverage the LLVM toolchain to
> estimate [it].

That is the right number for this project. A crate contributes its *linked*
lines, not its repository size: pull in `curve25519-dalek` and you are charged for
the code that survives into the binary, not for the whole crate, and not for zero
as today.

**Why it is worth doing here specifically:**

1. It makes the first-party/third-party boundary irrelevant — one number covers
   both, so "how big is this kernel" stops having two incompatible answers.
2. It is per-profile, so it would finally quantify what `--no-default-features`
   buys on `size`/`extreme-size` in code terms rather than in stripped bytes.
3. It sidesteps the LTO attribution problem: linked-ness is a property of the
   build, not of a symbol name that inlining may have erased.
4. It is directly comparable to a published kernel — Asterinas reports LCS
   per-component against Linux 6.12.0 (task scheduler 1.6 vs 27.2 KLoC = 17×,
   slab allocator 1.6 vs 8.7 = 6×, frame allocator 1.2 vs 7.1), which is a far
   better yardstick than the ones used in Stat 1.

**Sketch of how:** build with debug info retained and line tables intact, then map
the linked symbols back to source lines via DWARF (`llvm-dwarfdump
--debug-line`, or `llvm-symbolizer` over the symbols `scripts/symbol_sizes.py`
already enumerates), dedupe by `file:line`, and count distinct source lines
reached. Per-crate rollup comes free from the file paths. The paper's own
estimate is LLVM-based, so this is the same approach rather than an invention.

Until that exists, treat the numbers in this doc as **first-party maintenance
burden**, which is what they honestly are.

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
