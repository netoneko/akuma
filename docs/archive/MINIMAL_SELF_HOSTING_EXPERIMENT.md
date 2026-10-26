# The minimal self-hosting kernel — experiment brief

**Status: in progress (started 2026-10-24); see [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md). Date: 2026-10-23.**

**Oversight note (2026-10-24):** the host track and goose supervision moved from
Kimi to Claude Sonnet 5 after Kimi ran out of tokens (a bad subscription setting
on the owner's side). The metal track is still goose + GLM.

Prepared as part of rust.org.il talk prep
([`RUST_ORG_IL_TALK.md`](RUST_ORG_IL_TALK.md)) — the talk's composability
argument should be backed by fresh measurements, not the derived estimates
below. See "Measurements to take before the talk".

Question: how small can the composed kernel be and still boot under a
hypervisor, serve ssh, and compile itself? Extraction turned out to be cheap;
this tests whether composition is cheap too — build a kernel from the crate
tree that satisfies exactly one constraint, with the dependency graph as the
carving tool.

The motivating observation (`LINE_COUNT_ANALYSIS_POST_AMD64.md`): `src/`
production code is 136 lines, the kernel is already a composition of ~98
crates, and most of `amd64/src` is hardware ambition, not ABI. If that is
true, a "just enough" kernel is a deletion job plus one or two more
extractions, not a rewrite.

---

## The target

Boot under Firecracker-style virtio-mmio (no PCI walk), single vCPU,
ext2 root on virtio-blk (a ramdisk root will not do — the nightly toolchain
plus vendored registry is a gigabyte-scale rootfs that must arrive
pre-staged), run `sshd`, and complete `cargo build` of the kernel inside
the guest. Everything else — USB, audio, WiFi, NVMe, framebuffer, SMP
bring-up, and the KVM hypervisor itself — stays out.

Why ext2-on-virtio-blk is nearly free: `akuma-ext2` is a host-tested crate,
the transport is `akuma-virtio`, and the page cache already exists
(`akuma-fpcache`). What gets added back versus a tmpfs-only picture is the
acting half: `blk.rs`-class glue plus the virtio-blk queue pair.

## Deliverables (pinned, 2026-10-23)

The ladder, in order. Each stage has a mechanical gate; a stage counts when its
gate passes under KVM, snapshot-restored from a known-good boot. The ordering
is load-bearing — apk needs DNS and TLS before it can fetch an index, git
clone needs the same stack, cargo needs git's transport. Do not reorder to
make an earlier stage prettier.

| # | deliverable | gate |
|---|---|---|
| 1 | booting in KVM | reaches init spawn, banner on console, zero `[Fault]`/`PANIC`/`[sig!]` in the ring |
| 2 | userspace works | a static musl hello runs, exit 0, output matches host-built binary |
| 3 | busybox works | `ash` runs a script; `ls`/`cat`/`echo`/`sh` behave against the ext2 root |
| 4 | sshd works | ssh round-trip from host executes a command, output byte-equal (the `vm_ready` check, not the log marker); exec-only — no pty per the exclusions below |
| 5 | **no herd** | constraint, not a stage: init is kernel-spawned (`init=/bin/ash` + an init script, the extreme-size pattern); no box/container machinery in the image or the syscall path |
| 6 | `apk add curl` | index fetch over the network succeeds; package unpacks; `/etc/apk/world` updated; binary present and linked |
| 7 | curl works | HTTPS fetch of a fixed URL; body checksum-equal to the same fetch from the host |
| 8 | git clone works | clone over HTTPS; `git log` and `git fsck` agree with the host-side clone |
| 9 | cargo fetch works | resolve + download the kernel's own crate graph (or a pinned small one); `--offline` build from the cache succeeds |
| 10 | rustc works | compile `hello.rs` in-guest; the binary runs in the same guest |
| 11 | cargo works | `cargo build` of a small project completes first-try |
| 12 | **the fixed point** | in-guest `cargo build` produces a kernel ELF; the ELF is extracted via virtio-blk and **booted in KVM again**, reaching the same banner — and byte-compare against the host-built kernel where determinism allows |

Stage 12 is the demo. Extraction under KVM is a file copy off the virtio-blk
image, not the metal `dd`-onto-GRUB story (`AKUMA_AMD64_BARE_METAL_SELFHOST.md`
§1) — the fixed point, minus the bootloader.

Reused harnesses: the gates mirror the existing acceptance playbooks
(`acceptance/05` boots, `acceptance/10` self-hosts) and the
`trace-failing-syscalls-amd64.md` runbook names any syscall the census missed.
The image-building machinery (`populate_disk.sh`, `bootstrap/`) stages the
toolchain the same way the existing self-host image does.

## Process (pinned, 2026-10-23)

**Build and extraction are one loop.** Each next stage is a new line of
code, init step, or injection point in the composition root that pulls in a
crate. The goal is to pull as few crates as possible — and where the needed
logic still lives in `amd64/src/`, the stage's work is extracting it into a
crate first, then pulling it. Composition and refactoring are not separate
projects; every stage either adds a dependency or creates one.

**Dual-track verification, parity as the gate.** The same kernel is checked
from two sides, and both sides run on open-weight models — the experiment
eats its own dogma here: the talk's commitment is that this stack can be
built with open tools (`RUST_ORG_IL_TALK.md`), so the tracks are:

- **On the metal track**: an agent (goose with GLM) running on Akuma itself
  builds and tests the amd64 target under its own KVM, closing the loop on
  the box (the proven loop of `AKUMA_KVM_DEV_CYCLE.md`).
- **The host track**: Crush + Kimi on the Mac builds the same kernel and
  checks it against Firecracker under Lima.

A stage counts when *both* tracks pass. The two hypervisors disagreeing about
the same kernel is a finding, not noise — parity between KVM-on-Akuma and
Firecracker-on-Lima is the experiment's cross-validation, and each track's
harness names what the other's silence hid. Using open-weight models on both
tracks is load-bearing: if the minimal kernel can be composed by the same
class of open tools the talk claims the whole stack is buildable with, that
is the claim, demonstrated.

## Session protocol (pinned, 2026-10-25)

Stages 1-6 were run as a ladder, not as an experiment: briefs differed in shape, nobody wrote down what a session cost, and
two briefs carried supervisor mistakes (stage 7A's gate used the gateway's own address `10.0.2.2` as a host alias, which no
guest ever did, and sent the agent down a dead end; the agent read almost none of the KVM docs because the briefs only named
the previous `RESULT` files). From stage 7B on every goose session follows this:

1. **One brief shape** (`scripts/kvm/goose/BRIEF_TEMPLATE.md`): the key fact the agent must not rediscover, a *read-first*
   list of specific doc sections (the agent greps source files by habit and does not find `docs/` by itself), gates with the
   exact command and expected output, ops traps, an **event-based** commit rule, and a stop rule counted in boots.
2. **Before sending, the supervisor checks the brief against the docs**: every address, key and command in a gate must appear
   in a doc or a passing run. A gate invented from memory is the supervisor's bug.
3. **The agent has no clock.** Rules are tied to events it can see (a boot, a build, a fix), never to minutes.
4. **Same numbers for every session**, from `scripts/goose_session_stats.py` (run on the box against goose's `sessions.db`):
   tool calls, calls before the first edit, input/output tokens, `docs/` reads and which files, source reads, `vm-dev`
   boots, commits and pushes. They go in the agent log under the session's section.
5. **The composition rule travels in every brief** (template § "Composition rule"): look in `crates/` and `amd64/src/` before
   writing, depend on or extract rather than copy, and report pulled / extracted / written-new plus `taoxichuan/src` line counts
   per session. It was in briefs 01-04 and silently dropped from 06a on; by stage 7 the composition root had grown to 4,450
   newly written lines (`main.rs` 2,928). A rule stated once at the top of a doc is not a rule the agent sees; it has to be in the brief.
6. **Changes are one at a time where possible.** 7B changed three things at once (docs list, corrected gate, event-based
   commit rule), so its numbers against 7A show a difference, not a cause; say so in the log rather than crediting the docs.
   With one run per stage there is no statistics, only a record that makes the next brief better.

## Exclusions (also pinned)

No pty layer, no users/multi-user machinery, none of it. sshd serves
exec-only sessions (no terminal allocation); init is a script, not a service
manager; everything multiuser-shaped stays out of the image and out of the
syscall path. "The most minimal Rust setup ever" is the constraint — every
exclusion is load-bearing for that claim.

How to get there — the concrete extraction order per stage — is deliberately
not pinned yet.

## The carve, measured

From the raw file survey of `amd64/src` (2026-10-23, ~56k raw lines):

| class | lines | content |
|---|---:|---|
| skippable under microvm | ~11,500 | xHCI 2,532 · HDA 1,092 · rtw89 ~2,379 · touchpad 784 · UAS 681 · NVMe 597 · kbd/input/i2c/fbdev/wifi/PCI/watchdog/splash |
| the hypervisor itself | ~6,000 | `kvm/` + `kvm.rs` — booting *under* a hypervisor means not being one |
| ABI floor (stays) | ~38,400 | `usermode.rs` 9,220 · `fd.rs` 3,819 · `mm.rs` 3,522 · `sched.rs` 3,093 · signal · net · loader · exec — minus what single-vCPU drops (lapic, shootdown, half the BKL policy) |

Two notes. The skippable 31% is also where most of the remaining `unsafe`
lives — the composable target is the safest subset of the tree. And the
"ABI floor" is mostly *acting* half; the decision halves of the same
subsystems are already out in crates.

## The syscall census

Two measured anchor points from the archive plus a derived target, with a
package manager's measured table in the Background as a cross-check that
the shape is stable across workloads.

**Floor — 20 syscalls.** `AKUMA_AMD64_RUST_STD.md` §2: an in-kernel
`STRACE=1` census of a hello-world Rust std binary —
`arch_prctl`, `set_tid_address`, `poll`, `rt_sigaction`, `sigaltstack`,
`brk`, `clock_gettime`, `clone`, `gettid`, `exit_group`, `rt_sigprocmask`,
`mmap`, `mprotect`, `write`, `open`/`fstat`/`read`/`close`, `munmap`.
Four answered `ENOSYS`; only one was fatal. musl's fallbacks mean the floor
is lower than it looks.

**Target — ~70–90 syscalls.** The union of (a) the syscall vocabulary the
amd64 self-host bring-up actually records (`AKUMA_SELF_HOSTING_AMD64.md`,
~68 names grepped from the doc) and (b) a derived list for cargo's
file-locking and hardlink behavior (`fcntl`, `flock`, `linkat`) plus
terminal ioctls for sshd (`TCGETS`, `TIOCGWINSZ`). The file cluster
(`openat`, `pread64`, `getdents64`, `newfstatat`, `statfs`, `renameat`,
`linkat`, `utimensat`, `faccessat`, `fchdir`), memory (`mmap`, `mprotect`,
`mremap`, `madvise`, `brk`, `munmap`), process (`clone`, `execve`, `wait4`,
`exit_group`, `arch_prctl`, `set_robust_list`, `rseq`, `prlimit64`,
`getrandom`, `getrusage`, `uname`, `sysinfo`, `sched_getaffinity`,
`sched_yield`), identity (`getpid`, `getppid`, `gettid`, `getuid`–`getegid`,
`umask`, `getcwd`, `chdir`), signals (`rt_sigaction`, `rt_sigprocmask`,
`rt_sigreturn`, `sigaltstack`, `kill`, `tgkill`), waiting (`epoll_create1`,
`epoll_ctl`, `epoll_wait`/`epoll_pwait`, `eventfd2`, `ppoll`, `poll`,
`pipe`/`pipe2`, `dup`–`dup3`, `socketpair`), time (`clock_gettime`,
`clock_getres`, `clock_nanosleep`, `nanosleep`, `gettimeofday`, `times`),
fsync/ftruncate, and the socket set (`socket`, `bind`, `listen`, `accept4`,
`connect`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`,
`sendto`/`sendmsg`, `recvfrom`/`recvmsg`, `shutdown`, `ioctl`).

## Syscall → crate map

The point of the experiment: each surviving syscall family should resolve to
an already-extracted crate, so composing the kernel is picking crates, not
writing them. Current ownership (verify against the tree before relying on
this table — it is a map, not a contract):

| family | syscalls | owning crate(s) | acting half stays in |
|---|---|---|---|
| ABI vocabulary | all | `akuma-syscalls-abi`, `akuma-syscalls` | both kernels' dispatch |
| Memory | `mmap`, `mremap`, `madvise`, `munmap`, `membarrier`, `brk` policy | `akuma-syscalls-mem` (plan), `akuma-mmap`, `akuma-user-space` | `amd64/src/mm.rs`, glue |
| Futex / sync | `futex` | `akuma-syscalls-sync` | lock, waits, in-hold reads |
| Poll / wait | `epoll_*`, `ppoll`, `pselect6`, readiness | `akuma-syscalls-poll`, `akuma-net-yarn` (wait loop) | waker registration, `EPOLL_TABLE` |
| Time | `clock_gettime`, `clock_nanosleep`, `nanosleep`, `times`, `gettimeofday`, `getrusage` | `akuma-syscalls-time` | comparator + dispatch |
| File metadata / dirs | `fstat`, `newfstatat`, `statfs`, `getdents64`, `access`/`faccessat`, `readlinkat`, `symlinkat` | `akuma-syscalls-glue` fs arms over `akuma-vfs`/`akuma-ext2` | VFS resolve, ext2 acting |
| File mutation | `renameat`, `linkat`, `unlinkat`, `mkdir`, `utimensat`, `chmod`/`fchmodat`, `ftruncate`, `fsync`/`fdatasync` | `akuma-ext2` + glue | ext2 journaling-adjacent paths |
| Fd lifecycle | `openat`, `close`, `dup`–`dup3`, `fcntl`, `flock`, `pipe`/`pipe2` | `akuma-fdtable` (decisions), `akuma-exec` (`SharedFdTable`, acts) | glue / `amd64/src/fd.rs` |
| Process | `clone`, `execve`, `fork`, `wait4`/`waitid`, `exit`/`exit_group`, `kill`, `getpid` family, identity syscalls, `umask`, `getcwd`/`chdir`/`fchdir`, `prlimit64`, `sched_getaffinity`/`sched_yield` | `akuma-exec`, `akuma-exec-core`, `akuma-elf` | process table, address-space switches |
| Signals | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `sigaltstack`, `tgkill` | `akuma-exec` + glue | frame build/restore |
| Address space | `arch_prctl`, `set_tid_address`, `set_robust_list`, `rseq`, `mprotect` | `akuma-mmu`, `akuma-pagetable`, `akuma-pte-amd64`, `akuma-cpu` | `amd64/src/usermode.rs`, `mm.rs` |
| Sockets / net | `socket`…`shutdown`, `getsockname`/`getpeername`, sockopts | `akuma-net` (stack + AF_INET table), `akuma-syscalls-net` (ifreq) | glue net arms |
| AF_UNIX | `socketpair`, `sendmsg`/`recvmsg` (`SCM_*` base) | `akuma-net-unix` | glue |
| NIC | — (device, not syscall) | `akuma-net-nic` + `akuma-virtio` (virtio-net) | rings, doorbell |
| Terminals | `ioctl` (`TCGETS`, `TIOCGWINSZ`) | `akuma-tty` (the one line discipline) | pty/session glue |
| RNG | `getrandom` | `akuma-primitives` or glue | — |
| Block | — (device) | `akuma-virtio` (virtio-blk), `akuma-ext2` | queue pair glue |

Carving rule that falls out of the map: a syscall family survives iff its
crate chain is already extracted; the experiment's *new* work should be
limited to the acting halves that still name a removed subsystem (the
hypervisor's, the PCI layer's, SMP's) — if a carve wants to touch a decision
crate, the seam was drawn wrong and the experiment says so.

## Method

1. **Measure, don't derive.** `strace -f -c cargo build` and an sshd session
   on real Linux produce the exact census in an afternoon; diff it against
   the target list above. The kernel-side tooling already exists:
   `strace_err`, the `[sc!]` trace, and
   `docs/runbooks/trace-failing-syscalls-amd64.md`.
2. **Compose.** New bin crate (or feature set) selecting the crate chain;
   boot in KVM. KVM is the iteration vehicle — snapshot, boot in seconds,
   no metal reboots.
3. **Gate.** `cargo build` completes inside the guest; the ELF matches a
   host build (the byte-identical fixed-point check,
   `AKUMA_AMD64_BARE_METAL_SELFHOST.md` §4); sshd serves a session from
   the host. Any `strace_err` line naming an unimplemented syscall the
   census says should exist is a failure.

## Why this is worth doing beyond the size

The constraint is an auditor. Composing "just enough" forces boot to stop
being a script (dependency-ordered init instead of `kernel_main`'s fixed
sequence) and forces the control loop (tick source, netpoll policy, idle
brackets) to become injectable — the same sweep the pre-port refactor did
for syscalls, aimed at the two least-composed parts of the tree. It is also
the right shape of task for an on-box agent: a crisp verifiable loop with a
mechanical oracle, tested in KVM, with this archive as its textbook.

---

## Measurements to take before the talk

The numbers in this brief are derived estimates or stale snapshots. Before
any of it is presented, re-measure — the repo's own rule is "never trust a
percentage without re-measuring" (`600_BUGS_ANNIVERSARY.md` slide 09):

- **Fresh line-count snapshot on a clean tag.** This brief and
  [`LINE_COUNT_ANALYSIS_POST_AMD64.md`](LINE_COUNT_ANALYSIS_POST_AMD64.md)
  were measured on the `kvm-part-1` working tree with uncommitted changes
  (see its Caveats). Cut a tag and re-run `scripts/cloc_akuma.py` there;
  quote the tag, not the branch.
- **The actual syscall census.** Run `strace -f -c cargo build` (full clean
  self-host build) and a scripted sshd session on real Linux; diff against
  the target list in "The syscall census". Replace the derived portion with
  the measured union and note the delta — if the census surprises the
  derived list, that finding goes in the talk too.
- **Carve feasibility, one number.** From the clean tag, count
  `amd64/src` production lines excluding the microvm-skippable files (the
  ~31% class in "The carve, measured") so the "deletion not extraction"
  claim is a measured line count, not a raw-file estimate. Raw lines and
  production lines are not interchangeable.
- **Unsafe in the subset.** Production `unsafe` sites of the skippable
  files specifically — the "the carve deletes most of the remaining unsafe"
  claim needs its own count, currently asserted from the wave docs'
  per-file tables rather than measured on the skippable set.
- **Demo rehearsed end-to-end.** The live boot (`git clone && cargo build`
  in-guest) on the actual hardware, including the failure mode: know what
  the recovery looks like when the demo gods frown, and have the
  `strace_err` runbook (`docs/runbooks/trace-failing-syscalls-amd64.md`)
  warm.

## Background

- [`LINE_COUNT_ANALYSIS_POST_AMD64.md`](LINE_COUNT_ANALYSIS_POST_AMD64.md) —
  the sizes this brief builds on.
- [`AKUMA_AMD64_RUST_STD.md`](AKUMA_AMD64_RUST_STD.md) — the 20-syscall floor.
- [`AKUMA_SELF_HOSTING_AMD64.md`](AKUMA_SELF_HOSTING_AMD64.md) — the recorded
  self-host vocabulary this brief's target list was grepped from.
- [`APK_MISSING_SYSCALLS.md`](APK_MISSING_SYSCALLS.md) — a measured 48-syscall
  table for a package manager (asm-generic numbering): a cross-check that the
  floor-and-target shape is stable across workloads, not just self-hosting.
- [`AKUMA_AMD64_STREAMLINING.md`](AKUMA_AMD64_STREAMLINING.md) — the
  pre-port refactor whose shape this experiment repeats, aimed at boot and
  the control loop.
- [`AKUMA_KVM_DEV_CYCLE.md`](AKUMA_KVM_DEV_CYCLE.md) — the groundwork for
  the on-metal track: the agent-in-Akuma / KVM build-test cycle this
  experiment's process section builds on (loop proven 2026-10-23).
