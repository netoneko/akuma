# RUST.org.il talk — planning notes

Working notes for a talk at rust.org.il about self-hosting the Akuma kernel in Rust.
Audience: chill local meetup, not a conference. Focus is on consistently applying
what Rust was sold as, not on specific functions or technical problems.

## Speaker's own conclusions (verbatim, in the order stated)

- "the memes were right"
- "self hosting on amd64 story that requires way more than running on qemu"
- "rust shines in drivers, host level testing, the forced invention of inversion of control because of the big pre-porting refactor"
- "all of this would be a complete nightmare in c"
- "it's more about consistently applying what rust was sold as"
- "maybe i'll start with how it's exciting to be able to run things but running useful things forces you into a discipline because if you're doing things you should do them well"
- "now your own software stack can be built by open models in relatively short time with minimal investment of money"
- "the question is can reliable software be built with new tech, if yes, what enables it"
- "and rust definitely enables it in a variety of ways"
- "rust enables better discripline through the build system and tools"
- "the more reliable we make the process the faster we can move"
- "ease of debugging, strict compiler, easy host testing, easy extraction"
- "if i knew this sooner i would've moved faster"
- "rust compiler and cargo are great exercises for the whole system"
- "people in the audience care about their tools, so now that building your own tooling or even your own stack is possible with open source, open weights and open hardware, rust is a great tool to rely on"
- "and then kvm closes the self hosting loop one more time"
- "and with rust built drivers derived from open spec that allow networking that is essential to the whole thing"
- "many projects/companies are doing rust rewrite like google with argon and with google's tech track record i can trust they know what they are doing"
- "the future is now, cyberpunk is here, you can install a kernel from crates.io (theorethically) and have your whole supply chain owned at kernel level" (2026-11-01, on the tokio kernel crate. The numbers under it: Taoxichuan's image already links 15 crates.io crates, 21,861 audit lines and 789 of its 1,669 `unsafe` lines, all at ring 0; a kernel published as a crate adds a `build.rs` that runs on the builder's machine. The counter-point is that it is countable: 141,339 audit lines for the whole Akuma stack, against 2,440,506 for Taoxichuan on a Linux host)

## Context

- Origin anecdote: first winter of userspace bring-up happened in Taoxichuan,
  Jingdezhen, during a ceramics residency. Not in the talk, but the kernel's
  origin story.
- Google Argon reference checked: Gemini agent migrating C/C++ to Rust, Zircon
  kernel among the targets. Precise claim: "Google pointed an AI at migrating
  kernel code to Rust and it cleared their audit bar" — not "Google rewrote
  their kernel in Rust."
- The Ethernet driver (rtl8169) is the datasheet-clean story; rtw89 WiFi is not.
- KVM in 2 days because Rust pays off.
- Composability argument and the minimal-kernel experiment brief:
  [`MINIMAL_SELF_HOSTING_EXPERIMENT.md`](MINIMAL_SELF_HOSTING_EXPERIMENT.md) —
  has a "Measurements to take before the talk" checklist; its numbers are
  derived until re-measured on a clean tag.

## Before and after the cleanup (re-measured 2026-11-02, one tool, both ends)

The slide numbers. Goose + GLM wrote Taoxichuan up to the cutover; from there Opus cleaned it up
([`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md), A1-B15, 62 commits in two days). **Before** is the
cutover, `c362f165` (2026-11-01 13:39); **after** is `961a77be` (B15, 2026-11-02 16:37; no kernel source changed
after it). Both ends measured today with the same `scripts/cloc_akuma.py --audit taoxichuan akuma-amd64`,
`--features no-tests`, clean trees. The older sections below used earlier versions of the tool or a dirty tree, so
their "before" figures (84,761 on 10-30) do not compare with these; quote this table.

| audit lines (live code compiled into the image) | before (cutover) | after (B15) | change |
|---|---:|---:|---:|
| **taoxichuan alone** (the guest) | **87,761** | **77,839** | -9,922 (-11%) |
| of which the kernel's own code | 7,844 (`taoxichuan` crate) | 3,771 (`main.rs` 13 + `akuma-tokio-kernel-amd64` 3,758) | -52% |
| of which third-party (crates.io) | 41,748 (22 crates, 48%) | 21,861 (15 crates, 28%) | -48% |
| `unsafe` lines in the guest | 2,850 | 1,668 | -41% |
| - in the kernel's own code | 187 (`taoxichuan`) | 98 (`akuma-tokio-kernel-amd64`; `main.rs` has none) | -48% |
| - in other workspace crates | 791 | 781 | -1% |
| - in third-party crates | 1,872 (66%) | 789 (47%) | -58% |
| build-time host code (build.rs, proc-macros and their dependencies) | 31,743 | 428 | -99% |
| every file rustc read (`whole`, a C-style count) | 131,299 | 113,163 | -14% |
| **akuma-amd64 alone** (the host) | **152,104** | **136,727** | -15,377 (-10%) |
| `unsafe` lines in the host | 3,505 | 2,401 | -31% |
| - in the kernel's own code (`amd64/`) | 289 | 289 | 0 |
| - in workspace crates | 784 | 773 | -1% |
| - in third-party crates | 2,432 (69%) | 1,339 (56%) | -45% |
| **taoxichuan on an Akuma host** (union, shared lines once) | **161,128** | **141,903** | -19,225 (-12%) |
| what Taoxichuan adds to the host | 9,024 | 5,176 | -43% |
| share of Taoxichuan already live in the host | 90% | 93% | |
| **taoxichuan on a Linux host** (Linux 6.18 + KVM 2,362,283, + the guest) | **2,450,044** | **2,440,122** | |

- **The cleanup shrank the host too**, by more than it shrank the guest: most of it was shared code. The in-tree
  virtio driver replaced `virtio-drivers` and its stack (zerocopy alone was 8,334 lines and 743 `unsafe`) in every
  kernel, and with it went `syn` and the proc-macros (the build-time row).
- **The `unsafe` that went away was almost all dependencies'.** Ours barely moved: 978 -> 879 lines in the guest
  (kernel + workspace), 1,073 -> 1,062 in the host. Third-party `unsafe` fell by 1,083 lines in the guest and 1,093
  in the host, because the crates holding it (zerocopy, virtio-drivers, safe-mmio) left the build.
- **The guest's own code halved and stopped being a kernel.** `taoxichuan/src` went from 12,131 lines to a 19-line
  `main.rs`; what was its private syscall dispatch, spawn ring and fd model is now the shared glue the host runs
  (`akuma-syscalls-glue` went from not in the guest to its second-largest crate, 10,753). The workspace share grew
  (38,169 -> 55,965) for that reason: the same behaviour, now in code two kernels share and test.
- Against Linux: the guest went from **9.5x** to **10.7x** smaller than Linux trimmed to the same job (836,651),
  and from 21x to **24x** smaller than Linux as Firecracker ships it (1,868,016). The host went from 15.5x to
  **17.3x** smaller than Linux + KVM.
- What did not change: it still self-hosts. K1 == K2 at the cutover's predecessors (`6af48c06`, ryzen KVM and
  metal) and after: B15 under Pop's KVM (`5f568279...`) and under TCG on the Mac (`b0c08970...`).

## Bugs fixed, by subsystem (the bug-fix lists, 2026-11-02)

From the two itemized lists: [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md) (Akuma, the whole project's history) and
[`TAOXICHUAN_BUG_FIX_LIST.md`](TAOXICHUAN_BUG_FIX_LIST.md) (Taoxichuan's own code, its rigs and its experiment). One
item per distinct, named bug a doc records as fixed; a fix in code both kernels run is counted once, in Akuma's list.
Akuma's subsystems are tagged per write-up, not per bug, so a grab-bag doc lands in one row.

**Total: 1,586 fixes** - 1,481 in Akuma (415 docs) and 105 in Taoxichuan (28 docs).

| Akuma subsystem | fixes | % |
|---|---:|---:|
| Syscall / ABI compatibility | 245 | 16.5 |
| Memory & virtual memory | 177 | 12.0 |
| Boot & drivers | 141 | 9.5 |
| Scheduler & process management | 139 | 9.4 |
| SMP & locking | 136 | 9.2 |
| VFS & filesystem | 134 | 9.0 |
| Networking | 95 | 6.4 |
| Toolchain & self-hosting | 91 | 6.1 |
| Misc / cross-cutting | 64 | 4.3 |
| Console & terminal | 58 | 3.9 |
| Containers | 56 | 3.8 |
| Userspace apps & libraries | 40 | 2.7 |
| Signals & exceptions | 40 | 2.7 |
| SSH | 39 | 2.6 |
| Rump kernel & syscall proxy | 26 | 1.8 |
| **Akuma total** | **1,481** | |

| Taoxichuan subsystem | fixes | % |
|---|---:|---:|
| Syscalls, files & pipes | 37 | 35.2 |
| Processes, fork, threads & scheduling | 33 | 31.4 |
| SMP, time & boot | 9 | 8.6 |
| Exec & ELF loading | 8 | 7.6 |
| Memory & copy-on-write | 6 | 5.7 |
| Signals | 5 | 4.8 |
| Networking & readiness | 4 | 3.8 |
| Build & integration | 3 | 2.9 |
| **Taoxichuan total** | **105** | |

**This branch fixed 116**: all 105 of Taoxichuan's (the kernel exists only here), plus **11 in Akuma**. Akuma's
list on `main` reads 1,470; on this branch, 1,481:

| Akuma subsystem | `main` | branch | the fixes |
|---|---:|---:|---|
| Syscall / ABI compatibility | 242 | 245 | `dup`/`F_DUPFD` dropped `O_NONBLOCK` (tokio's signal driver blocked); `pidfd_open` flags and close-on-exec; `rt_sigpending` served by no kernel |
| Misc / cross-cutting | 59 | 64 | `SIG_UNBLOCK` left a stray SIGKILL/SIGSTOP bit; `akuma-pmm`'s CoW table a heap node per shared frame; a `kvm0` guest disk read as 856 MiB (herd reload race); the audit scanner swallowed code after a gated last field; amd64 self-test build broken since the boot-kit extraction |
| Memory & virtual memory | 176 | 177 | `kvm0` guest RAM capped at 1 GiB (one NPT directory) |
| Networking | 94 | 95 | in-tree virtio-net sent frames without the `virtio_net_hdr` (DHCP never answered) |
| Containers | 55 | 56 | every amd64 `box run` failed to spawn (glue's `exec` feature gated to AArch64) |
| **Total** | **1,470** | **1,481** | **+11**, from 9 docs (406 -> 415) |

All 11 turned up in Taoxichuan work: running a second kernel on the same crates, growing its VMs, measuring it.
**Five were bugs in `main`'s code**, which every Akuma kernel carries until this branch merges: `dup`'s lost
`O_NONBLOCK`, `pidfd_open`, the missing `rt_sigpending`, the CoW table and the 1 GiB guest-RAM cap. **Six were in code
this branch wrote**, caught and fixed before it reaches `main`: the `box run` regression (`3e2bb725`, the
one-ELF-loader step), the in-tree virtio-net header, the amd64 self-test build, `SIG_UNBLOCK` in the newly extracted
`akuma-syscalls-signal`, the herd reload race in the new `boot_dev.sh`, and the audit scanner.
- Two thirds of Taoxichuan's bugs (70 of 105) are in syscalls/files/pipes and processes/threads/scheduling. 59 of
  the 105 come from one document, the experiment's agent log
  ([`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md)).

## Crates: `main` vs this branch (2026-11-02)

Counted as directories under `crates/` with a `Cargo.toml`; "forbids `unsafe`" = `#![forbid(unsafe_code)]` in its
`src/lib.rs`. (`docs/reference/crate-safety.md` counts differently; these two columns compare with each other.)

| | `main` | branch | change |
|---|---:|---:|---:|
| crates in `crates/` | 98 | 117 | +19, none removed |
| of which forbid `unsafe` | 82 | 97 | +15 |
| kernel roots | `src/` (AArch64), `amd64/` | + `taoxichuan/` (19 lines), `osaka/` (10 lines) | +2 |

The 19 new crates, by size (raw lines of `.rs`, tests and comments included):

| crate | lines | `unsafe`? | what it is |
|---|---:|---|---|
| `akuma-tokio-kernel-amd64` | 6,134 | yes | the whole amd64 tokio kernel (was `taoxichuan/src`) |
| `akuma-boot-amd64` | 969 | yes | the boot kit: PVH/multiboot2 entry, the physmap, kernel page tables, heap + PMM |
| `akuma-ring3-amd64` | 920 | yes | ring 3: ET_EXEC parsing, the initial user stack, the syscall/sysret transitions |
| `akuma-virtqueue` | 886 | no | the driver side of virtio-mmio as pure decisions (registers, negotiation, split virtqueue); replaced `virtio-drivers` |
| `akuma-syscalls-glue-amd64` | 706 | no | the x86_64 syscall ABI over the shared glue for the fd family and x86-only time calls |
| `akuma-sched-amd64` | 569 | yes | the scheduler's machine half: per-slot page-table root, trap stack, user context, FP state |
| `akuma-lapic-amd64` | 566 | no | the LAPIC timer, EOI, PIT calibration, TSC rate |
| `akuma-elf-exec` | 525 | no | the pure exec decisions for an ELF image: interpreter, load placement, entry |
| `akuma-user-tree` | 512 | no | copy-on-write fork decisions over a 4-level user page-table tree |
| `akuma-user-layout` | 505 | no | the layout of a user address space: valid regions, brk window, mmap slab |
| `akuma-syscalls-signal` | 384 | no | signal bookkeeping: the `rt_sigaction` table, mask algebra |
| `akuma-runtime-tokio` | 322 | no | the syscall surface a tokio program needs, as one dependency |
| `akuma-fpu-amd64` | 274 | no | the FPU save area as pure byte algebra (fxsave/xsave layout, power-on image) |
| `akuma-user-walk` | 241 | no | user-memory access by page walk: region check, chunked copies, string readers |
| `akuma-syscalls-eventfd` | 234 | no | the `eventfd(2)` state machine |
| `akuma-syscalls-proc` | 222 | no | the process family's pure decisions: clone flags, exit, wait status |
| `akuma-comconsole` | 199 | no | the 16550 COM-port boot console |
| `akuma-affinity` | 135 | no | the pure half of `sched_setaffinity`/`sched_getaffinity` |
| `akuma-tokio-kernel` | 55 | no | `entry!` and the `Kernel` builder |

15 of the 19 forbid `unsafe`; the four that cannot are the amd64 machine layer (boot, ring 3, the scheduler's machine
half) and the kernel crate that drives it.

**Nothing fell out.** Of the 117, 116 are in at least one kernel's default build (`cargo tree -e normal,build` of
`akuma` for `aarch64-unknown-none` and `akuma-amd64`, `taoxichuan`, `osaka` for `x86_64-unknown-none`; with
`--all-features` the set is the same). The one that is in none is `akuma-scheduler`, by design: the scheduling-policy
simulator, a host tool (`sched-sim`) that ranks candidates before they go into a kernel.

## Facts re-measured 2026-11-02 (after B15: the kernel is a crate)

The "before and after" table above supersedes this section's numbers for slides; the 11-01 and 10-30 sections stay
as history. Measured at `961a77be` (B15 of [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md); no kernel source
changed after it), clean tree, `scripts/cloc_akuma.py --audit taoxichuan akuma-amd64`, `--features no-tests`. The
Linux 6.18 figures are still the 10-30 builds.

**The three numbers**

| what an auditor would read | 11-02 (B15) | 11-01 | 10-30 |
|---|---:|---:|---:|
| **taoxichuan alone** (the guest image) | **77,839** | 78,351 | 84,761 |
| of which Taoxichuan's own code | 13 (`main.rs`) + 3,758 (`akuma-tokio-kernel-amd64`) | 3,907 | 6,652 |
| akuma-amd64 alone (the host) | 136,727 | 135,167 | - |
| **taoxichuan on an Akuma host** (shared lines once) | **141,903** | 140,576 | 158,909 |
| **taoxichuan on a Linux host** (Linux 6.18 + KVM, 2,362,283, + taoxichuan) | **2,440,122** | 2,440,634 | 2,447,044 |

- **The kernel is a crate.** `taoxichuan/src/main.rs` is 19 lines (13 audit lines): one `Kernel` builder chain over
  `akuma-tokio-kernel` (44 lines, `forbid(unsafe_code)`: `entry!`, the builder, the linker script) and
  `akuma-tokio-kernel-amd64` (3,758 audit lines, 98 of them `unsafe`). A second kernel on the same crates, **Osaka**,
  is 10 lines and boots a from-scratch image. `taoxichuan/src` went 12,131 lines (cleanup start) -> 6,548 (11-01)
  -> 5,924 (before B15) -> **19**.
- Taoxichuan adds **5,176** lines to an Akuma host; **93%** of its live lines (72,663 of 77,839) are already live
  there. The Akuma-host total went up 1,327 because akuma-amd64 did (+1,560 over B1-B15, not broken down here), while
  Taoxichuan went down 512.
- The image: 75 workspace crates (55,965 lines) and 15 crates.io crates (21,861, **28%**). `unsafe`: 1,668 lines,
  **789** of them third-party (47%).
- Largest crates in the guest: smoltcp 11,030, `akuma-syscalls-glue` 10,753, `akuma-exec` 5,254, heapless 3,872,
  `akuma-tokio-kernel-amd64` 3,758.
- Against Linux: the guest is **10.7x** smaller than Linux trimmed to the same job (836,651) and **24x** smaller than
  Linux as Firecracker ships it (1,868,016). The host is **17.3x**. Linux's host alone is **30x** Taoxichuan.
- **Bugs this branch fixed: 116.** 105 in Taoxichuan's own code
  ([`TAOXICHUAN_BUG_FIX_LIST.md`](TAOXICHUAN_BUG_FIX_LIST.md), 28 docs; the biggest classes are syscalls/files/pipes
  37 and processes/fork/threads/scheduling 33), and 11 in shared Akuma crates, found through Taoxichuan or on the way
  ([`BUG_FIX_LIST.md`](BUG_FIX_LIST.md) went 1,470 -> 1,481 fixes, 406 -> 415 docs, over the branch's 566 commits).

**Runs on it (B15):** the battery + goose/GLM 10/10, `tokioprobe` at `smp=1` and real `smp=4`, `rustc` + `cargo
build`; Osaka the same `tokioprobe` and battery. Census: 76 syscalls used by the workloads, 0 unserved.

**Python works, as a side effect of tokio.** Taoxichuan was built to run tokio programs, and nobody aimed it at
Python. Then, on 2026-11-02, GLM (goose, inside a Taoxichuan guest under Akuma's hypervisor on the ryzen, working on
[infinity](INFINITY_ON_TAOXICHUAN.md)) needed to look inside crates. It reached for `python3` and downloaded rig-core,
reqwest and rustls-platform-verifier from crates.io with `urllib` over HTTPS. That is Alpine's CPython 3.14.8, an
unmodified musl binary that came along with the box's root filesystem when the toolchain image was copied from it.
`ssl` (OpenSSL 3.5.9), `sqlite3`, `threading` and `subprocess` all import and run. Nothing in the kernel knows
Python exists: it runs on the syscall surface that was built for tokio, rustc and cargo.

**Fixed points since 11-01:** A6 `8909a603` under Akuma's own hypervisor on the ryzen (`26d1ead2...`), B14 `ade75544`
under Pop's Linux KVM at `smp=4` (`6343290a...`), and **B15 `961a77be`, the crate kernel, under Pop's KVM: K1 == K2
(`5f568279...`, 2,304,624 bytes, about 7.5 minutes a generation)** - the kernel whose `main.rs` is 19 lines builds
itself reproducibly. **And under TCG on the Mac** (x86 emulated on arm64, no hypervisor, `fp_tcg.py`): **K1 == K2** (`b0c08970...`),
33 minutes a generation (see [`TAOXICHUAN_FIXED_POINT.md`](TAOXICHUAN_FIXED_POINT.md), "Under TCG").

## Facts re-measured 2026-11-01 (after the first day of cleanup)

Supersedes the audit figures in the 10-30 section below for anything quoted on a slide; the 10-30
section stays as the "before". Measured on `rust-org-il-experiment` @ `15ff2709`, clean tree,
`scripts/cloc_akuma.py --audit taoxichuan akuma-amd64`, `--features no-tests`. Linux 6.18 figures
are the 10-30 builds (that tree did not change). The day's work is the status table of
[`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md).

**The three numbers**

| what an auditor would read | audit lines | after A5 | 10-30 |
|---|---:|---:|---:|
| **taoxichuan alone** (the guest image) | **78,351** | 78,223 | 84,761 |
| of which taoxichuan's own crate | 3,907 | 3,672 | 6,652 |
| **taoxichuan on an Akuma host** (akuma-amd64 + taoxichuan, shared lines once) | **140,576** | 141,339 | 158,909 |
| **taoxichuan on a Linux host** (Linux 6.18 `x86_64_defconfig` + KVM, 2,362,283, + taoxichuan; nothing shared) | **2,440,634** | 2,440,506 | 2,447,044 |

"After A5" is the same measurement on the A5 commit (the process-model steps A1-A5 of the cleanup plan). Taoxichuan
now adds 5,088 lines to an Akuma host (73,135 of its 78,223, 93.5%, are already live there). The Akuma-host total
went *up* because akuma-amd64 did (135,167 -> 136,251): `e2d3bdc1` turned glue's `exec` back on there to fix `box
run`, not anything Taoxichuan did. Unsafe lines: 1,669 for Taoxichuan alone, 789 of them third-party.

- Taoxichuan adds **5,409** lines to an Akuma host (135,167 alone). **93%** of its live lines (72,942 of
  78,351) are already live in the host.
- Its own crate is **5%** of its image. The other 95% is 70 workspace crates and 15 crates.io crates.
- Against Linux: the guest is **10.7x** smaller than Linux trimmed to the same job (836,651) and
  **23.8x** smaller than Linux as Firecracker ships it (1,868,016). The host is **17.5x**. The whole Akuma
  stack is **17x** smaller than Linux host + minimal Linux guest (2,391,510), **20x** with Firecracker's
  guest, before the VMM. On a Linux host, the stack is 17x the all-Akuma one: Linux's host alone is
  30x taoxichuan.

**What moved in a day (each one a commit, `TAOXICHUAN_CLEANUP_PLAN.md`)**

- **virtio-drivers left every kernel's image** (`56d121bc`). An in-tree split-ring driver
  (`akuma-virtqueue` plus about 500 lines in `akuma-virtio`) replaced 14,521 third-party lines
  (virtio-drivers, zerocopy, safe-mmio, bitflags 2, thiserror): -13,981 net.
  The 10-30 fact "zerocopy has more `unsafe` lines than all workspace crates together" is gone with it.
- **Build-time host code went from 31,743 lines to 419.** `syn` arrived only through the virtio
  stack's proc-macros (`enumn`, `thiserror-impl`, `zerocopy-derive`; `cargo tree -i syn` at `3e2bb725`).
  It is no longer in the build, and "`syn` alone is 3x taoxichuan" no longer holds.
- **The device-tree parser left the x86 guest** (`a4cd34b2`): `fdt` is aarch64-only now.
- One ELF loader, not two (`3e2bb725`, -5,449). Smoltcp's unused DNS socket off (-983).
- Third-party code went from 49% of the image to **28%** (21,861 lines), and from 67% of its `unsafe`
  lines to **47%** (789 of 1,669).
- Largest crates in the guest now: smoltcp 11,030, `akuma-syscalls-glue` 10,734, `akuma-exec` 5,254,
  then taoxichuan itself at 3,907.
- `taoxichuan/src`: 12,131 lines when the cleanup began, 6,548 at `15ff2709`.

**Still true:** none of these kernels has been audited. These are sizes of the reading an audit would
take.

**Runs on it (1 day after the cutover):** goose + GLM (a prompt and a tool call), `apk add git`,
`rustc` + `cargo build`, `tokioprobe` (multi-thread runtime, `tokio::process`, HTTPS through rustls) at
`smp=1` and `smp=4`, and the fixed point K1 == K2 (`3e2bb725`, ryzen, SMP=1).

## Classic blunders (the user's list, being compiled)

Recorded as they come up in the "classic blunders" entry of
[`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md). Each one
fails silently: nothing errors, something just waits.

- **The ssh stdin pump** (2026-11-01): sshd wrote a child's stdin into a descriptor the child was not
  reading, and sent EOF through a syscall the kernel no longer had
  ([`TAOXICHUAN_SSH_STDIN_PUMP.md`](TAOXICHUAN_SSH_STDIN_PUMP.md)).
- **A timed sleep that never expired on an idle core** (same day): the wake pass ran only inside the
  switch, and the idle loop switched only for threads already ready
  ([`TAOXICHUAN_ONE_CLOCK_TIME_FOLD.md`](TAOXICHUAN_ONE_CLOCK_TIME_FOLD.md)).
- Honorable mentions: `wait4(-1)`, the networking pumps, an init its children cannot fork from.

## Facts to consider (measured 2026-10-30)

Information, not a storyline. Measured on `rust-org-il-experiment` @ `9f1e333f` plus the
working tree (which held uncommitted taoxichuan work by another agent), with
`scripts/cloc_akuma.py --audit` — method in
[`AUDIT_SURFACE_METHODOLOGY.md`](AUDIT_SURFACE_METHODOLOGY.md). "Audit lines" = code lines
rustc read for the exact build, minus `#[cfg]`-false and test-only items; comments and blanks
never count. Re-measure on a clean tag before quoting: the taoxichuan numbers move daily.

**Composition**

- taoxichuan adds **7,674 lines** on top of an Akuma host: akuma-amd64 alone is 151,235 audit
  lines, the two together 158,909. **91%** of taoxichuan's live lines (77,090 of 84,761) are
  already live in the host, including all 41,748 of its third-party lines.
- taoxichuan's own crate is **7.8%** of its image (6,652 of 84,761); the rest is 54 workspace
  crates and 22 crates.io crates.
- The aarch64 kernel's own crate is **80** live lines; everything else is composed.
- A third of what the compiler reads is compiled out: 127,706 lines in, 84,761 live. smoltcp
  alone loses 47% (IPv6, 6LoWPAN, 802.15.4 sit in the same files).

**`unsafe`**

- **zerocopy has more `unsafe` lines (743) than all 54 workspace crates in the guest together
  (738).** Akuma names zerocopy in exactly two function signatures
  (`crates/akuma-virtio/src/transport.rs`, `read_config_space`/`write_config_space`), whose
  four trait bounds virtio-drivers' `Transport` trait requires. The linked ELF has **0**
  zerocopy symbols: it is all generic code, inlined. (Fair note: that `unsafe` is the crate's
  purpose, transmute proofs in one place, checked with Kani and Miri.)
- Third-party code is **49%** of the guest's lines and **67%** of its `unsafe` lines (1,872 of
  2,812).
- The allocator (`akuma-alloc`, 267 `unsafe` lines) has more `unsafe` than the kernel crate
  (taoxichuan, 202).
- A crate named `safe-mmio` contributes **168** `unsafe` lines (via virtio-drivers).
- 41 of the guest's 77 crates have zero `unsafe` lines; together they hold 41,476 of the
  84,761 lines.

**Dead weight and oddities**

- A **device-tree parser** is in the x86 PVH guest, which has no device tree: `fdt` (914 lines)
  arrives through `akuma-mmu -> akuma-fdt`. 0 symbols in the ELF (the linker drops it), but it
  is still source in the image's dependency set.
- **Two `bitflags` major versions** ship in one image: 1.3.2 via smoltcp, 2.13.1 via
  virtio-drivers. virtio-drivers also brings `thiserror` into a `no_std` kernel.
- **`syn` alone (20,716 lines) is 3x taoxichuan's own code**, and never runs in the guest. All
  build-time host code (proc-macros, build scripts, their dependencies) is 31,743 lines, 37% the
  size of the image.

**The experiment** (git and
[`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md))

- 200 commits to `taoxichuan/` in 6.3 days (2026-10-24 to 10-30). Own code grew 2,246 -> 6,964
  production lines while direct `akuma-*` dependencies went 16 -> 34.
- `git blame` of `taoxichuan/src` at HEAD (non-blank, non-comment lines): 68.5% from commits
  named after goose sessions (s1...s8q, slices 1-3) that carry the user's identity and no AI
  trailer (git alone cannot prove they are goose-glm's); 31.4% from Claude Sonnet 5.5 commits,
  2,057 of them written on 10-30 alone; 32% of today's lines date from day one (10-24).
- Cost per tool call grows with session length: about 19k tokens under 40 calls, about 105k at
  130+. Six long sessions burned 71% of all tokens.
- Cost per milestone fell from 20-28M tokens to 5-11M after the methodology change; the
  cheapest was the process-model plan at 1.5M.
- About one brief in two carried a wrong premise, and goose's survey caught every one.
- **A week of testing on a hypervisor built by the same model.** Akuma's KVM (written with
  GLM-5.3-Flash, per the user; the z.ai dashboard puts the week before the experiment at ~200M
  tokens) hosted the metal track 10-24 -> 10-29: 2,700+ goose tool calls' worth of guest boots,
  deliberate guest faults, hangs, kills and `herd stop` on live guests, with **zero guest-caused
  host crashes**. The incidents in that week: two host resets not traced to KVM (10-26, BKL
  contention under a long in-host kernel build; 10-28, unexplained, Akuma recovered by itself) and
  two wifi data-plane freezes on 10-27 (rtw89, after a group rekey; the driver missed the dead
  link). Otherwise the ryzen's own wifi driver carried a week of ssh, git and package mirroring.
  After 10-29 the runs moved to QEMU TCG on the Mac and Linux KVM on Pop.

**Tokenomics** (goose's `usage_ledger` from the Akuma partition, cross-checked against the z.ai
usage API; full method and tables in the agent log, "GLM tokenomics from the goose ledger")

- The whole experiment's GLM work: **6,989 calls, 569.4M tokens** (16.1M GLM-5.3 on day one, the
  rest GLM-5.3-Flash). The ledger matches z.ai's own daily totals to within 0.1M.
- **98.3%** of input tokens were cache hits; output was 0.4-1.8% of tokens. An agent mostly
  re-reads its own context.
- At z.ai's pay-as-you-go API prices the whole experiment is **$24.91**. Two hours of GLM-5.3 on
  day one are $4.86 of that: 20% of the cost for 3% of the tokens.
- On the $18/month Lite plan it took **3 weekly quota resets**. The quota readings imply ~50
  credits per million tokens (about 200M Flash tokens per Lite week); 569M x 50 = 2.85 weeks.
- z.ai's published credit formula, with this cache rate and the calls' timing (only 222 of 6,989
  were in peak hours, the rest at the documented 50% off-peak rate), predicts **~36** credits per
  million. Part of the gap is the model split and part is outside the ledger: the GLM-5.3 day
  costs ~1,000 credits more than Flash would (~0.1 Lite week), and 10-20/21's 115M Flash tokens
  (another machine) add ~0.4 week if they fell inside the reset weeks, bringing the formula to
  ~2.4 of the user's 3 weeks. **Not all of it:** the quota readings were taken in Flash-only
  windows and are consistently token-based - ~195M Flash tokens per Lite week (five readings,
  192-203M) and ~35M per 5 hours - which sits between z.ai's own Lite estimates for Flash with no
  off-peak discount (146M/week) and with all of it (292M), although 97% of the calls were
  off-peak. z.ai's docs apply the off-peak rate to Flash (current plan 50%; legacy V2 plans 0.4x
  off-peak / 1.2x peak). The z.ai dashboard for 10-24 -> 10-30 (UTC+8 days) shows **97.2%
  off-peak usage**, 97.8% cache hits and 567.71M tokens (16.17M GLM-5.3, 551.54M Flash), matching
  the ledger, but no view of the credits charged; the user has not seen an off-peak discount on
  Flash, with development running around the clock. The same dashboard puts the week before the
  experiment at ~200M tokens (+184%). With no discount, this mix costs ~69 credits per
  million, ~145M tokens per 10,000-credit week, below the ~195M measured. Unresolved; a legacy
  plan, or a weekly allowance other than 10,000 credits, would not follow the formula used here.
- The worker was **GLM-5.3-Flash**, which the user rates a bit below Sonnet level; the
  supervisor and executor was Claude Sonnet 5.5.
- A refined-process milestone (after the 2026-10-26 methodology change) costs **1.6-46M tokens,
  $0.07-1.84** at API prices: `apk update` 6.3M ($0.26), `apk add curl` 7.0M, dynamic PIE 28.3M
  ($0.97), curl over HTTPS 46.4M ($1.84). The median execution milestone is about 16M, $0.60.
- One problem cost more than ten milestones: preemption on a single core (M4) took **159M tokens**,
  80% of a Lite week, against 186M for the ten refined milestones around it. The user stopped it.
- Cost per call grows with context: a 130+-call session averages ~105k tokens per call, a sub-40
  one ~19k.
- Pacing beats bursting: the refined days ran 57-125M tokens a day, so a Lite week lasted two or
  three days. One Lite month holds ~866M tokens, enough for a median-cost replay of the whole
  ladder (25-30 milestones, ~400-480M) at about 28M a day, one 5-hour window daily.
- The same median replay on the pay-as-you-go API is about **$15-20**, less than one Lite month,
  with no caps.
- **The supervisor out-consumed the worker.** Claude Code transcripts (`~/.claude-personal`, usage
  fields only): 1,221M Claude Sonnet 5.5 tokens on 10-24 -> 10-30 against GLM's 569M, 2.1x, and
  more than GLM on every single day; 10-30 alone was 340M (GLM's busiest day: 125M).
- **Claude executing** (10-29 18:00Z -> 10-30, 20 active hours, ~12 milestones from pn S1 to the
  CoW fork and the unsafe cut, 32 commits): 1,380 model calls, 1,571 tool calls, **406M tokens**.
  Its average call carried **294k** tokens of context; GLM's carries 56-77k.
- **The same work by GLM at 2-4x Claude's tool calls** (56-77k tokens per call): 2x 176-242M
  tokens, 3x 264-363M, 4x 352-484M (about 630M if the slowdown lands in long sessions, the M4
  pattern). That is 0.9-2.4 Lite weeks, $6-17 on the API. **At 2x slower GLM would spend fewer
  tokens than Claude did**, because each of its calls carries a quarter of the context.
- **What the supervisor's tokens bought** (each Claude call classified by its tools; keyword
  rules, good to a few points). Supervising goose (10-24 -> 10-29 18:00Z, 816M): **38% polling**
  (1,083 "nothing changed" reports at a 5-minute interval plus 230 sleeps/Monitors, 312M),
  ~30% real supervision (goose logs, steering, briefs, merges, verification), **16% Claude's own
  coding**. Executing (after the switch, 406M): **~76% coding** (reading source 40%, editing 20.5%,
  build/test and boot waits 16%), supervision 2.4%.
- **The "nothing changed" polls alone (257M tokens) are 45% of everything GLM spent on the
  experiment (569M).** Each re-sent ~237k tokens of context: about 2.8M tokens an hour to report
  that nothing happened, for ~90 hours.
- **Per milestone, supervising GLM cost Claude more than doing the work itself:** ~50M Claude
  tokens per supervised GLM milestone (10-26 -> 10-29, on top of GLM's own ~32M) against ~34M per
  milestone when Claude executed (and those were the harder ones). Token counts, not money: a
  Claude token and a GLM-Flash token are priced differently.
- **EXTRAPOLATION, not a measurement - the whole project on GLM alone, supervision not counted**
  (the user's position: with a proper process supervision is unnecessary or much cheaper). Claude's
  coding calls only (read source, edit, build/test, boot waits; plus the commits and docs the
  process requires): 1,645-1,796 model calls. Redone by GLM at 2-4x as many calls and 56-77k tokens
  per call: **184-553M GLM tokens**. Two independent methods agree at the low end: 2x gives
  184-277M, and Claude's ~12 milestones at GLM's refined median (16M each) give ~192M.
  Added to GLM's actual 569M (learning phase included): **753M-1.12B tokens, 3.8-5.6 Lite weeks,
  $32-45 on the pay-as-you-go API**; at 2x that is **about one $18 Lite month (~866M)**.
- **What keeps it an extrapolation:** no GLM run exists for the hardest class of milestone (rustc,
  cargo, threads, CoW fork - Claude executed those); the 2-4x factor is the user's assumption, not
  a measurement; the refined process was itself Claude-supervised; and the Lite quota charged ~40%
  more per token than z.ai's published formula, unexplained.
- **The user's assessment (2026-10-30):** the experiment was flawed and ultimately inconclusive, but
  the initial claim - that your own stack can be built by open models in a short time for little
  money - still looks plausible on these numbers.

**The open combination and what comes next** (the user's conclusions; agent log, 2026-10-30)

- No way to account for the harness: every number is the model's; goose's own contribution is not
  separable.
- goose (open source) + GLM-5.3-Flash (open weights) did complex kernel work: boot to HTTPS `curl`
  in the guest, on Akuma's own KVM, which the same model wrote; goose itself ran on Akuma.
- With more composable crates extracted, the same combination should be able to build specified,
  stripped-down kernels - for an agent, or for one Rust application.
- Next frontier idea: **crate-compatibility kernels**, e.g. `akuma-syscalls-tokio`, which pulls and
  sets up the syscall families tokio needs. Tokio already runs on Akuma (goose, `kot` on
  axum/hyper, reqwest + rustls), and its kernel-facing quirks (`EPOLLET` re-arm, the eventfd wake of
  an epoll-parked reactor) are already found and fixed.

- Future possibility (not a plan): harden by composition - compile capabilities out (read-only
  disk, a census-built syscall allow-list, no dynamic library loading), lock execution to signed
  binaries verified by a signed kernel, and filter the rest by user id - and verify the pure
  crates with fuzzing, Kani/Verus and Miri. Precedents: Firecracker's Kani harnesses found a real
  virtio-boundary bug; Asterinas confines `unsafe` to one core crate (~14% TCB); Hubris fixes its
  task set at build time in ~2,000 lines; AutoVerus has LLM agents writing Verus proofs (>90% of
  150 tasks). Details in the agent log, 2026-10-30.

**Rust features that make composition, hardening and verification possible** (counted
2026-10-30)

- **Cargo features + `#[cfg]`**: capabilities are selected at compile time. The root kernel gates
  8 syscall families behind `sc-*` features; the `extreme` profile compiles paths out of 8 crates;
  `no-tests` removes the boot suite. `--audit` shows a third of the lines rustc reads for
  taoxichuan are `#[cfg]`-dead in that build.
- **Build-script cfgs, checked**: 13 `kernel_*` cfgs (`kernel_smp_shared`,
  `kernel_profile_extreme`, the `kernel_no_bkl_*` phases) are emitted by `build.rs`, and
  `rustc-check-cfg` catches a misspelled one.
- **`#![forbid(unsafe_code)]`**: **79 of 107 crates** carry it; it cannot be switched off locally,
  so `unsafe` lives in a few leaf crates (`akuma-cpu`, `akuma-port`, `akuma-dma`) with one stated
  contract each, and `scripts/check_unsafe_budget.py` gates its growth.
- **`no_std` crates that also build for the host**: the same code runs in the kernel and under
  `cargo test` (2,763 host tests); pure decision crates return effects instead of performing them
  (`akuma-fdtable`), which is what makes them testable, and later provable.
- **The dependency graph as the carving tool**: taoxichuan is a `Cargo.toml` of 34 `akuma-*` path
  dependencies; rustc's dep-info gives the exact files compiled, which is how the audit counts.
- **Ownership for hardware**: `FrameLease` guards for DMA buffers, `DmaArray<E, N, Align>` with a
  borrow word, `#[must_use]` on PSCI calls.
- **Dependency injection through traits**: a crate states what it needs as a trait (or a callback
  table) and the composition root supplies it - `akuma-net`'s `NetRuntime` (taoxichuan plugs in a
  TSC clock, an RDRAND RNG and no-op park/wake), the `Filesystem` trait `akuma-ext2` implements,
  virtio's `Transport`. So a crate can be swapped for a replacement, a better one or an
  experimental one without touching its users; generics do the same at zero cost
  (`akuma-locks-rw-cell` is generic over `T`, which is what let `akuma-ext2` take
  `forbid(unsafe)`).
- **Macros for single-source tables**: one `syscall_table!` row generates the enum and both
  architectures' decode and encode, so a transposed syscall number can only be transposed once.
- **LTO and linker GC**: composed-in code that is never called costs nothing at run time (`fdt` and
  `zerocopy` have 0 symbols in taoxichuan's ELF).
- **Practical tests, not formal tools**: the tree uses no Kani, Miri or Verus and has not needed
  them; correctness comes from host tests, exhaustive model checkers written as tests, differential
  tests, and the same static binaries run on Linux and Akuma. The formal tools remain available
  for ordinary Rust crates if a component ever needs them.
- Future possibility: **crate sets preconfigured for a language runtime** (Rust std + tokio, or
  Go), selected with cargo features from the runtime's syscall census. Go is the next thing to try
  on taoxichuan: on the full kernel it started working in the same stretch as cargo building the
  kernel.

**Extraction on this branch** (snapshot 2026-10-30; more extractions are expected in the next few
days - re-count with `git ls-tree -d --name-only $(git merge-base main HEAD) crates/` against HEAD)

- **9 new crates** since the main merge-base (10-24): 98 -> 107. 2,812 production lines and 980
  test lines; 7 of the 9 carry `forbid(unsafe_code)` (not `akuma-boot-amd64`, `akuma-ring3-amd64`).
  `akuma-boot-amd64`, `akuma-comconsole`, `akuma-ring3-amd64` (10-24), `akuma-elf-exec`,
  `akuma-syscalls-eventfd` (10-26), `akuma-lapic-amd64` (10-28), `akuma-fpu-amd64`,
  `akuma-syscalls-signal`, `akuma-user-tree` (10-30). Plus new modules in existing crates:
  `akuma-mmap::HoleSet`, `akuma-syscalls-abi::legacy`, `akuma-syscalls-net::sockaddr`.
- **Extraction flowed back**: all 9 are linked by taoxichuan, and 4 (`akuma-boot-amd64`,
  `akuma-elf-exec`, `akuma-lapic-amd64`, `akuma-fpu-amd64`) are already consumed by the full amd64
  kernel; `sockaddr` took amd64's `unsafe` from 85 to 83.
- 9 of taoxichuan's 34 `akuma-*` dependencies did not exist a week ago.
- **taoxichuan builds taoxichuan** (`a99870a5`, 2026-10-30): a 3 GiB KVM guest compiles its own
  kernel in 11m41s (1,433,608-byte binary). Booting the produced ELF again - the stage 12 fixed
  point - is not recorded yet. Memory for the same compile: Linux 447 MiB, Akuma 1,356 MiB,
  taoxichuan at least 2,460 MiB (eager file mmap).

**Linux comparison** (Linux 6.18, built 2026-10-30; method in `AUDIT_SURFACE_METHODOLOGY.md`)

> Of course Linux works better and was written by smart people. It's a big plus if you are not an
> idiot. However,

- Guest: taoxichuan **84,761** audit lines; Linux trimmed to the same job (tinyconfig +
  kvm_guest + taoxichuan's features) **836,651** (9.9x); Linux as Firecracker ships it
  **1,868,016** (22x).
- Host: akuma-amd64 **151,235**; Linux `x86_64_defconfig` + KVM **2,362,283** (15.6x).
- Stack (host + guest, shared lines counted once): Akuma **158,909**; Linux **2,391,510** with
  the minimal guest (15x), **2,861,309** with Firecracker's (18x), before counting the VMM.
- Linux's minimal guest has more lines in `include/linux` headers alone (136,437) than
  taoxichuan's whole image.
- Not a risk comparison: Linux's lines have decades of review by many people; these numbers are
  how much reading each stack asks of someone who wants to vouch for it.
- **taoxichuan has never been audited.** Its 84,761 lines are a measure of what an audit would
  have to read, not of anything anyone has read.
- The user's position: **Linux reigns supreme** - ever since they installed it on their
  grandfather's Siemens workstation in 2001. — `scripts/linux_audit_surface.sh` (firecracker / minimal /
host configs of 6.18) is building; numbers go in `AUDIT_SURFACE_METHODOLOGY.md` and here.

## Hardening by composition: ideas and positioning (2026-10-30)

Summary written for the talk; numbers checked against the measurements above (corrections noted
inline).

### Layered walls (each optional, each stops something different)

- **Box with its own syscall profile.** The cheapest wall. It narrows what the app can reach, but
  it shares the kernel, so a bug in an allowed syscall still escapes it.
- **Ring 3 process.** Contains faults and exploits to the task, and gives enforcement points:
  user-id filtering, an egress allow-list, `copy_to_user` validation and a signature check at
  `execve`.
- **KVM guest.** Stops a kernel bug from touching the host. The guest runs on Akuma KVM, Linux
  KVM, Firecracker or TCG.

### Compile-time hardening

- Read-only disk image with no ext2 write paths.
- Syscall table as an allow-list built from the app's census, so an unlisted call has no code
  behind it.
- No dynamic library loading: compile out `PT_INTERP`, which also removes `LD_PRELOAD`-style
  injection.
- Execution locked to signed binaries, verified by a signed kernel. A guest is checked by the
  host's VMM, bare metal by the bootloader.
- W^X (taoxichuan has had it since `075de2ec`), an egress allow-list in the socket table, and
  per-user-id filtering for what cannot be compiled out.

### Syscall middleware (a layer stack on the boundary)

- **Layers:** record, allow-list, egress and quotas, path rewriting, ABI emulation shims (NetBSD),
  fault injection, deterministic time.
- **Shape:** a tower-style `Service` over decoded syscalls, with pure crates that get host tests
  and differential suites.
- **Constraints:** layers are inside the TCB, so there must be a single dispatch entry. Blocking
  calls (`futex`, `epoll_wait`, `wait4`) do not wrap cleanly, so start with layers that decide
  before the call and observe after it.

### Dev and ship from one composition root

- Develop on a permissive debug kernel with record, trace and fault-injection layers.
- Run real workloads to produce the census.
- Ship a second build with the allow-list from the census, debug layers compiled out and unneeded
  families absent.
- Sign it. Only that kernel is accepted in production.
- **Risks:** a census covers only exercised paths, so use a log-and-allow stage before deny.
  Tracing can hide timing bugs, so keep layers off scheduler and futex paths. Debug and shipped
  builds must differ only in features.
- taoxichuan's `trace-err` feature is the proto-version of this.

### Crate-compatibility and runtime kernels

- `akuma-syscalls-tokio`: a crate that pulls in exactly the syscall families tokio needs. Tokio
  already runs on Akuma, and the `EPOLLET` re-arm and eventfd-wake quirks are fixed.
- Generalised: preconfigured kernels per language runtime (Rust with tokio, Go), with an strace
  census as the feature list and Linux-differential suites as the conformance gate.

### Comparison with other models

| | Unikernel | Framekernel (Asterinas) | Akuma composition |
|---|---|---|---|
| Surface reduction | Build only the needed OS pieces | Not the main point | Allow-list from census, capabilities compiled out |
| App/kernel isolation | None inside the image | Services separate from the app | Ring 3, optional box and KVM |
| Where `unsafe` lives | Everywhere in the image | One privileged framework (OSTD) | Leaf crates, some in the composition root |
| Linux ABI | Partial | 245 x86_64 syscalls dispatched (measured at `7861bff`) | 198 on amd64; runs rustc and cargo |
| Verification target | Little | The framework (Verus) | Pure crates - a future possibility; practical tests today |

**Linux ABI coverage on x86_64** (counted 2026-10-30 from each kernel's dispatch: Asterinas
`kernel/core/src/syscall/arch/x86.rs` at `7861bff`; Akuma amd64 = `akuma-syscalls-abi` rows + amd64's
native arms + legacy folds; taoxichuan = `dispatch_inner`'s arms + legacy folds, without the 38 its
wrong `GETPID` constant answers - see `SYSCALL_CONSTANT_AUDIT_2026_10_30.md`). Dispatched calls,
not completeness: some handlers on every side are partial.

| | taoxichuan | Akuma amd64 | Asterinas |
|---|---:|---:|---:|
| syscalls dispatched | 120 | 198 | 245 |
| share of Linux 6.18's 383 | 31% | 52% | 64% |

- Akuma amd64 serves 71% of Asterinas's set (174 of 245); 88% of Akuma's set is also in Asterinas.
  Together they cover 269 (70% of Linux). Asterinas-only: namespaces, cgroups, inotify, xattrs,
  POSIX timers, the new mount API; Akuma-only: SysV message queues, Linux AIO, the clock setters.
- taoxichuan serves 60% of Akuma amd64's set and 48% of Asterinas's; **98% of taoxichuan's calls
  are in each of the other two** - a near-pure ladder, taoxichuan (120) inside Akuma amd64 (198)
  inside Asterinas (245). taoxichuan-only against Akuma amd64: `sendfile`, `getrlimit` (the minimal
  kernel got them first); against Asterinas: `membarrier`, `copy_file_range`.
- **taoxichuan builds itself, runs `cargo -j4`, git, HTTPS curl and apk on 31% of Linux's x86_64
  syscalls** (the experiment brief had estimated 70-90 calls for self-hosting).
- Akuma's AArch64 kernel dispatches 201 of the 325 asm-generic calls (62%).

What userspace buys over a unikernel: fault containment, a smaller exploit blast radius,
enforcement points, multi-process workloads (`cargo -j4`) and unmodified binaries that can be
signed separately from the kernel. What it costs: syscall-crossing overhead, and complexity (CoW
fork, mmap, the process table), which is where the bugs and the `unsafe` cluster.

**Positioning:** Akuma takes the unikernel's pick-only-what-you-need build, keeps a monolithic
kernel's Linux ABI and process model, and is moving toward a framekernel-style `unsafe` boundary.
The `unsafe` confinement is by convention, not proof, so do not claim a TCB equivalent to
Asterinas's.

### Numbers for the slides

- **Production lines and `unsafe` sites:** the AArch64 bin crate has 136 lines and 0 `unsafe`
  sites. taoxichuan has about 6,900 lines, with `unsafe` sites going 131 -> 67 -> 55 (HEAD
  `16936a0a`). After the cleanup (2026-11-01 @ `15ff2709`): 6,548 lines and **34** `unsafe` sites.
  The amd64 bin has 83.
- **Audit surface (Linux 6.18; re-measured 2026-11-01 @ `15ff2709`):** taoxichuan's guest is 78,351
  audit lines against 836,651 for a trimmed Linux guest (10.7x) and 1,868,016 for Firecracker's
  (23.8x). The host is 135,167 against 2,362,283 (17.5x). The whole stack is 140,576 against
  2.4-2.9M (17-20x), before counting the Firecracker VMM. Taoxichuan on a Linux host: 2,440,634.
  (10-30: 84,761 / 151,235 / 158,909.)
- **Memory for the same compile:** Linux 447 MiB, Akuma 1,356 MiB, taoxichuan at least 2,460 MiB
  (eager file mmap).
- **Caveat:** these are sizes of the reading an audit would take. None of the Akuma kernels has
  been audited.

### Ownership and portability

- Full ownership down to KVM closes the signing chain, gives one debugging surface across guest
  and host, and allows the 140,576-line whole-stack figure (158,909 on 10-30). It also means owning the audit burden,
  with firmware, microcode and the CPU's virtualisation hardware still below the stack.
- The guest also runs on Linux KVM, Firecracker and TCG, so the claims are reproducible by others
  and host bugs separate from guest bugs. On a Linux host the total stack is about 2.4M lines
  again (2,440,634 on 11-01), so show two rows: full ownership (Akuma on Akuma, 140,576) and guest
  only (taoxichuan on any KVM, 78,351).
- **Ready-made line:** "It works in practice" (self-hosted build, `cargo -j4`, leak gate), then
  the architecture.
