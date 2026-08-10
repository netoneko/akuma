# Line-count statistics and what they might mean (2026-08-07)

An exploration, not a conclusion. `src/` + `crates/` was counted, split into
production vs test code, and then compared against other kernels to see which
readings of the numbers survive contact with context. Several don't.

Companion doc: the profile/image-size half of this investigation — what a
profile's *bytes* cost, which is a different question with a different answer —
lives in [`reference/build-profiles.md`](../reference/build-profiles.md).

**Measured at commit `d3f28d6`, branch `another-smp-attempt-0`.**

> ## Re-measured 2026-08-10 (branch `trim-fat-sshd`) — production code down 13.5%
>
> The body of this doc is the `d3f28d6` snapshot, kept as written. Since then the
> in-kernel SSH server, in-kernel shell, in-kernel editor, `async_fs`, and all
> kernel-side TLS/cryptography were **deleted** (see
> [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md)), and four crates went with
> them (`akuma-ssh`, `akuma-shell`, `akuma-editor` deleted; `akuma-ssh-crypto`
> moved to `userspace/`, out of this doc's scope). Same script, same scope
> (`scripts/cloc_akuma.py src crates`):
>
> | | `d3f28d6` | 2026-08-10 | delta |
> |---|---:|---:|---:|
> | **Production code** | **48,942** | **42,320** | **−6,622 (−13.5%)** |
> | Test code | 26,035 | 25,133 | −902 (−3.5%) |
> | Rust files | 172 | 127 | −45 |
> | All files | 189 | 140 | −49 |
> | Physical lines | 111,120 | 102,362 | −8,758 |
> | comment / code | 31.9% | 35.5% | +3.6 pp |
> | test / production | 0.53x | 0.59x | +0.06 |
>
> Two whole rows of the "production code by area" table went to **zero**:
>
> | area | `d3f28d6` | now |
> |---|---:|---:|
> | Shell (in kernel) | 3,425 | **0** |
> | SSH server (in kernel) | 2,427 | **0** |
> | Editor + terminal | 840 | 264 (`akuma-terminal` only — it survives; it is the PTY/termios layer the *userspace* sshd needs) |
>
> That accounts for ~6.4k of the 6,622-line drop; the remainder is
> `akuma-net`'s TLS client and X.509 verifier (1,027 lines: `tls.rs`,
> `tls_rng.rs`, `tls_verifier.rs`, `http.rs`), offset by growth elsewhere.
>
> **Two readings this changes, and one it does not.**
>
> The "scope limit: first-party only" caveat below gets *less* severe on the
> dependency side: `crypto` (63,580 B) and `tls/x509` (76,596 B) were named there
> as pure dependency code inflating the shipped image without contributing
> lines. Both are now gone from the kernel entirely — 18 crypto crates in the
> dependency tree became **0** — so the gap between "lines we maintain" and
> "code we ship" narrowed on both sides at once, not just the numerator.
>
> The test ratio moving 0.53x → 0.59x is **not** more testing. Production
> shrank 13.5% while test code shrank 3.5%; the ratio rose because the
> denominator fell. `src/process_tests.rs` (10,220 lines) and `src/tests.rs`
> (6,483) are still the two largest files in the tree by a wide margin, together
> 39% of all code. Any conclusion in the body below that leans on the ratio
> should be re-read with that in mind.
>
> What does *not* change: the two biggest production areas are still process/
> threads/MM and the syscall layer. Nothing removed here touched them.


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

### Scope limit: first-party only

Every count below covers `src/` + `crates/` and **nothing else**. Akuma links a
substantial amount of third-party Rust — smoltcp, embedded-tls, curve25519/
ed25519-dalek, sha2, aes, crypto-bigint, virtio-drivers, talc, fdt, arm_pl031,
spinning_top — and none of it appears in the 48,942 figure while all of it ships
in the image.

The byte measurements show how large that gap is. In the `size` image, the
`crypto` (63,580 B), `tls/x509` (76,596 B) and `smoltcp` (58,570 B) groups are
**entirely dependency code** — ~199 KB, over 22% of sized symbols — contributing
zero lines to the count, with more dependency code (talc, virtio-drivers, fdt)
folded into the unattributed remainder. So "49k lines" describes *the code this
project maintains*, not the code it ships. Both are legitimate numbers; they
answer different questions, and only the first one is measured here. See
[Planned: Linked Code Size](#planned-linked-code-size-lcs).

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
| Akuma | 48,942 Rust (first-party) | yes |
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

### The actual peer group

Projects in the same territory — an independent kernel with a POSIX-ish or
Linux-compatible userspace, capable enough to run real third-party software.
Figures below are from public sources (linked at the end of this section);
**only the Akuma row is measured here.**

| Project | Started | Language / shape | Size | Capability high-water mark | Hosts a Rust toolchain? |
|---|---|---|---|---|---|
| **Redox** | 2015 | Rust, microkernel | kernel <30k–50k lines (own docs vary) | `relibc`; Linux-compatible at API *and* syscall-ABI level; COSMIC desktop | **Yes — Jan 2026.** rustc + cargo run natively; can build Rust CLI/TUI programs; first merge request submitted from inside Redox. Third attempt; ~10.5 years from project start |
| **Asterinas** | ~2022 | Rust, framekernel (monolithic address space, safe-Rust services) | **>100K lines Rust, 50+ contributors** | 210+ Linux syscalls (230+ by 0.18); Ext2/exFAT32/overlay, TCP/UDP/Unix; Nginx 1.26.2, Redis 7.0.15, SQLite 3.46.1 at ~Linux parity (Nginx *faster*: 22,912 vs 19,227 rps); TCB 14.0% | **No.** The paper benchmarks server workloads; no compiler runs on it |
| **Sortix** | 2011 | C | — | POSIX; installable on real hardware | Self-hosting **C** toolchain at 1.0 (Mar 2016) — ~5 years |
| **ToaruOS** | Jan 2011 | C, from scratch | — | own libc, compositing GUI, dynamic linker, network stack; replaced all third-party runtime deps in 2018 (1.6) | Not established |
| **Aero** | ~2021 | Rust, monolithic | — | Unix-like, Linux-inspired, SMP, 5-level paging | No evidence found either way |
| **Maestro** | ~2018 | Rust | — | Linux-compatible; own init (Solfège), utils, package manager | No evidence found either way |
| **Akuma** | 2026 | Rust, monolithic | 48,942 first-party lines | 17 syscall families; CoW fork, threads, lazy mmap; ext2, TCP/IP, in-kernel SSH; runs apk, rustc, llama.cpp | **Yes — builds its own kernel.** 147 units, 8m29s, self-built ELF boots (2026-06-19); `release-smp-shared` in-VM build reaches the ELF (2026-08-05); a full build has since completed **in one go** under SMP=4 `-j4` (9m43s, EXIT=0, 108 crates, ELF emitted) — at least 2 clean runs so far |

**What this comparison actually shows:**

**Hosting your own build is close to a two-project club, and the two got there
differently.** Redox's January 2026 milestone was *running* rustc and cargo and
compiling Rust programs — not building the OS itself. Akuma builds its own kernel
and the result boots. On that specific axis Akuma is further along, having reached
it with roughly half the first-party code and one maintainer, where Redox took a
decade, a team, and three attempts.

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
userspace and are excluded from that count, while Akuma's 49k includes smoltcp,
VFS, SSH, and a shell. Comparing the two numbers without that adjustment would
flatter or damn this project arbitrarily — the same trap as Reading A's xv6 row.
And per [Scope limit](#scope-limit-first-party-only), the Akuma figure omits every
linked crate, which is exactly what
[LCS](#planned-linked-code-size-lcs) would fix.

Sources: [Phoronix — rustc/Cargo on Redox](https://www.phoronix.com/news/Redox-OS-January-2026) ·
[heise — Redox compiles code on itself](https://www.heise.de/en/news/Redox-OS-compiles-code-on-itself-for-the-first-time-11173992.html) ·
[The Register (2019) — nearly self-hosting after four years](https://www.theregister.com/2019/11/29/after_four_years_rusty_os_nearly_selfhosting/) ·
[Redox book — microkernels](https://doc.redox-os.org/book/microkernels.html) ·
[Asterinas, USENIX ATC'25](https://arxiv.org/abs/2506.03876) (local copy: `atc25-peng-yuke.pdf`) ·
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

This sharpens Stat 2 rather than contradicting it: 609 of 26,035 test lines
(2.3%) are counted as tests but cannot run. Small, but it is exactly the kind of
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
  than blanket-suppressed. 0.55% dead production code (Stat 5) is that config
  working, not luck. 76 targeted `#[allow(dead_code)]` against 1 crate-level allow
  is the right ratio; most codebases invert it.
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
  76 sites is small enough to audit once.
- **The pre-commit gap is specific: profiles are checked, feature sets are not.**
  `clippy --profile size` runs with default features, so `#[cfg(feature = …)]`
  gating bugs are invisible to it — exactly the shape of the `extreme-size`
  breakage (see `reference/build-profiles.md`). The 4 MB floor is a stated goal
  whose build nothing verifies automatically.
- **Comments assert intent that stopped being true.** `cleanup_box_queues`'
  "Called from sys_kill_box" is false in-source; `src/tests.rs:3` points readers at
  a dead entry point. At 31.9% comment density (Stat 3) comments are load-bearing,
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
  runtime coverage. Some of the 48,942 production lines may never run on any
  boot, and nothing here would show it.
- **Test quality.** Nothing here measures assertion density, or whether the three
  *running* msgqueue tests check anything meaningful. "Wired into the suite" and
  "actually covering the behaviour" are different properties, and only the first
  was measured.
- **Defect density over history.** This would settle Stat 1's "inherently hairy vs
  under-decomposed" question for `exceptions.rs`/`smp.rs`; it needs a git-history
  survey, not a line count.
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
