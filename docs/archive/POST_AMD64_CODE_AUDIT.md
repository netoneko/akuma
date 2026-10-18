# Post-amd64 code audit: test-only APIs, unsafe growth, and Firecracker-on-akuma

**Date: 2026-10-17** · branch `main` @ `5a577983` ("tty: name who puts a raw terminal back in
canonical mode")

Four questions, asked after the amd64 port reached bare-metal self-hosting and
the Ryzen box became the daily driver:

1. **Which production functions are only ever called from tests?** The kernel has
   functions with real test coverage and zero production callers — tested code that
   never ships a behaviour. The seeding incident is
   [`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](AMD64_CLAUDE_LOGIN_INPUT_STUCK.md), and the
   unwired functions cluster exactly there (§1.1).
   This audit repeats — fresh tree, fresh method — the exercise of
   [`TRIM_FAT_DEAD_CODE.md`](TRIM_FAT_DEAD_CODE.md) (2026-08-11).
2. **Where is `unsafe` heading, and what keeps it heading the right way?**
3. **What would it take to run Firecracker *on top of* akuma** — akuma as the host
   OS, Firecracker as an akuma userspace process — as a hardening/platform-convergence
   accelerator? *(Scoped amd64-only, §3.3.)*
4. **What to extract/unify into crates next** once the glue's size makes the
   fold-batch pattern impossible to keep applying incrementally (§4).

Measured with [`scripts/cloc_akuma.py`](../../scripts/cloc_akuma.py) (`--rev` / `--vs`
for history), which separates production from test lines the way this repo actually
gates them (including the `no-tests` in-kernel boot-suite gate that a filename-based
counter mis-attributes).

---

## 1. Production functions with only test callers

This repeats the exercise of [`TRIM_FAT_DEAD_CODE.md`](TRIM_FAT_DEAD_CODE.md)
(2026-08-11) on the current tree — fresh method: extract every `pub fn`
(~1,500 across 34 kernel crates), word-search callers with `rg -w` over
`src/ amd64/ crates/ userspace/`, count a hit as production only if the line
is code and precedes the file's first `#[cfg(test)]` marker; everything
surviving (107 → 41 after same-file filtering) re-verified by reading. The
open incident this audit was seeded by is
[`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](AMD64_CLAUDE_LOGIN_INPUT_STUCK.md)
(claude's stdin stalls after idle on the ryzen metal; kernel `a096dc54`
capture, hook `34e1f405`): the bytes sit in the kernel canonical buffer, the
process's epoll is armed, but readiness never reports — leading hypothesis is
that `current_channel()` probes a *different channel* than the thread that
sees the data. The tty findings below are that seam.

### 1.1 The tty stack (the incident area)

- **`TerminalState::push_input` / `read_input`**
  (`akuma-terminal/src/lib.rs:162,176`) — a complete second stdin queue,
  documented as *the* input path ("there is currently no caller — this note is
  for whoever wires one up"). Zero callers anywhere, tests included. Real
  input arrives via `ProcessChannel::write_stdin` + the amd64 console pump,
  which fires `input_waker` directly (`amd64/src/console.rs:385`), bypassing
  `input_buffer` entirely. **A vestigial second view of stdin next to the
  real one is exactly the incident's shape.**
- **`ProcessChannel::is_raw_mode`** (`akuma-exec/src/process/channel.rs:556`)
  — the raw-mode flag is written in production, never read. Raw-mode truth
  exists in three places (channel flag, `TerminalState.is_canonical()`, the
  amd64 console's copy); glue branches on the second. The incident initially
  looked like exactly this bug.
- **`get_process_channel(tid)`**
  (`akuma-exec/src/process/exec.rs:141`) — doc: "Used by the SSH shell to get
  a handle for interrupting a process." Never called. Production uses
  `current_channel()` with the per-slot `get_channel(tid)` fallback the
  incident handoff suspects. **The correct by-tid channel resolver exists;
  production calls something else.**
- `ChannelTable::channel_mut` (`akuma-net-unix/src/lib.rs:1096`) — unused
  accessor, low.

**Status 2026-10-18:** `push_input`/`read_input` and the channel's `is_raw_mode`
were deleted (`55bbb8c0`); `get_process_channel` stays, as §1.5 says.

### 1.2 Cleanup / leak paths

- **`pipe_destroy`** (`akuma-syscalls-glue/src/pipe.rs:482`) — doc names its
  purpose precisely: a spawned child's stdin pipe read-end never closes via
  refcounting; "`waitpid` is what knows the child is gone." The waitpid hook
  that was to call it was never written. **Suspected per-spawn pipe-slot +
  buffer leak on amd64** — worth a live measurement before deleting.
**Status 2026-10-18 — `pipe_destroy` was already obsolete and is deleted, along
with `PipeTable::destroy` and its three tests.** The "suspected per-spawn pipe
leak" is the one this section's author could not see from the code: the hand-managed
spawn pipes it served went away with C2 slice 6 (2026-09-11) — a spawned child's
stdio is one `ProcessChannel` now, and an `Arc` needs no reap (`usermode.rs`,
`struct Spawn`: "There are no pipes"). The audit's own instruction was to prove it
first, and the instrument it asked for now exists: `/proc/health`'s `pipes_live` /
`pipes_high_water` / `pipes_refused` (see §1.3). Measured on QEMU amd64, 80
consecutive ssh exec sessions (the workload that died at ~44 in
`proposals/AMD64_SPAWN_PIPE_LEAK.md`): `pipes_live 1 → 1`, `pipes_high_water 1`,
`pipes_refused 0`. On the trashcan after boot plus a handful of `hget` runs:
`pipes_live 2`, `pipes_high_water 4`, `pipes_refused 0` — healthy, but not a
churn test; the 80-session run was QEMU only.

- `dmesg_clear` (`akuma-kernel-core/src/console.rs:108`) — the amd64 kernel
  wired `SYSLOG_ACTION_CLEAR` to its *own* ring (`serial.rs:162`); the
  kernel-core ring's clear is orphaned and aarch64 has no syslog handler.
  Divergent `dmesg -c` semantics, medium.
- `Pipe::is_write_closed` / `is_readable` (`akuma-pipe/src/lib.rs:111,117`) —
  the EOF/readiness predicates, unused; `amd64/src/pipe.rs` re-tests
  emptiness by hand. Predicate-drift trap, low–medium.

### 1.3 Self-tuning machinery whose adaptive half never runs

`akuma-kacho`'s `WakeBudget::observe_quiet/observe_work` (the staircase that
makes `budget()` adapt) — zero production callers: amd64 reimplemented the
accumulation with its own `quiet_before` counter and calls only the pure
`budget()` (`sched.rs:1127`). The exact drift the shared crate exists to
prevent. Likewise unwired-but-running: BKL `kernel_lock_lost_ticket_recoveries`
/ `stuck_suppressed` health counters, `reclaimed_slot_count`,
`pressure_reclaimed_pages`, `trampoline_bail_count` — four robustness
mechanisms that run **invisibly**: they increment counters no /proc, PSTATS
or dmesg surface ever reads.

**Status 2026-10-18 — wired, as the decision rule said (wire, don't delete):**
`/proc/health`, a file rather than a console line (the console is the channel of
last resort on the metal boxes, and the idle block that prints `[PSTATS]` runs
with the BKL held, where every serial byte is a VM exit). It reports
`bkl_recoveries`, `bkl_lost_ticket_recoveries`, `bkl_stuck_suppressed`,
`bkl_max_wait_spins`, `trampoline_bails`, `pressure_reclaimed_pages` and
`socket_slots_reclaimed` on both kernels, plus the pipe table (amd64: live /
high-water / refused / cap — the pipe leak-vs-demand reading; AArch64: live).
Allocation-free; `akuma-procfs` carries it as `Global::Health` so the tree
simulation walks it; `reference/subsystems/vfs.md` documents the keys. One
counter moved to make that possible: `RECLAIMED_SLOTS` is now
`akuma_net::SOCKET_SLOTS_RECLAIMED`, because the module that wrote it is compiled
out of a rump-only build and `/proc/health` reads it on every build.
**Not done:** `WakeBudget::observe_quiet/observe_work` (the adaptive staircase) —
that is a different problem (amd64 reimplemented the accumulation; the fix is to
adopt the crate's, not to surface a counter).

### 1.4 Tested-but-unwired logic

`MmapRegion::file_page_source` + `page_file_offset` (`akuma-mmap`) — the
modelled file-backed-page seam, tested but never adopted (production fills
re-derive offsets). `is_interface_query` (`akuma-syscalls-net`) — the
dispatch predicate that exists to stop ifreq cmd-table drift between glue and
amd64's `fd.rs`; both consumers hand-match `SIOCG*` numbers instead, and the
amd64 comment admits it mirrors glue. `dac::may_delete` — superseded
piecewise at its call sites; two copies of the unlink rule now drift apart.
`generation_state` (`akuma-exec/src/process/table.rs:852`) — the stale-tid
detector for the `c0002a3e` bug class; doc claims a test asserts on it; there
is no test and no caller.

**Status 2026-10-18:** `generation_state` now has its test (`e2086348`).

### 1.5 Fully dead — built for production, never reached

`translate_current_user_va` (the `BUSYBOX_HASH_MISCOMPUTE` post-mortem
*prescribes* this corruption check; never wired), `map_kernel_block_2mb`,
`remove_lazy_region`/`is_in_lazy_region`, `set_args`/`take_stdout`,
`socket_udp_recv_ready`, the `diag.rs` lock timers ("call this around existing
lock sites" — nobody did).

Verified **fine**, do not "fix": the whole real terminal pipeline
(`translate_output`, `process_canon_input`, `drain_canon_ready`,
`enter/exit_raw_mode`), VINTR→SIGINT, the ext2 audit, DAC enforcement.

**Decision rule** (as in the August doc, sharpened): for duplicate-truth
pairs, **delete the unwired copy and repoint its tests at the wired twin** —
keeping both is the worst option, a drift trap whose tests give false
confidence in a path nothing takes (the `parse_elf64_ehdr` →
`parse_elf64_ehdr_checked` repoint is the worked example). That rule clears
§1.4–1.5 and the tty duplicates `push_input`/`read_input` (tests already read
via `drain_canon_ready`) and `is_raw_mode` (deleting one of the *three* raw-mode
sources of truth). Three deliberate exceptions, wire-don't-delete:

- **§1.3's counters** are the measurement half of machinery that runs; deleting
  them hides the machinery. A PSTATS/procfs surface for them is a day of work
  and the first instrument for the next wedge.
- **`pipe_destroy`** documents a suspected leak; delete only in the commit that
  proves the leak doesn't exist (or wire the waitpid hook).
- **`get_process_channel(tid)`** stays until the channel-resolution incident
  settles — it is currently evidence; if `current_channel()` wins, delete it
  with the incident doc citing it.

### 1.6 Method limits

Text-based: macro-generated identifiers or rename-and-re-export callers would
be missed (top items hand-checked); feature-gated callers in non-default
profiles would read dead (checked for the reported items); leak/cleanup
severity is inferred from code, not from a live-VM measurement.

---

## 2. Unsafe code: growth, retreat, and what keeps it retreating

### 2.1 The timeline

Production code lines vs `unsafe` lines inside `unsafe { }` blocks, sampled across
history (`cloc_akuma.py --rev`, sorted by date):

| date | commit | prod lines | unsafe lines | unsafe % | sites |
|---|---|---:|---:|---:|---:|
| 2025-11-27 | `b6d30390` | 11 | 0 | — | 0 |
| 2025-12-30 | `613bf085` | 5,609 | 217 | 3.9% | 81 |
| 2026-01-30 | `fd850670` | 18,266 | 1,452 | 7.9% | 318 |
| 2026-02-17 | `ab4dd402` | 18,093 | 1,308 | 7.2% | 281 |
| 2026-03-04 | `12548d47` | 24,690 | 1,772 | 7.2% | 423 |
| 2026-03-28 | `50dc6afd` | 33,060 | 2,237 | 6.8% | 553 |
| 2026-06-06 | `2a9adde1` | 36,663 | 2,535 | 6.9% | 636 |
| 2026-07-15 | `1145d4b8` | 43,811 | 2,852 | 6.5% | 784 |
| 2026-08-11 | `0eb61454` | 38,621 | 2,837 | 7.3% | 747 |
| 2026-08-24 | `bf0aa54b` | 45,464 | 1,942 | 4.3% | 525 |
| 2026-09-04 | `cda365cc` | 56,068 | 1,512 | 2.7% | 388 |
| 2026-09-13 | `7a815e6d` | 65,076 | 1,758 | 2.7% | 469 |
| 2026-10-01 | `1fa42be0` | 68,877 | 1,757 | 2.6% | 468 |
| **2026-10-17** | working tree | **82,860** | **1,760** | **2.1%** | **469** |

The story the table tells:

- **The peak was real.** By mid-July 2026 the kernel carried ~2,850 lines of
  `unsafe` — 6.5–7.3 % of production code, and growing in lockstep with the code.
  At that trajectory, "it's a kernel, there's unsafe" would have become permanent.
- **The reversal coincides with the amd64 port.** Between July and September the
  absolute `unsafe` count fell by ~1,100 lines *while production code nearly
  doubled* (43.8k → 65k). This is not one edit; it is the fat-trimming campaign
  (deleting test-only and never-called code — see
  [`TRIM_FAT_DEAD_CODE.md`](TRIM_FAT_DEAD_CODE.md) and the rest of `TRIM_FAT_*`)
  plus the crate-extraction fold batches
  ([`AKUMA_AMD64_4B_FOLD_*`](AKUMA_AMD64_4B_PREREQUISITES.md)), which moved
  subsystems into standalone crates where `#![forbid(unsafe_code)]` could be
  applied per crate.
- **The plateau is the current state.** Sept 13 → today: prod +18k lines,
  `unsafe` +2 lines. Growth of `unsafe` has been decoupled from growth of code.

### 2.2 Where the remaining `unsafe` lives

Today: **86 crates, 66 carry `#![forbid(unsafe_code)]`** (77 %), **23 contain
`unsafe`**, 469 `unsafe` sites, 1,760 lines (2.1 % of production code; 96.7 % of
production code is outside `unsafe` blocks).

| crate | prod lines | unsafe lines | sites | safe % |
|---|---:|---:|---:|---:|
| `akuma-mmu` | 2,930 | 549 | 105 | 81.3 % |
| `akuma-threading` | 3,763 | 311 | 65 | 91.7 % |
| `akuma-alloc` | 893 | 271 | 24 | 69.7 % |
| `akuma-exceptions` | 3,126 | 169 | 83 | 94.6 % |
| `akuma-cpu` | 488 | 167 | 50 | 65.8 % |
| `akuma-virtio` | 1,073 | 67 | 38 | 93.8 % |
| `akuma-net-nic` | 1,977 | 49 | 34 | 97.5 % |
| `akuma-user-access` | 438 | 40 | 20 | 90.9 % |
| `akuma-el0-entry` | 102 | 36 | 4 | 64.7 % |
| `akuma-psci` | 67 | 24 | 2 | 64.2 % |
| `akuma-pmm` | 1,338 | 18 | 3 | 98.7 % |
| everything else | — | 149 | ~40 | — |

Half of all remaining `unsafe` sits in five crates (`mmu`, `threading`, `alloc`,
`exceptions`, `cpu`) — and that is *where it should be*: page-table writes, context
switches, the allocator, trap entry. The problem is not the location, it is that
those five have no enforcement boundary yet.

### 2.3 What can be done about it

What worked (keep doing):

1. **Extraction-first.** The single biggest `unsafe` reduction came from moving a
   subsystem into its own crate and forbidding `unsafe` everywhere *around* it
   (`akuma-pmm`: 3 unsafe sites in 1,338 lines, everything else `forbid`).
   A `#![forbid(unsafe_code)]` crate wall turns "please don't write unsafe here"
   into a compiler guarantee, and the 66/86 ratio shows the method scales.
2. **Delete instead of fence.** The July peak was inflated by test-only and
   never-called code carrying `unsafe` for paths nothing took. The dead-code
   sweeps shrank `unsafe` without touching a live line.
3. **Keep the counter honest.** `cloc_akuma.py --self-test` exists because
   `grep -c unsafe` is wrong in both directions (attributes, raw strings,
   `unsafe fn` bodies). Quote `unsafe_sites` next to `unsafe_code` — sites
   measure blast radius, lines measure volume.

What to do next, in order of value per effort:

1. **`unsafe` RFC per big-five crate.** For `mmu`, `threading`, `alloc`,
   `exceptions`, `cpu`: write down the invariant each `unsafe` block relies on,
   and shrink the `unsafe` block to the smallest statement that needs it
   (safe wrappers per operation, `unsafe` only inside the wrapper). The
   `safe %` column is the score: `akuma-alloc` at 69.7 % and `akuma-cpu` at
   65.8 % are the outliers.
2. **A lint gate, not goodwill.** Add a CI check (trivial on top of
   `cloc_akuma.py --json`) that fails when `unsafe_sites` grows in a crate
   without a matching `crates/<name>/UNSAFE.md` entry. The Sept→Oct plateau
   happened by discipline while the amd64 port had full attention; a gate makes
   it survive the next port.
3. **Target: every new crate starts `forbid`.** This is already close to the
   de-facto rule (15 of the last fold-batch crates forbid). Make it a stated
   rule in CLAUDE.md: `forbid(unsafe_code)` is the default, an `unsafe`
   allowance is a reviewed exception.
4. **`unsafe` opsemis reviews on the amd64 side.** The amd64 kernel is newer and
   its per-arch `unsafe` (ring transitions, `fxsave`, io ports) was written fast.
   A focused pass auditing each amd64 `unsafe` block against its arch doc
   (SDG) — the way the aarch64 mmu was audited during the 4B folds — would
   retire the "newest code, least audited" asymmetry.

---

## 3. Firecracker *on* akuma (akuma as host): effort estimate

First, a clarification the docs make easy to trip over: **akuma already runs
*under* Firecracker on both architectures** — aarch64 on Graviton
([`AKUMA_FIRECRACKER_TERRAFORM.md`](AKUMA_FIRECRACKER_TERRAFORM.md)) and amd64 on
the Ryzen KVM host since 2026-09-03
([`AKUMA_FIRECRACKER_AMD64.md`](AKUMA_FIRECRACKER_AMD64.md)). The question here is
the inverse and much larger one: akuma as the host kernel, Firecracker as a
process on it, microVMs nested beneath.

### 3.1 What exists today (measured)

- **No hypervisor code at all.** No EL2/stage-2/VHE code on aarch64
  (`akuma-psci` only *talks to* a hypervisor over `hvc`); no VT-x/VMCS/EPT/SVM
  anywhere on amd64. The `crates/akuma-firecracker` crate is akuma's *guest-side*
  device map, not a VMM.
- **~85 % of the non-hypervisor syscall surface Firecracker needs already
  exists**: anon+file mmap, `eventfd`, `timerfd`, epoll, futex, threads
  (`CLONE_VM/THREAD/FILES`), pidfd, AF_UNIX, TCP/IP, static+dynamic musl ELF
  loading. Firecracker's static musl binary would *load* under akuma.
- **What's missing**: `/dev/kvm` (obviously), `memfd_create` (exists as a string
  in a name table only — Firecracker backs guest RAM with memfd), mmap-on-fd,
  `signalfd` (not found), TUN/TAP, vhost, a char-device ioctl framework
  (`sys_ioctl` is a whitelist over PTYs and ifreqs), seccomp.

### 3.2 The three paths

| path | what it is | size |
|---|---|---|
| (a) KVM-compatible in-kernel shim | implement the `/dev/kvm` ioctl ABI (≈40 ioctls, `KVM_GET_API_VERSION == 12` exact-match, errno-exact failures — Firecracker aborts rather than degrades) backed by a greenfield type-2 hypervisor. **Scoped amd64-only (§3.3): VMX+EPT on the HP/Ryzen boxes** | **~1–1.5 eng-years** (was 1.5–3 dual-arch) |
| (b) fork rust-vmm onto a native "akuma-vmm" API | replaces `kvm-ioctls`, but forks Firecracker, which defeats the purpose (no longer the official binary) | L, and mostly pointless |
| (c) **akuma-native microVM runner** | skip Firecracker: a small kernel module + runner that reuses what akuma already owns — `akuma-pmm` (frames), `akuma-user-space`/`akuma-mmap` (address-space construction — the kernel already builds page tables per process), `akuma-cow`, `akuma-virtio` (device models; akuma has been booted with exactly Firecracker's virtio-mmio layout). **Scoped amd64-only (§3.3)** | **M — 4–8 months** to "one Linux microVM boots" (was 6–12 dual-arch) |

### 3.3 Scoping: amd64-only

Decided 2026-10-17: the hypervisor work targets **amd64 only for now**, no
dual-arch abstraction layer. The cut is bigger than it looks:

- **The test boxes are amd64.** Both bare-metal hosts are virtualization-capable
  (HP i5-4460: VT-x + EPT; Ryzen 7 8845HS: SVM + nested NPT) and run the amd64
  kernel at self-hosting maturity. There is no bare-metal aarch64 akuma host at
  all — akuma aarch64 only exists under QEMU/Graviton, i.e. always beneath
  someone else's hypervisor, so an EL2 port would start life as nested
  virtualization with no dev box for it. amd64-only is not just cheaper, it is
  the only arch with a real host to target.
- **Deleted by the cut:** EL2/stage-2/VTTBR bring-up, GICv3 distributor/redis
  emulation, and the temptation of a portable "hv" abstraction layer.
- **Kept (and still hard):** VMCS/EPT management, vmentry/vmexit save-restore
  (amd64 doesn't even do `fxsave`/`xrstor` for its *own* context switches yet —
  that becomes a prerequisite, not a parity item), and interrupt injection. On
  the i5-4460 there is no APICv (Haswell predates it), so virtual interrupts mean
  an in-hypervisor APIC man-in-the-middle against akuma's existing LAPIC/IOAPIC
  drivers; the Ryzen's AVIC/posted-interrupts can come later as an optimization.
- **Revised sizing:** path (c) native runner **4–8 months** to "one Linux
  microVM boots" (was 6–12); path (a) KVM-compatible shim **~1–1.5 eng-years**
  (was 1.5–3). A ~30–40 % cut, essentially all from deleting the ARM side.
- **Deferred, not cancelled:** aarch64 becomes M-later, gated on owning any
  bare-metal aarch64 hardware at all (or accepting nested-under-KVM development,
  which is its own project).

### 3.4 Recommendation and milestone path

Path (c) first, amd64-only; path (a)'s ioctl layer later, if wanted, on top of
(c)'s hypervisor core. The milestones:

- **M0 — amd64 context-switch hygiene (S):** `fxsave`/`xrstor` on context
  switch. Needed by the hypervisor (guest FPU state) and already listed as an
  amd64 parity gap in §"not broken vs QEMU" — doing it first means M1 doesn't
  start on a kernel that loses FPU state.
- **M1 — hypervisor seed (S→M):** VMX + EPT on the HP/Ryzen boxes: one guest
  vCPU in a VMCS, EPT mapping donated frames, hosted by a kernel thread,
  exits handled in-kernel. Exit criterion: *akuma boots akuma*.
- **M2 — native runner (M):** guest gets a Firecracker-shaped boot handoff
  (PVH `hvm_start_info`, the protocol akuma amd64 itself boots under — the
  guest side needs no FDT work at all on amd64); virtio-blk/net from akuma's
  own device models. Exit criterion: a Linux microVM reaches userspace under
  akuma. Free validation: akuma's own boot suite as the guest image.
- **M3 — `/dev/kvm`-shaped surface (L):** char-device node, ioctl subset,
  `memfd`, eventfd-backed irqfd/ioeventfd.
- **M4 — Firecracker loads (L):** official binary, `--no-seccomp`, single-vCPU
  `--no-api` microVM to userspace.
- **M5 — parity (XL):** SMP vCPUs (collides with the BKL design — per-VM exits
  under a global lock serialize badly; the runtime-toggleable fine-grained
  phases exist but would be stressed), real seccomp, TUN/TAP or smoltcp-backed
  virtio-net bridging.

Top risks, in order: (1) the hypervisor core is greenfield on a BKL kernel;
(2) Firecracker's hard KVM-version/capability checks leave no graceful-degradation
path; (3) `memfd`+mmap-on-fd are real VFS work, not shims; (4) irqfd means
kernel-internal wakeup wiring akuma's userspace-token eventfd doesn't provide;
(5) nested KVM-on-KVM on the Ryzen dev box is untested for the dev loop itself.

**Is it worth it for hardening?** The honest comparison: the same convergence
benefit (fast native-virt boots of the standard suite, real-silicon exposure
QEMU can't express, snapshot/restore for fuzzing) is *already banked* by running
akuma under Firecracker on the Ryzen host — that rig exists and is used for the
731-test image and the Chromium/kami work. Firecracker-on-akuma buys a different
thing: akuma's own kernel bugs surfacing through a second, adversarial consumer
(`memfd`, irqfd, TUN/TAP, heavy anonymous memory, many short-lived processes) —
excellent hardening pressure, but it is hardening *of akuma*, at hypervisor-scale
cost. Path (c) M1–M2 is the part with a good value/effort ratio; the rest should
wait until the amd64/aarch64 convergence table stops moving.

---

## 4. Extraction and unification candidates

Where the crate fold should go next, grounded in the current tree
(`scripts/cloc_akuma.py --json`, sizes in production lines).

### 4.1 Dissolve `akuma-syscalls-glue` — the fat one

The 4B fold batches established the pattern: each syscall family gets a **pure
"deciding half" crate** (e.g. `akuma-syscalls-mem` — plans the mapping, no
locks, no page-table edits, two leaf dependencies) and the kernel keeps the
**acting half**. The deciding halves exist; the acting halves never left the
19,421-line glue monolith:

| glue module | lines | deciding-half counterpart |
|---|---:|---|
| `fs.rs` | 3,758 | none |
| `net.rs` | 2,225 | `syscalls-net` (380) |
| `proc.rs` | 2,109 | none |
| `poll.rs` | 1,796 | `syscalls-poll` (1,832) |
| `mem.rs` | 1,614 | `syscalls-mem` (926) |
| `unixsock.rs` | 1,346 | none |
| `sync.rs` | 947 | `syscalls-sync` (1,360) |
| `pty.rs` + `term.rs` | 1,384 | none |

`mem`, `poll`, `sync`, `net` are split half-and-half: the pure part is extracted
and host-tested, the acting part still lives in glue. The move is per-domain
acting crates (`akuma-syscalls-fs`, `akuma-syscalls-proc`,
`akuma-syscalls-unixsock`) sitting next to their decision crates, until glue is
a thin dispatch table and then gone. `fs.rs` first — it is where the amd64
allocation-audit debt (`AKUMA_FIRECRACKER_AMD64.md` §3.24.5) is concentrated.

### 4.2 One `akuma-tty`

The terminal stack is smeared across `akuma-terminal`, `akuma-pty`,
`glue/src/term.rs`, `glue/src/pty.rs`, with fbcon/uart at the edges. The line
discipline / canonical-mode state machine — where the recent raw-terminal bug
lived (commit `5a577983`) — has no single home. Extract one crate with the
discipline as a pure, host-testable core (the `syscalls-mem` pattern), so that
"who puts a raw terminal back in canonical mode" is answerable by grep.

### 4.3 The fd table

The port log lists "single-kernel-fd-table" as an amd64 parity gap.
`akuma-slot-table` already provides the id-recycling machinery. Extracting the
fd table onto it closes a convergence blocker and a spawn-ceiling
(`AKUMA_FIRECRACKER_AMD64.md` §3.24.4) at once, and is a prerequisite for the
microVM runner: a guest process's fd table should not be the host kernel's.

### 4.4 `mmu` safe-wrapper facade

`akuma-mmu` holds 549 `unsafe` lines — a third of the kernel's total (§2.2).
It will never carry `forbid`, but the `akuma-mmap` / `akuma-user-space`
boundary is where a safe facade belongs: map/protect/translate as checked
wrappers, raw `unsafe` shrinking to the leaf writes. This is §2.3-1 made
concrete.

### 4.5 Smaller pairs

- **`akuma-locks-rw` vs `akuma-locks-rw-cell`** — two crates, ~210 lines each;
  merge, or document why the split survives.
- **`fbcon`'s renderer** — 3,432 lines, 2,501 of them test code; the
  font/glyph/subpixel part is pure and host-testable, the driver part is tiny.
  The split `akuma-ext2` already did right (3,555 prod / 2,601 test, separate
  concerns, `forbid`).
- **`akuma-exec` (5,972) vs `akuma-exec-core`** — exec is the biggest non-glue
  crate; ELF image loading arguably already belongs to `akuma-elf`.

### 4.6 amd64-specific: the unsafe is where the newest code is

`amd64/` is 28 % of production code (23,054 lines) but holds **61 % of all
`unsafe` lines (1,078 of 1,760) and 73 % of all `unsafe` sites (343 of 469)**.
The newest kernel carries the least-audited `unsafe`. Per-file worst offenders:
`sched.rs` (189 lines / 33 sites — the context switch, including the missing
`fxsave`), `usermode.rs` (157/31), `idt.rs` (83/39), `lapic.rs` (82/16),
`multiboot2.rs` (66/20), `smp.rs` (65/17), `paging.rs`+`mm.rs` (63/34).

Four tiers of work:

1. **Trivial lifts into `forbid` crates (S each).** `port.rs` — 32 lines, 20
   `unsafe` (raw `in`/`out`) — becomes a ~50-line `akuma-port` of safe wrappers.
   `uaccess.rs` (copyin/copyout, 14 sites) is the textbook safe-wrapper
   candidate: validate+copy once, callers get safe functions. `gdt.rs` (93
   lines, 37 `unsafe`) likewise. The aarch64 twin (`akuma-uart`: 53 lines, 6
   sites) shows the target shape. Three tiny crates, ~75 sites encapsulated.
**Status 2026-10-18 — `port.rs` done, and not the way tier 1 wrote it.** The
recipe was "a ~50-line `akuma-port` of safe wrappers", i.e. a safe `outb`. That is
the wrong shape: a safe `outb(port, val)` lets any safe code reprogram the PICs or
reset the machine with a typo — the reason `akuma-cpu` refuses `msr ttbr0_el1` and
`smc` (CLAUDE.md records it three times). `crates/akuma-port` has **no function
that takes a port number**; it exposes devices (`COM1`, `CMOS`, `PIC`, `PIT`,
`I8042`, `PCI_CONFIG`, `RESET_CONTROL`, `DEBUGCON`), each of which can only reach
its own range, so "this port is the i8042" is asserted once, by a type, instead of
at every call site. The four `in`/`out` instructions are the crate's only `unsafe`
(`raw.rs`, behind `#![deny(unsafe_code)]` and one `#[allow]`). Host tests run
against a fake bus and assert each device's access sequence, that it stays inside
its range, and that the registry has no overlaps except the named `0xCF9`-inside-
`0xCF8` alias. Moved: `serial`, `reboot`, `clock`, `lapic`, `kbd`, `pci`,
`multiboot2`; `amd64/src/port.rs` is deleted. Measured with
`cloc_akuma.py src crates amd64`: `unsafe` 812 -> 789 sites, 2838 -> 2745 lines
tree-wide (amd64 alone 342 -> 315 sites). **Still open from tier 1:** `gdt.rs`
(the invariant is the `sysret`/`syscall` selector order, so a wrapper has to encode
the order, not hide the `asm!`) and `uaccess.rs` (not mechanical: `stac`/`clac`,
CR0/CR4, and a copy routine whose fault fixup is welded to the page-fault handler;
only the validation arithmetic is a clean extraction).
Verified on QEMU (boot suite 814/0) and on the trashcan (kernel `37c3794f`,
`no-tests`): 15 PCI functions enumerate, PIT calibration (`62374 counts per
10000us`), the CMOS read at boot and the RTC write-back after SNTP, the console,
and a `reboot -f` issued *by the new kernel* all work. The reset came back, but
the log of the kernel that issued it is gone, so whether `0xCF9` or the i8042
fallback did it is not recorded.

2. **Finish the fold on drivers that already have pure twins.** The xHCI split
   is the model — `crates/akuma-xhci` is 2,178 lines of pure layout/bit math
   under `#![forbid(unsafe_code)]` while `amd64/src/xhci.rs` (2,548 lines) is
   the acting half. Same shape for hda (1,700 vs 1,138), rtw89 (3,809 vs
   865+1,532), touchpad (2,386 vs 786). The remaining lift: MMIO register
   access behind wrappers, so acting halves shrink toward interrupt + DMA
   management.
3. **Arch-generic machinery as crates (M — and hypervisor prep).** Extracting
   `akuma-paging` (a page-table walker with a trait over PTE flag semantics)
   is *M1 groundwork*: EPT is the same data structure with different flags, and
   the microVM runner reuses the walker directly. `lapic.rs` wants to sit next
   to the aarch64 timer/GIC crates. And the split between `akuma-exceptions`
   (3,126 lines, exists) and `amd64/src/idt.rs` (2,482 lines, still 83
   `unsafe`) deserves an audit — the boundary is undefined.
4. **`amd64/src/dns.rs` — the one genuine duplication, and it is a live §1
   hit.** The shared, tested `akuma_net::dns::resolve_host_blocking` hung on
   its *first-ever amd64 caller* (SNTP bootstrap, 2026-09-05, 90 s, no
   timeout); `dns.rs` is the hand-rolled A-record client routed around it,
   over UDP primitives `clock.rs` already trusted. The unexercised-bug bug,
   caught by having a second kernel — tracked in
   `AKUMA_FIRECRACKER_AMD64.md` §3.30, and adjacent to the §3.30.4 netpoll /
   apk-bulk-hang suspect. Fix the shared path, delete the bypass.

**How the driver "duplication" question resolves: it mostly doesn't exist.**
The amd64 kernel was built stage-by-stage *wiring the shared crates in
unmodified* wherever the code decides anything (`blk.rs`: "the first time it
runs a driver from the shared crates"; `sock.rs`: "Wiring, not implementation";
`futex.rs`: "The decisions are not here"). Each amd64/src file that pairs with
a crate is the deliberate **acting half** — `xhci.rs` is "the MMIO / DMA half
of the xHCI driver, over `akuma_xhci`" — and both halves are always used
together: the crate is compiled into the amd64 kernel, the acting half is what
only the amd64 kernel can supply. Nothing is dead; there is no second copy to
delete. What *is* per-arch and unaudited is the acting halves themselves — the
MMIO writes, DMA `.bss` statics and ring transitions that the pure half cannot
contain — which is precisely where §2's `unsafe` concentration sits, and why
tier 2 of this list is extraction-plus-facade rather than dedup.

**Status 2026-10-18 — tier 4 done; the premise was half wrong.** The shared
`resolve_host_blocking` does not hang today: boot-time SNTP resolves through
`smoltcp_net::dns_query` in a QEMU amd64 boot, and so does syscall 300 after the
netpoll daemon is alive. The 2026-09-05 hang was most likely the frozen clock
(§3.30.3's `IF=0`: `dns_query`'s only timeout is `uptime_us`, which did not move)
and has had no reproduction since. What `dns.rs` had that the shared path did
**not** was behaviour, and it was the reason deletion was not a one-liner:
smoltcp's DNS socket fails on the first `NXDOMAIN` with one configured server,
while `dns.rs` walked a resolver list on a timeout *or* an answer without a record
— a measured case (the HP box's uplink; and again on QEMU slirp, which
`NXDOMAIN`s `example.com` while `1.1.1.1` answers). So the fix moved the
*client*, not the call site: `crates/akuma-dns` (pure, `forbid(unsafe_code)`, 22
host tests where `dns.rs` had none) holds the wire format and the walk/retransmit
loop over effects (the `akuma-sntp` shape); `akuma_net::dns::resolve_ipv4` wires
it to the UDP sockets and is now the resolver for every target. `amd64/src/dns.rs`,
`smoltcp_net::dns_query` and smoltcp's DNS socket (one socket slot) are deleted.
**Policy stayed per-kernel:** amd64 sets fallbacks (1.1.1.1, 8.8.8.8) at net
bring-up; AArch64 sets none, so its behaviour is unchanged — an `NXDOMAIN` from the
configured resolver is final there. Side effect worth naming: the amd64 call now
waits through `blocking_relax` (drops the BKL while parked) where `dns.rs` called
`yield_now` directly. Verified on QEMU amd64 (boot SNTP; `hget example.com` walks
`10.0.2.3 → 1.1.1.1` in 0.13 s; a nonexistent name fails in under a second) and
AArch64/HVF (resolves; fails fast), and **on the trashcan** (kernel `8436ea8f`,
`no-tests`, installed over `/boot/akuma-amd64`, rebooted): boot SNTP synced via
the shared client, `hget` of `example.com` over http and https both succeed in
0.05 s, a nonexistent name fails in 0.05 s with `1.1.1.1` and `8.8.8.8` each
reporting `no record`, and `/proc/health` reads back on real hardware. The box's
own uplink resolver is `1.1.1.1`, so the `NXDOMAIN` walk itself (the QEMU slirp
case) was not exercised on metal.

Ordering: tier 1 immediately (days); tier 4's `dns.rs` fix rides the §3.30
bug; tier 3's `akuma-paging` before §3.4 M1 so the hypervisor starts on an
extracted, tested walker instead of a fork of `amd64/src/paging.rs`.

**Next fold: platform crates for the acting halves.** The pure/acting split
already works *within* one kernel; the natural extension is giving each
platform's acting halves their own crates — `akuma-amd64-core` (or per-device:
`akuma-xhci-amd64`, `akuma-lapic-amd64`, …) alongside the aarch64 equivalents —
following the precedents that already exist: `akuma-ryzen-amd64` (machine
description, `forbid(unsafe_code)`, host-tested against captured hardware) and
`akuma-net-nic`'s rtl8169 glue ("the same discipline its acting half keeps").
What that buys: (a) the DMA/aliasing contract each acting half states in
comments becomes a crate boundary with a doc header rustdoc can't lose;
(b) convergence accounting becomes mechanical — two platform crates with the
same trait surface are diffable, and unpinned divergence (the CLAUDE.md rule)
shows up as a signature change instead of a behaviour change; (c) the
platform-crate unsafe shrinks toward the leaf MMIO, which is the §2.3
end-state. Not urgent for files that just grew this structure (xhci, nvme);
worth doing when a file next gets touched for §1/§4 reasons anyway.

### 4.7 Tie-breaker rule

The test the fold batches implicitly used: *extract what can be pure and
host-tested; leave acting code until it has a second caller.* That criterion
is about to change — the two follow-on experiments (a minimal self-hosting C
kernel run under Firecracker on the Linux host, and the native microVM runner
of §3.4) are both prospective consumers of ext2, the virtio device models, the
tty core, and the ELF loader. If either becomes real, §4.1–4.2 stop being
hygiene and become shared infrastructure.

---

## Appendix: method

- Test-vs-production split and `unsafe` counts: `scripts/cloc_akuma.py --json`
  (`--rev` for history; the script's docstring documents why filename-based
  counters mis-attribute this repo's inline `#[cfg(test)]` modules after crate
  extraction, and `--self-test` checks the `unsafe` lexer).
- History sampled every ~250 commits of `git log --all` plus the working tree.
- Dead-in-production findings: manual caller analysis (grep + read), seeded by
  the terminal/tty stack, extended across the highest-traffic kernel crates;
  see §1 for per-finding evidence.
- Firecracker-on-akuma: syscall-surface inventory against Firecracker v1.16.1's
  known host requirements, cross-checked against
  `docs/reference/firecracker*/` (measured against v1.16.1) and the port logs.
