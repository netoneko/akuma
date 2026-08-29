# Line-count statistics and what they might mean

**This is a living document, not a historical record.** Unlike most of
`docs/archive/`, the numbers and analysis below are kept current — re-measured
and rewritten in place whenever the tree changes enough to matter. It's used as a
real competitive comparison against other kernels, so stale numbers here aren't
"history," they're just wrong — replace them, don't stack a new dated block on top.
Past snapshots live in git history (`git log -p` this file), and the two 2026-08-10
removals have their own write-ups in
[`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md) /
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) if you need the delta at a
specific commit.

An exploration, not a conclusion. `src/` + `crates/` was counted, split into
production vs test code, and then compared against other kernels to see which
readings of the numbers survive contact with context. Several don't.

Companion doc: the profile/image-size half of this investigation — what a
profile's *bytes* cost, which is a different question with a different answer —
lives in [`reference/build-profiles.md`](../reference/build-profiles.md).

**Current measurement: 2026-08-23.** `scripts/cloc_akuma.py src crates`.

**Cumulative reduction since the first measurement (48,942 production lines):
−5,242 lines of production code**, from two cuts made on 2026-08-10 and
substantially given back by the work since:
1. **In-kernel SSH server, shell, editor, `async_fs`, and all kernel-side
   TLS/cryptography deleted** (−6,622) — see
   [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md). Four crates went with it
   (`akuma-ssh`, `akuma-shell`, `akuma-editor` deleted; `akuma-ssh-crypto` moved
   to `userspace/`, out of this doc's scope).
2. **The multikernel (one-kernel-per-core, `smp`/`cfg(kernel_smp)`) deleted in
   full** (−3,741) — `src/smp.rs`, the `akuma-smp` crate,
   `FileDescriptor::RemoteFd`/`RemoteKind`, the
   `prepare_user_address_space`/`remote_fd_close` runtime hooks, the two
   `spawn_process_from_image*` entry points, and every `cfg(kernel_smp)` guard in
   the syscall layer. See [`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md).

Since those cuts, three crates were extracted into host-testable form and real new
code landed, taking production back up to its current figure — see
[Stat 6](#stat-6-extraction-costs-about-7-and-a-naive-counter-reads-it-as-bloat).

> Every Akuma number here is measured. Every number about *another* kernel is a
> public or recalled figure, marked as such, and should be re-checked before being
> cited anywhere.

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
third-party Rust — none of it appears in the 43,700 figure while all of it
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
virtio-drivers, fdt) folded into the unattributed remainder. So "43.7k lines"
describes *the code this project maintains*, not the code it ships. Both are
legitimate numbers; they answer different questions, and only the first one is
measured here. See [Planned: Linked Code Size](#planned-linked-code-size-lcs).

---

## The numbers

```
Language                   files     blank   comment      code    % test
Rust                         152     11993     32053     72535     40.4%
Markdown                       5       143         0       284      0.0%
TOML                          14        40       147       175      0.0%
SUM                          171     12176     32200     72994     40.1%
```

| bucket | files | blank | comment | code |
|---|---|---|---|---|
| Production | 155 | 6,534 | 23,506 | **43,700** |
| Tests | 16 | 5,642 | 8,694 | **29,302** |

- comment / code = **44.1%**
- test code / production code = **0.67x**
- 117,370 physical lines

> **An 8-line discrepancy in the tool, recorded rather than papered over.**
> `cloc_akuma.py`'s printed summary reports production as **43,692** for the same
> run whose `--json` output sums to **43,700**, and the gap is inside the Rust
> bucket (72,535 printed vs 72,543 in JSON). Every figure in this document uses
> the JSON, because that is what the per-area table below is built from and it has
> to sum. The delta is 0.018% and changes nothing at the precision used anywhere
> here; it is still a bug in the counter and should be fixed.

Production code by area. Every one of the 155 production files is assigned by
explicit rule, an assertion checks that none is left unassigned, and the rows sum to
the measured **43,700**. The eleven areas match
[`BUG_FIX_LIST.md`](BUG_FIX_LIST.md)'s subsystem categories so the two ledgers can
be cross-referenced (see
[Stat 7](#stat-7-bug-density-per-area-and-why-the-grouping-decides-the-answer)):

| area | prod code | share | vs 2026-08-17 |
|---|---:|---:|---:|
| Scheduler & Process | 10,997 | 25.2% | +978 |
| Syscall / ABI | 6,259 | 14.3% | +326 |
| Networking | 5,815 | 13.3% | +1,299 |
| Memory & VM | 4,845 | 11.1% | +136 |
| VFS & Filesystem | 4,008 | 9.2% | +2 |
| Boot & Drivers | 3,821 | 8.7% | +835 |
| Signals & Exceptions | 3,418 | 7.8% | +101 |
| SMP & Locking | 2,081 | 4.8% | −18 |
| Containers | 1,118 | 2.6% | +4 |
| Console & Terminal | 963 | 2.2% | +52 |
| Misc / cross-cutting | 375 | 0.9% | +100 |
| **total** | **43,700** | **100%** | **+3,815** |

**Where the +3,815 went** is the six days of work between the two measurements, and
it is concentrated in exactly two places. **Networking +1,299** is the NIC-path
audit ([`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md)) — the virtio-net interrupt
path, the `nicstat` profiler, the loopback ring conversion and the socket-table
work. **Boot & Drivers +835** is the Firecracker port: the new
`crates/akuma-firecracker` (224) plus `src/platform.rs`'s FDT-derived device map.
The remaining areas moved by maintenance-sized amounts, and SMP & Locking is the
only one that shrank.

Grouping rules, and the judgment calls they embed:

- **Memory & VM** — `akuma-pmm`, `allocator.rs`, `pmm.rs`, `file_page_cache.rs`,
  `akuma-exec/src/mmu/`, `syscall/mem.rs`.
- **Scheduler & Process** — `akuma-exec` `threading/` + `process/` + `elf/` and the
  crate root, plus `syscall/proc.rs` and (new 2026-08-23) `akuma-scheduler`, the
  host-only placement/wake-policy model. The largest area, and the one the old
  grouping merged with memory into a single 38% row.
- **Syscall / ABI** — `src/syscall/` minus the files claimed by another area
  (`mem.rs`, `proc.rs`, `net.rs`, `term.rs`, `container.rs`, `signal.rs`).
- **SMP & Locking** — `smp_shared.rs`, `bkl_profile.rs`, `akuma-exec` `sync.rs` /
  `bkl.rs` / `bkl_model.rs` / `bkl_guard.rs`, `akuma-net/src/locks.rs`, and the
  `akuma-primitives` preempt/irq/once/toggled-guard leaves.
- **Signals & Exceptions** — `exceptions.rs`, `syscall/signal.rs`,
  `threading/sigframe.rs`, `process/signal.rs`. **This is the mapping's most
  consequential call**: `exceptions.rs` alone is 2,767 lines, and the SMP/BKL
  campaigns were *investigated* as concurrency work while *landing* as edits
  inside it. Filing it here rather than under SMP & Locking swings that area's
  bug density by a factor of ~3.9 (10.5 against 41.3) — see Stat 7.
- **Networking** — `akuma-net`, `akuma-rump`, `rump_proxy.rs`, `syscall/net.rs`,
  and `nic_profile.rs` (the `[NICSTAT]` window printer, added by the NIC audit —
  filed here rather than under Boot & Drivers because it measures the stack, not
  the device).
- **VFS & Filesystem** — `akuma-ext2`, `akuma-vfs`, `src/vfs/`, **`src/fs.rs`**,
  `primitives/inode_pin.rs`. Earlier versions of this list wrote the fourth entry
  as a bare `fs.rs`, which is ambiguous: `src/syscall/fs.rs` is 2,237 lines and
  stays in **Syscall / ABI**, where the exclusion list above places it and where
  Stat 1 counts it. Reading the bare `fs.rs` the other way moves 2,237 lines and
  swings both areas by a third, so the disambiguation is load-bearing.
- **Containers** — `akuma-isolation`, `akuma-exec/src/box_mod/`,
  `syscall/container.rs`. Its own row now; the old grouping folded it into
  process/MM.
- **Console & Terminal** — `akuma-terminal`, `console.rs`, `syscall/term.rs`,
  `primitives/console.rs`, and `klog.rs` (the `log`-crate sink that routes into the
  console; added 2026-08-21 so smoltcp's own `log::info!` output stops going
  nowhere). Formerly "Editor + terminal".
- **Boot & Drivers** — `main.rs`, `boot.rs`, `gic*`, `timer*`, `irq.rs`,
  `ramfb.rs`, `fw_cfg.rs`, **`platform.rs`**, `akuma-virtio`, `akuma-timer`,
  **`akuma-firecracker`** (new — the FDT parser whose device map moves the GIC
  redistributor at run time), and the primitives mmio/clock/addr leaves.
- **Misc / cross-cutting** — `config.rs`, `akuma-kacho` (the shared
  observe/decide/hysteresis layer every self-tuning policy sits on) and the
  remaining `akuma-primitives` scaffolding.

**Shell** and **SSH server (in kernel)** were their own rows through 2026-08-09
(3,425 and 2,427 lines); both are **0** — see
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md). **`akuma-smp`** fed the CPU
row through 2026-08-10; also **0** — see
[`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md).

Reproduce with `scripts/cloc_akuma.py src crates --json` and the assignment rules
above; the exact rule table used for this measurement is recorded in
[`600_BUGS_ANNIVERSARY.md`](600_BUGS_ANNIVERSARY.md) § "The central finding".

---

## Stat 1: 43.7k lines of production code

**Reading A — "that's a lot for one kernel."** True against the teaching-OS
reference points most people carry:

| kernel | ~prod lines | runs a Linux userspace? |
|---|---|---|
| xv6-riscv | ~6–7k C (kernel) | no |
| seL4 (verified core) | ~10k C | no (microkernel; needs a userland OS personality) |
| Akuma | 43,700 Rust (first-party) | yes |
| Linux | ~30M+ | it *is* the reference |

**Reading B — "it's small for what it does," and this is the one that holds.**
The comparison to xv6 is unsound, and an earlier draft of this analysis made it
before catching itself. xv6 has no `mmap`, no threads or `clone`, no signal
delivery, no networking, no dynamic linking, and no real libc; base xv6 `fork`
copies eagerly, with CoW left as a lab exercise. It runs ~21 syscalls and its own
handful of C utilities cross-compiled on the host — it cannot host rustc, or
llama.cpp, or apk, or anything else needing `mmap` + threads + musl.

Those omissions are *precisely* Akuma's two largest areas. Process/threads/MM
(15,842 lines, 36.3%) is CoW fork, `CLONE_VM`, real address spaces, lazy mmap,
demand paging, thread groups, signals. The syscall layer (10,410 lines, 23.8%) is
20 syscall families — `src/syscall/fs.rs` alone is 2,237 lines. Well over half
the kernel is the cost of the Linux ABI, and the ABI is the entire point: it's
why unmodified musl binaries run.

**The workloads are the sharpest evidence for this.** As of 2026-08-17 the kernel
runs, unmodified: **Redis** (the Alpine package *and* the official `redis:alpine`
image pulled from Docker Hub, in a box, reachable from the host —
[`REDIS_END_TO_END.md`](REDIS_END_TO_END.md)), **Go** (`go build` compiles and links
on-target: Go 1.26.3 from apk, cold module build 112 s over 38 stdlib packages, warm
14 s, binary runs — [`GOLANG_MISSING_SYSCALLS.md`](GOLANG_MISSING_SYSCALLS.md)
§ Milestone Status), and **Rust** (two on-target toolchains; the nightly one builds
this kernel in-VM and the result boots —
[`AKUMA_SELF_HOSTING.md`](AKUMA_SELF_HOSTING.md)).

None of those needed a "support" component. Redis needed four corrections to
existing behaviour — `connect(2)` classifying TCP state before dialing,
`sys_writev` stopping at a short write, `/proc/self/` chasing its own symlink,
`waitid` checking parentage — because the expensive part was already built and is
already counted here. A feature-count reading of this codebase would predict a
"Redis support" component; there isn't one, and that is a claim about *what kind of
code* the line count is made of.

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
| **Asterinas** | ~2022 | Rust, framekernel (monolithic address space, safe-Rust services) | **>100K lines Rust, 50+ contributors** | 210+ Linux syscalls (230+ by 0.18); Ext2/exFAT32/overlay, TCP/UDP/Unix; Nginx 1.26.2, Redis 7.0.15, SQLite 3.46.1 at ~Linux parity (Nginx *faster*: 22,912 vs 19,227 rps). **Entirely safe Rust at the service layer** — all `unsafe` is confined to one library (OSTD, ~15k lines = TCB 14.0%, comparable in size to seL4's verified core), which is the only part they formally verify (Verus, with CertiK) and only for memory safety; concurrency and logic bugs are chased with model checking (Converos) and tests instead, with logic-level verification stated as aspirational. As of 0.18.0 (2026-06-09), 100+ NixOS packages verified including **Firefox** (needed new kernel support: `ARCH_GET_GS`/`ARCH_SET_GS`) and QEMU. **Deployed in production at Alibaba Cloud** — stated by the presenters in the USENIX ATC'25 talk, recorded 2026-08-23 from recall of the video; no timestamp or written citation yet, so re-verify before citing externally | **Undocumented, not unlikely.** The ATC'25 paper says no compiler ran on it then; the 0.18.0 release notes (checked 2026-08-10) confirm Firefox and QEMU run but say nothing about rustc/cargo/gcc. Given 210+ syscalls and that userspace breadth, a compiler working is near-certain — treat this cell as "nobody has published it", not as evidence against. The distinct claim Akuma makes is *self-build*: the kernel compiled under itself, and the result boots |
| **Sortix** | 2011 | C | — | POSIX; installable on real hardware | Self-hosting **C** toolchain at 1.0 (Mar 2016) — ~5 years |
| **ToaruOS** | Jan 2011 | C, from scratch | — | own libc, compositing GUI, dynamic linker, network stack; replaced all third-party runtime deps in 2018 (1.6) | Not established |
| **Aero** | ~2021 | Rust, monolithic | — | Unix-like, Linux-inspired, SMP, 5-level paging | No evidence found either way |
| **Maestro** | ~2018 | Rust | — | Linux-compatible; own init (Solfège), utils, package manager | No evidence found either way |
| **Akuma** | 2026 | Rust, monolithic | 43,700 first-party lines | 20 syscall families, ~170 dispatched syscall numbers; CoW fork, threads, lazy mmap; ext2, TCP/IP, userspace SSH (`/bin/sshd`, in-kernel SSH removed 2026-08-10); runs apk, rustc, llama.cpp, **nginx** (stock apk `nginx-1.30.4-r1`, benchmarked against the same binary in Docker — [`NGINX_MISSING_SYSCALLS.md`](NGINX_MISSING_SYSCALLS.md)), **Redis** (Alpine package *and* the official `redis:alpine` Docker image in a box, host-reachable), **Go** (`go build` on-target, plus Go binaries under SMP), and **Rust** programs with tokio/hyper/reqwest/rustls. **Boots on real server hardware**: `m6g.metal` (Graviton2, 64 cores) under Firecracker v1.16.1 / KVM in VHE mode, 292 boot tests passed at 1 vCPU and 302 at 2, SSH from a remote workstation, ~15 MB/s inbound HTTP — [`AKUMA_FIRECRACKER_TERRAFORM.md`](AKUMA_FIRECRACKER_TERRAFORM.md) §10 | **Yes — builds its own kernel.** 147 units, 8m29s, self-built ELF boots (2026-06-19); `release-smp-shared` in-VM build reaches the ELF (2026-08-05); a full build has since completed **in one go** under SMP=4 `-j4` (9m43s, EXIT=0, 108 crates, ELF emitted); the build is now **reliable**, not retry-dependent |

**What this comparison actually shows:**

**Hosting your own build is close to a two-project club, and the two got there
differently.** Redox's January 2026 milestone was *running* rustc and cargo and
compiling Rust programs — not building the OS itself. Akuma builds its own kernel
and the result boots. On that specific axis Akuma is further along, having reached
it at 43.7k lines — just past the 30–50k range Redox's own docs cite for
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

**The Redis row is a direct overlap, and the nginx row is now one too.** Asterinas
runs Redis 7.0.15 at roughly Linux parity and nginx 1.26.2 *faster* than Linux, and
publishes throughput for both; Akuma runs Redis including the official
`redis:alpine` image pulled from Docker Hub (2026-08-16), and as of 2026-08-20 runs
stock apk nginx benchmarked against **the same nginx binary** in Docker
([`NGINX_MISSING_SYSCALLS.md`](NGINX_MISSING_SYSCALLS.md)). That is the same
workload on a kernel with 43.7k first-party lines and one maintainer against >100k
lines and 50+ contributors.

**What that measurement says, and the part that is still missing.** On the TCP
handshake the two kernels are a dead heat (p50 130.5 us vs Docker's 132.3, 500
samples, 0 errors). On a full HTTP round trip Akuma's median is 1.6× Docker's
(732 vs 461 us) and its p99 is 9× (5,880 vs 639 us) — a tail this repo has already
attributed to the 3 ms scheduler tick. So the honest form of the claim is no longer
"runs it" against Asterinas' "runs it at parity"; it is that **Akuma is at parity on
connection setup and behind on tail latency, measured**. Redis still has no clean
cross-kernel rps figure ([`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md)
is partly superseded and its host was contaminated), so that row's comparison is
still open. And reading any of this as a size-efficiency win would be exactly the
error Reading A makes with xv6, one table row further along.

**Asterinas is the sharpest lesson, because it optimized for the opposite thing.**
Twice the code, 50+ contributors, three years — and it beats Linux on Nginx
throughput while not running a compiler at all. Capability is not one axis, and
"lines of code" predicts position on none of them. A project can be larger, faster,
more rigorously verified *and* less self-sufficient simultaneously.

**Keeping this comparison up to date is worth the effort on its own.** Measuring
against Linux and against Asterinas is good practice: a reference that is
unambiguously better is what turns "this feels slow" into a specific thing to go
and fix, and it is where a lot of this project's direction has come from. Asterinas
published nginx throughput against Linux, which is a large part of why nginx got
run here at all. Treat the table as a source of inspiration and of next targets,
not as a scoreboard to settle.

**Size comparisons across kernel architectures are close to meaningless.** Redox's
30–50k is a *microkernel*: drivers, much of POSIX, and the network stack live in
userspace and are excluded from that count, while Akuma's 43.7k includes smoltcp
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
the safe-Rust / 14%-TCB / verification-scope claims are stated in the ATC'25 abstract, the
Asterinas blog and LWN's coverage (checked 2026-08-17) ·
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

## Stat 2: 0.67x test-to-code — the most misleading number here

29,302 test lines against 43,700 production lines looks like strong discipline.
Three things complicate it.

**It is ~25x Linux's in-tree ratio, which means almost nothing.** Linux's in-tree
tests — `tools/testing/selftests/`, `lib/test_*.c`, KUnit behind
`CONFIG_*_KUNIT_TEST`, scattered driver selftests — are on the order of **~1–2%
of its code (~0.02x)**. But Linux's real coverage isn't in the tree: LTP,
syzkaller, xfstests, KernelCI, the 0-day bot, and distro QA are. Akuma's tests
are all in-tree because **there is no external ecosystem pointed at it** — the
boot suite *is* the harness.

So the in-tree ratio measures *where tests live*, not how much testing exists. A
mature kernel at 0.02x can be better tested than a young one at 0.67x by orders
of magnitude. Comparing the two as quality signals is a category error.

**A different tradition replaces tests entirely.** seL4 has a famously small test
suite and ~20x its code size in Isabelle/HOL proof (~10k lines C, ~200k+ lines of
proof). Under a "verification instead of testing" model the test ratio approaches
zero while confidence goes up. The ratio is not a quality axis at all — it's an
artifact of methodology.

**The distribution undercuts the aggregate.** 18,923 of the 29,302 test lines
(68.0%) are in three files:

| file | test code |
|---|---|
| `src/process_tests.rs` | 10,972 |
| `src/tests.rs` | 6,274 |
| `src/sync_tests.rs` | 1,677 |

Meanwhile, by component:

| component | prod code | test code | ratio |
|---|---|---|---|
| `src/syscall` | 10,012 | 0 | — |
| `src/vfs` | 1,124 | 0 | — |
| `crates/akuma-isolation` | 717 | 551 | 0.77x |

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

## Stat 3: 47.7% comment-to-code — and the extraction programme is why it moved

High for a systems codebase; `~15–20%` is the usual range quoted for Linux.

**Re-measured 2026-08-29** (`scripts/cloc_akuma.py src crates`). Only this stat was
re-measured on that date; everything else in this document still carries the
2026-08-23 figures.

| point | comment / code |
|---|---|
| `v0.0.7` (2026-06-19) | **21.2%** |
| 2026-08-23 | 44.1% |
| 18c60d1a (2026-08-29, before `akuma-mmap`) | 47.6% |
| after `akuma-mmap` | 47.7% |

**The `v0.0.7` row is the one that reframes this stat.** Ten weeks ago the ratio
was 21.2% — inside the `15–20%` band this section quotes for Linux, not above it.
So "high for a systems codebase" describes a *recent* state, not a standing
characteristic of the codebase. Over those ten weeks code grew 48%
(57,775 → 85,472) while comments grew **233%** (12,259 → 40,809).

The +3.6 is **not** a drift in how existing code is commented — it is the
extraction programme showing up in the aggregate. Extracted leaves document their
own seam and run far above the tree mean, so every extraction moves lines from a
45%-commented bin crate into a 67–89%-commented leaf:

| component | code | comment | ratio |
|---|---|---|---|
| `src` | 30,887 | 14,022 | 45.4% |
| `crates/akuma-exec` | 16,083 | 9,900 | 61.6% |
| `crates/akuma-syscalls-poll` | 740 | 657 | **88.8%** |
| `crates/akuma-syscalls-sync` | 741 | 517 | 69.8% |
| `crates/akuma-mmap` | 398 | 266 | 66.8% |

Three syscall-family extractions landed between the last two measurements
(`akuma-syscalls-time`, `-sync`, `-poll`); `akuma-mmap` accounts for only +0.1 of
the +3.6. See [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §6.

### The same window, in `unsafe`

`scripts/cloc_akuma.py` gained `unsafe`-site counting on 2026-08-29, and it runs
under `--rev`, so the same window can be measured rather than asserted:

| | `v0.0.7` | `v0.0.7-akuma-on-aws` | 2026-08-29 |
|---|---:|---:|---:|
| date | 2026-06-19 | 2026-08-21 | 2026-08-29 |
| crates | 10 | 14 | 22 |
| code (`src` + `crates`) | 57,775 | 72,865 | 85,472 |
| comment / code | 21.2% | 44.1% | 47.7% |
| `unsafe` sites, tree-wide | **777** | 656 | **680** |
| … in `src/` | 536 | 308 | 313 |
| … in `crates/` | 241 | 348 | 367 |
| `unsafe` per kloc | 13.4 | 9.0 | **8.0** |
| crates with `#![forbid(unsafe_code)]` | **0 of 10** | **0 of 14** | 13 of 22 |
| code in those crates | 0 | 0 | 9,623 |

`v0.0.7-akuma-on-aws` is the tag this document's own 2026-08-23 measurement was
taken at — the 44.1% row and that tag agree exactly, which is a useful check on
both.

Two windows, two different stories, and the second is the honest one to quote for
recent work:

- **`v0.0.7` → now (10 weeks).** The tree grew 48% and `unsafe` fell 12.5% in
  absolute terms. `src/` shed 42% of its sites.
- **`akuma-on-aws` → now (8 days).** `unsafe` *rose* 656 → 680. Half of the +24 is
  one crate: `akuma-syscalls-linux`'s 12 `transmute` layout assertions, which are
  `unsafe` that buys safety — they pin `repr(C)` ABI structs against Linux headers
  at compile time. The rest is `akuma-exec` +6 and `src` +5 against ~12,600 lines
  of growth, and `akuma-isolation` −1 (its last site, removed deliberately).
  Density still fell, 9.0 → 8.0 per kloc.

The `forbid` row is the one to notice: **the entire enforcement discipline is
eight days old.** At `akuma-on-aws` not one of the fourteen crates banned `unsafe`;
thirteen of twenty-two do now. That is not a slow trend, it is a sweep — which
means it has not yet been tested by much subsequent change.

Over the full ten weeks the codebase grew by half and shed an eighth of its
`unsafe` in absolute terms. The `src/` and `crates/` rows show the mechanism:
extraction does not delete `unsafe`, it *relocates* it — out of the bin crate, into a crate where it is
either irreducible and documented as such, or absent and enforced absent. The
`forbid` discipline did not exist at all at `v0.0.7`.

Read against Stat 3's Reading B (comment density as a complexity tell), these two
tables are the counter-evidence: the prose grew alongside a measured *fall* in the
construct that most needs prose to be safe.

**Reading A — a debugging-history artifact.** Much of Akuma's commentary records
*why* an invariant exists, often citing the archived investigation that
established it (`Cargo.toml`'s feature block is an extreme case: several hundred
words per feature, with measured A/B results inline). That is unusually valuable
and unusually verbose.

**Reading B — a complexity tell.** Code needing that much explanation may be code
whose invariants aren't expressible in its structure. The BKL carve-out comments
are the test case: they exist because "which lock protects this, under which
feature combination" cannot currently be read off the types.

Both readings are consistent with the same measurement, and the per-crate table
above sharpens the second one. `akuma-syscalls-poll` at 88.8% is the case to watch:
a seam needing 657 lines of prose to say what must not cross it is a seam a reader
cannot infer from the types. `akuma-mmap` at 66.8% is the mild case — its central
claim ("this crate cannot lock or allocate") is expressed structurally, in an empty
`[dependencies]` table, and the prose only points at it.

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

**Item counts are current; the per-item line audit is not.** The default-feature
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
is test code that never runs**, which the 0.67x ratio in Stat 2 counts as
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

This sharpens Stat 2 rather than contradicting it: 609 of the current 29,302
test lines (2.1%) are counted as tests but cannot run. Small, but it is exactly the kind of
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

## Stat 6: extraction costs about 7%, and a naive counter reads it as bloat

Three crates — `akuma-pmm`, `akuma-virtio`, `akuma-primitives` — were extracted out
of `src`/`akuma-exec` into host-testable form. Production code went **up** across a
period of work called "trim the fat", and that is not a contradiction:

| | prod code |
|---|---:|
| the three new crates | **+2,410** |
| the components they came out of (`src` −1,670, `akuma-exec` −467, `akuma-net` −105, `akuma-rump` −10) | **−2,252** |
| genuinely new code in existing components | **+284** |
| **net** | **+442** |

So **extraction cost 158 lines across 2,252 moved** — about **7%**, which is the
`Cargo.toml` / `lib.rs` / public API / runtime-injection glue a standalone `no_std`
crate needs, plus the forwarding shim left behind (`src/pmm.rs` is 153 lines where
it was 898). The other +284 was never a refactor.

What the round actually bought: **test code +1,950 and comments +2,990** against
production +442. Extracting into host-testable crates is what those buy, and it is
what the work was for.

**Beware the filename-based prod/test split — it gets this backwards.** Counting
production as "everything not in `*_tests.rs`/`tests/`" reports the `akuma-pmm`
extraction as **+501 production lines**; attributing its inline
`#[cfg(test)] mod tests` correctly gives **+221**. Extraction into a host-testable
crate is exactly the move that relocates test code *into* a production-named file,
so a naive counter reads a test-coverage win as production bloat.
[`scripts/cloc_akuma.py`](../../scripts/cloc_akuma.py) has always handled this — it
evaluates `cfg` predicates rather than trusting filenames — and grew `--rev`/`--vs`
so the before/after comparison can be made without checking out two trees.

---

## Stat 7: bug density per area, and why the grouping decides the answer

**New 2026-08-17, re-measured 2026-08-23.** The re-derived split above makes
something possible that no previous version of this doc could do: cross-reference
the line ledger against [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md)'s **680 documented
fixes**. The eleven areas were chosen to match that doc's own subsystem categories
precisely so this join would be legitimate.

Bug set is the **580 kernel-attributable** fixes; the excluded 100 are Userspace
Apps (37), Toolchain & Self-hosting (37) and SSH (26), none of which has a kernel
line-area to map onto. `BUG_FIX_LIST.md`'s Rump row (26) is folded into Networking
here, matching the line rule above. `index` = bug share ÷ line share; **1.00 means
an area carries exactly the share of bugs its size predicts.**

| area | prod lines | line % | bugs | bug % | bugs/kLoC | index |
|---|---:|---:|---:|---:|---:|---:|
| Misc / cross-cutting | 375 | 0.9% | 22 | 3.8% | 58.7 | 4.42 |
| SMP & Locking | 2,081 | 4.8% | 86 | 14.8% | 41.3 | 3.11 |
| Memory & VM | 4,845 | 11.1% | 113 | 19.5% | 23.3 | 1.76 |
| Syscall / ABI | 6,259 | 14.3% | 128 | 22.1% | 20.5 | 1.54 |
| Containers | 1,118 | 2.6% | 19 | 3.3% | 17.0 | 1.28 |
| Console & Terminal | 963 | 2.2% | 15 | 2.6% | 15.6 | 1.17 |
| Networking | 5,815 | 13.3% | 69 | 11.9% | 11.9 | 0.89 |
| Scheduler & Process | 10,997 | 25.2% | 76 | 13.1% | 6.9 | 0.52 |
| Boot & Drivers | 3,821 | 8.7% | 23 | 4.0% | 6.0 | 0.45 |
| VFS & Filesystem | 4,008 | 9.2% | 17 | 2.9% | 4.2 | 0.32 |
| Signals & Exceptions | 3,418 | 7.8% | 12 | 2.1% | 3.5 | 0.26 |
| **total** | **43,700** | 100% | **580** | 100% | **13.3** | — |

**Six days changed the ordering in two places and nothing about the conclusions.**
Networking took 13 new documented fixes against +1,299 lines during the NIC audit
and its density still *fell* (12.4 → 11.9). Boot & Drivers took 12 during the
Firecracker port and rose from 3.7 to 6.0, moving it off the floor it shared with
VFS. The top four rows and the bottom rows are unchanged in both membership and
order, which is the useful result: a week of concentrated work in two areas did not
disturb the ranking.

### Reading A — "size predicts risk." It doesn't

The largest area in the codebase, **Scheduler & Process at 25.2% of all production
lines, carries 13.1% of the bugs** (index 0.52). The two areas that carry the most —
Syscall/ABI and Memory & VM — are 25.4% of the code and 41.6% of the bugs between
them. Size predicts *maintenance burden*, which is what this whole document
measures; it does not predict where the failures came from.

### Reading B — "so refactor by density." Only for the rows that survive re-cutting

Four assignments in the mapping are genuinely arguable. Each was flipped to its
alternative and the table recomputed (`syscall/sync.rs` (futex) → SMP;
`exceptions.rs` → SMP; `threading/mod.rs` → SMP; `syscall/poll.rs` → Scheduler),
giving six groupings in total:

| area | bugs/kLoC range | index range | verdict |
|---|---|---|---|
| Memory & VM | 23.3 – 23.3 | 1.76 | **robust — high** |
| Syscall / ABI | 20.5 – 27.4 | 1.54 – 2.06 | **robust — high** |
| Scheduler & Process | 6.4 – 9.0 | 0.48 – 0.68 | **robust — low** |
| VFS & Filesystem | 4.2 – 4.2 | 0.32 | **robust — low** |
| Boot & Drivers | 6.0 – 6.0 | 0.45 | **robust — low** |
| SMP & Locking | **10.5 – 41.3** | **0.79 – 3.11** | fragile — crosses 1.00 |
| Signals & Exceptions | **3.5 – 21.6** | **0.26 – 1.63** | fragile — crosses 1.00 |

*Ranges re-derived 2026-08-23 from the same four flips against the new line and bug
counts.*

**What survives:** memory management and the syscall/ABI layer still cost far more
per line than the filesystem and driver code, under every grouping tested — but the
multiple has come down. It was ~6× on 2026-08-17 (23.8 and 21.4 against 3.7 and
3.7); it is now **~4×** (23.3 and 20.5 against 4.2 and 6.0), because the Firecracker
port put 12 documented fixes into Boot & Drivers and lifted that denominator off the
floor. The *direction* is what has held across two measurements; the multiple is
drifting and should not be quoted without its date.

**What does not:** anything about concurrency. `src/exceptions.rs` is 2,862 lines,
and *where that one file is filed* swings SMP & Locking between 10.5 and 41.3
bugs/kLoC — because `BUG_FIX_LIST.md` tags a fix by the dominant subsystem of its
*investigation*, while lines are counted by *file location*, and the SMP/BKL
campaigns were investigated as concurrency work but landed as edits inside
`exceptions.rs` and `threading/mod.rs`. Grouped as one super-area
(scheduler + SMP + exceptions), concurrency comes out at **10.5 bugs/kLoC, index
0.79 — below average.**

**This retracts an intermediate finding.** A draft of this cross-reference used the
old seven-area grouping and concluded concurrency was *the* high-density area, at
roughly 5× the filesystem code, with `exceptions.rs` and `threading/mod.rs` named
as the refactoring queue. That was an artifact: the coarse grouping put
concurrency's *bugs* into a 4,406-line bucket while the files those fixes actually
landed in were counted under process/MM. Same two ledgers, two defensible
groupings, opposite verdict on the most consequential question.

### Limits that hold under any grouping

- **Bug counts measure *found and fixed*, not *present*.** Every density figure is
  as much a measure of attention as of defect. VFS at 4.2 is either genuinely
  simpler or simply less examined — `src/vfs` has **0** test lines by directory
  attribution — and this join cannot distinguish those.
- **Per-doc subsystem tagging** means grab-bag investigations smear across areas.
- **Tiny denominators manufacture signal.** Misc / cross-cutting at 58.7 bugs/kLoC
  is 375 lines against 22 grab-bag bugs, and means nothing.
- **Density cannot separate "inherently hard" from "under-decomposed."** Defect
  density *per file over git history* would; that is still not measured here.

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
  a dead entry point. At 47.7% comment density (Stat 3) comments are load-bearing,
  so wrong ones actively mislead rather than merely age.
- **Invariants are enforced per-site rather than derived.** The
  `caller_box != 0 → EPERM` rule appears at 3 sites in `src/syscall/container.rs`
  and is missing at the 2 that create and destroy box identity
  ([`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) §6).

### The pattern that explains both columns

Scheduler, SMP, BKL, fork/exec: measured, A/B'd, documented across 200+ archive
docs, and carrying the bulk of the 29,302 self-test lines. Those bugs cost weeks,
so they got instrumentation.

Everything this sweep found sits in the **quiet** areas instead — dead msgqueue
seams, a missing box-teardown call, orphaned allocator tests, inconsistent box
authorization. Stat 2 says the same thing structurally: `src/syscall` carries
10,012 production lines against 0 test lines by directory attribution, and its
bugs have been silent so far.

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
  runtime coverage. Some of the 43,700 production lines may never run on any
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
