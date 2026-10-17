# Post-amd64 code audit: test-only APIs, unsafe growth, and Firecracker-on-akuma

**Date: 2026-10-17** · branch `main` @ `5a577983` ("tty: name who puts a raw terminal back in
canonical mode")

Three questions, asked after the amd64 port reached bare-metal self-hosting and
the Ryzen box became the daily driver:

1. **Which production functions are only ever called from tests?** The kernel has
   functions with real test coverage and zero production callers — tested code that
   never ships a behaviour, which is exactly the shape of the recent terminal bug
   (a path that existed, was exercised by tests, and no runtime caller ever took).
   This audit repeats — fresh tree, fresh method — the exercise of
   [`TRIM_FAT_DEAD_CODE.md`](TRIM_FAT_DEAD_CODE.md) (2026-08-11).
2. **Where is `unsafe` heading, and what keeps it heading the right way?**
3. **What would it take to run Firecracker *on top of* akuma** — akuma as the host
   OS, Firecracker as an akuma userspace process — as a hardening/platform-convergence
   accelerator?

Measured with [`scripts/cloc_akuma.py`](../../scripts/cloc_akuma.py) (`--rev` / `--vs`
for history), which separates production from test lines the way this repo actually
gates them (including the `no-tests` in-kernel boot-suite gate that a filename-based
counter mis-attributes).

---

## 1. Production functions with only test callers

<!-- SECTION_DEAD_CODE -->

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
