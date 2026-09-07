# amd64: the unlock tree to self-hosting

**Date:** 2026-09-07
**Scope:** the dependency order among the amd64 work items — scheduler port,
`akuma-mmap` adoption, the `akuma-mmu` x86 surface, and the `usermode.rs` fold —
as they stand one day after the survey
(`docs/archive/AKUMA_AMD64_STREAMLINING.md`).
**Status:** plan, with measurements taken 2026-09-07.

> **B1 and B2 are DONE (2026-09-07)** — `docs/archive/AKUMA_AMD64_MMAP_REGIONS.md`.
> `amd64/src/mm.rs` is no longer a bump allocator: `akuma-mmap` holds the region
> table (`Process::regions`, per address space, under its own lock),
> `PteProt::from_region` is the crate's x86 backend with all six `Prot::ALL`
> variants pinned bit-for-bit, and the `#PF` handler demand-pages out of the
> region list. `MAX_MAPPING`, "never lazy", the global `NEXT_VA` bump and the
> `MAP_FIXED`/`mprotect` refusals are all gone; file-backed `mmap` is the one
> refusal left, and it needs a page cache.
>
> Two latent bugs fell out: a `fork` child leaked **every page it `mmap`ed after
> forking** (in neither the ledger nor the bump-window walk), and `munmap` could
> double-free a loader page against teardown. Both are fixed by giving every
> anonymous frame one owner — the frame ledger — and deleting the second path.
>
> Verified QEMU 323/0 (`SMP=1`) and 332/0 (`SMP=4`), Firecracker 322/0
> (`SMP=4`), and by the `c_stress` memory probes, which now build for x86_64:
> `mmap_stress`, `cowstale`, `madvshared` and `shmanon` pass, and **no remaining
> failure is a memory-mapping defect** — the six are `pread64`, `mremap`,
> `MADV_FREE`, `/proc/self/smaps`, file-backed `mmap` and, twice, the absence of
> signal delivery. `shmanon` was the one real find: `MAP_SHARED|MAP_ANONYMOUS`
> was recorded and then ignored by `fork`.
>
> **Not done from the B1 box:** file-backed mappings. B3 (widen the x86
> `UserAddressSpace`) is unblocked — its probes are region queries and the
> regions exist now.

> **The six memory gaps are CLOSED (2026-09-07, later the same day)** —
> `docs/archive/AKUMA_AMD64_MEMORY_CLOSEOUT.md`. `pread64`, `madvise` (including a
> real `MADV_DONTNEED`), `/proc/<pid>/{maps,statm}` plus the `access`-vs-`open`
> bug behind them, `mremap`, and file-backed `MAP_PRIVATE` `mmap` all landed, in
> that order, each verified before the next was started.
>
> **The probes went 4/10 → 8/10 on both rigs**, and 8/10 is the ceiling: the two
> that remain (`mprotectlb`, `eager_mprotect_probe`) need signal delivery, which
> is **A2**. Boot self-tests +34 on every rig and core count — qemu 323→**357**
> (`SMP=1`) and 332→**366** (`SMP=4`), firecracker 313→**347** and 322→**356**.
>
> **The B trunk is unblocked, not finished.** What landed for file mappings is
> not a page cache: every mapper gets its own copy of every page and a file
> mapping is always eager, which is the state the AArch64 kernel was in before
> `src/file_page_cache.rs`. Writable `MAP_SHARED` on a file is still `ENOSYS`,
> because that is the half where the sharing *is* the semantics. `akuma-fpcache`
> is the obvious thing to reach for next; item **D**'s "file-backed + lazy mmap"
> row wants the lazy half too.
>
> `akuma-syscalls-abi` gained `Pread64`/`Madvise`/`Mremap` and `akuma-procfs`
> gained `render_statm`/`render_maps_line` — both purely additive (190
> insertions, 0 deletions), and `src/` was not touched.

> **A1 and A2 are DONE (2026-09-07)** — `docs/archive/AKUMA_AMD64_BLOCKING.md`.
> `amd64/src/sched.rs` no longer contains a scheduler: `akuma-threading` is the
> scheduler on both architectures, and what is left is the machine effects
> (`CR3`, TSS trap stack, FS/GS bases, `fxsave`, BKL depth, per-core current
> slot) registered as `X86ArchHooks`. Pipes, `futex` and `wait4` park instead of
> polling. Verified QEMU 295/0 (`SMP=1`) and 304/0 (`SMP=4`), Firecracker 285/0
> and 294/0, **bare metal 297/0 at `SMP=4`**, and aarch64 proven unchanged
> against `main` (307/0 on the same accelerator).
>
> **Not done from the A1 box:** `thread.rs` (391 lines) and the `smp.rs`
> ticket-lock BKL → `akuma-bkl`. Both still stand.
>
> A side effect worth knowing about: consolidating the two boot paths
> (`boot::early_init` + `boot::self_tests`) found that the multiboot2 path had
> **never run seven of the PVH path's tests**, including the whole
> process-lifecycle suite. Bare metal went **262 → 297 checks** as a result.
> Both paths also start `init` now whether or not the suite passed — one flaky
> check used to leave the headless box with no sshd.

> **B3 is DONE and THE GATE IS GREEN (2026-09-07, later still)** —
> `docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md`.
> `cargo check -p akuma-syscalls-glue --target x86_64-unknown-none` passes. The
> x86 `UserAddressSpace` went **6 public methods to 38**: the `akuma-user-space`
> ledger, the scalar identity (`ttbr0`/`l0_phys`/`asid`/`is_shared`), the
> mapping and permission surface, and `impl UserPages`. Everything above the
> gate in the tree below is now deletions from `amd64/src`.
>
> The `u64`-vs-`Prot` mismatch the box below does not mention was the substance:
> `akuma-exec` passes AArch64 PTE flag words, and the x86 walker cannot read
> them. `akuma_mmu::user_flags::from_pte` decodes them to a neutral `Prot`
> first — total, fail-closed, and exactly invertible over `to_pte`. The full §1
> `Prot` migration (59 call sites, plus `LazyRegion`'s `0_u64` sentinel) stays
> undone.
>
> **A silent per-architecture type substitution, found by the gate:** the x86
> block defined `pub struct Prot`, which **shadowed** the neutral
> `akuma_mmap::Prot` that `pub use types::*` re-exports. `akuma_mmu::Prot` meant
> a region record on one architecture and a page-table encoding on the other.
> `akuma-syscalls-glue` is the first shared crate to name the type, and it
> failed to compile — the lucky outcome. Renamed `PteProt`, matching
> `amd64/src/paging.rs`'s own.
>
> **And a real leak, found by running rather than reading:** `map_page` — the
> walk that allocates the first three tables of every address space — used the
> untracked walker, so those frames were unreachable on teardown. The ledger
> parameter was `Option<&FrameLedger>`; it is mandatory now. Third frame-ownership
> bug on this target found by a live probe, after B1's two.
>
> Verified QEMU **405/0** (`SMP=1`, was 357) and **414/0** (`SMP=4`, was 366) —
> +48 both, one new suite (`amd64/src/uas.rs`) and nothing else moved — and on
> **bare metal, 407/0 with all 48 `uas:` checks passing** (HP 500-502nj, RAM
> image). aarch64 proven unchanged by section compare against `HEAD`: `.text`,
> `.rodata` and `.data` **byte-identical**.
>
> **C1 has a hand-off prompt:** `proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md`.
> It carries a blocker this chart does not show — `akuma-syscalls-glue`
> dispatches on **asm-generic/AArch64** numbers (191 constants, zero
> `#[cfg(target_arch)]`) and amd64 speaks **x86_64** ones. Handing glue an
> x86_64 number is a wrong answer, not a compile error, and deciding that
> vocabulary is C1 step 1.

> **C1 steps 1 and 2 are DONE (2026-09-07, later still)** —
> `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`. The two syscall-number
> vocabularies now meet in one place: `akuma-syscalls-abi::Syscall` went **36
> variants to 80**, generated from a single `syscall_table!` row per call so the
> enum, both decodes, both encodes and `ALL` cannot drift, and
> `amd64/src/usermode.rs` decodes through it. **`akuma-syscalls-glue` was not
> touched** — the AArch64 kernel's `.text`, `.rodata` and `.data` are proven
> byte-identical against `HEAD`.
>
> The `cfg`-the-`nr`-table shape was rejected for a reason worth carrying:
> **`cfg!(target_arch)` resolves to the *host* under `cargo test`**, so on an
> x86_64 developer machine `nr::WRITE` would silently become `1` and every host
> test of the AArch64 tables would be testing the other architecture.
>
> The dispatcher's two matches are now one neutral table (77 arms) plus a named
> list of **21 x86-only legacy spellings** — `open`, `stat`, `poll`, `select`,
> `fork`, `mkdir`, `arch_prctl`, `time` and friends — which have no asm-generic
> number and must not be given invented ones. The set of numbers the kernel
> answers is **identical**: 105 before, 105 after.
>
> **Caution 2 paid immediately.** Classifying the raw arms found that x86_64 88
> is `symlink`, not the `futimens` its comment claimed — there is no `futimens`
> syscall in Linux — so `ln -s` had been handing its link path to `utimensat` as
> a `struct timespec[2]` pointer. `utimensat` returns 0 on that, so **`ln -s`
> reported success and created nothing.** Fixed, and covered by a boot self-test
> whose negative control confirms only the `readlink` round trip catches it: the
> return value alone passes against the bug.
>
> Verified QEMU **413/0** (`SMP=1`, was 405) and **422/0** (`SMP=4`, was 414) —
> +8 both, exactly the eight new checks — and Firecracker on the box **403/0**.
> Host tests 1355 → **1359**. Bare metal not run: the change is a dispatch
> table with no machine dependency, and the box was left on Ubuntu.
>
> Steps 3–6 (folding arms into glue, starting with the leaves) are unstarted.

> **C1 step 3's first arm landed, and it broke bare metal only** (2026-09-07,
> verified the same day). `uname` is served by `akuma-syscalls-glue` now, which
> needed `akuma_exec::runtime::register` to have run — and that call
> (`exec_runtime::init`) was added to `kmain` and not to `kmain_mb2`. **QEMU and
> Firecracker enter via PVH; GRUB enters via multiboot2**, so both VMM rigs were
> green and the metal died at the first folded syscall with
> `akuma-exec: ExecConfig not registered`. A boot-protocol-shaped failure
> wearing a memory-shaped message.
>
> Reproduced without a reboot on the box's own OVMF/GRUB rig
> (`/root/ovmf5.sh`), which is the multiboot2 path under KVM — the A arm
> panicked at 10 s. Fixed by making the console hook and the runtime
> registration **one shared function**, `boot::install_shared_sinks`, called
> from both entries: the same remedy `boot::early_init` already is, applied to
> the step that had drifted next. `set_print_hook` had been duplicated in both
> `kmain`s and was the invitation.
>
> **A second, pre-existing defect surfaced behind it** — the netpoll
> lap-wait in `net::netpoll_spawn_selftest` was unbounded. Its budget is
> `uptime_us()`, which is `lapic::ticks()`, and `boot::self_tests` **stops the
> LAPIC timer** before that test runs; so whenever the daemon also fails to lap,
> the deadline is a promise nothing keeps. Both conditions hold on exactly one
> rig — OVMF/GRUB q35, whose e1000 this kernel does not drive — and the
> multiboot2 boot spun there forever, never printing a tally. That is the *hang*
> version of the headless-box failure the loop's own comment was written to
> prevent. A yield cap (`MAX_YIELDS`) restores the bound; it costs a healthy
> boot nothing, because the lap condition breaks out in microseconds.
>
> Verified: local QEMU/TCG PVH **425/0** (`SMP=4`); OVMF/GRUB multiboot2 rig
> reaches its tally at **415/1**, the one failure being that rig's undrivable
> NIC (`netpoll laps 0`, 200000 yields); and **bare metal 416/0**, where the
> real RTL8169 makes the same check pass (101 laps in 1408 yields) — with
> `ssh akuma "uname -a"` answering `de169eed-release`, i.e. the folded glue arm
> serving on the machine that could not boot before. Host tests green.

> **C1 step 3, batch 2 (2026-09-07)** — `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`.
> The `FastPath::Leaf` tier folded: `getuid`/`getgid`/`geteuid`/`getegid`, with
> `setuid`/`setgid` alongside. They take no arguments and consult no `Process`,
> so glue's prologue skips the identity resolve — which is the point on a target
> that does not populate `akuma-exec`'s process table. Deliberately **not**
> folded: `getpid`/`gettid`/`getppid`/`getpgid`/`getsid`/`getcwd`, which *are*
> identity and would have glue answering confidently and wrongly; they wait for
> C1 step 5.
>
> Verifying it with a real ring-3 caller — which §4's lesson says is the only
> thing that verifies a folded arm — found `getgroups`. `busybox id` printed
> `uid=0 gid=0` and then `id: can't get groups`, exit 1: glue has had
> `sys_getgroups` all along and `akuma-syscalls-abi` had no row for x86_64 115,
> so the number decoded to nothing. One row, one arm, `id` exits 0. That row is
> the pair that earns the two-number shape — asm-generic 158 is `getgroups`,
> **x86_64 158 is `arch_prctl`**, and this kernel answers both.
>
> The fifteen new checks are about the **number hop**, not the value: every one
> of these was `=> 0` before and is `=> 0` in glue, so a value check would pass
> against a dispatcher that had lost the arms. x86_64 102-108 lands in
> asm-generic's timer block, where `nr::SETITIMER` is 103 and reads two pointers
> out of `args[1]`/`args[2]`.
>
> `akuma-syscalls-abi` is a workspace member no crate under `crates/` and not the
> root kernel depends on — only `amd64/` links it — so the AArch64 kernel cannot
> be affected by the new row.
>
> **One open failure, newly visible rather than newly caused.** With the netpoll
> self-test fixed twice over (its own timer bracket, and a stalled-clock
> backstop in place of the yield cap that pre-empted it), bare metal reports
> `netpoll laps 0` after spending the whole 2 s budget across 410534 yields.
> The daemon is healthy — its own `mem:` line reaches `dmesg` every 10 s once
> the suite ends, and ssh answers throughout — so the fault is bounded to
> "during `boot::self_tests`, on real hardware, the daemon does not get picked",
> and it is boot-thread-relative: QEMU/TCG gives 101 laps in 101 yields, the
> daemon running on every one. Left open; it gates nothing and wants the
> scheduler picker instrumented. The gain is that it is a `[FAIL]` with a number
> beside it rather than a hang on one rig and a coin-flip on another.
>
> **[CLOSED 2026-09-07, and the diagnosis above is wrong in its central
> claim]** — `docs/archive/AKUMA_AMD64_NETPOLL_LAPS_ZERO.md`. The daemon *was*
> picked; it was inside **one lap** for longer than the whole budget. Three
> counters split what `NETPOLL_LAPS` alone could not (`entered 1, drains
> completed 1, ticks completed 0`) and named `clock::sync_tick` in one boot,
> with no picker instrumentation at all. The `RETRY_INTERVAL_US` rate limit was
> armed inside `sync_tick`, so a **failed** boot-time `sync_via_sntp` left
> `NEXT_RETRY_US` at `0` and the daemon's first lap re-attempted it for the
> full 2.5 s `RETRY_TIMEOUT_US` — against a 2 s test budget. Deterministic on a
> condition nobody was watching, not flaky: the "coin flip" was whether the
> boot SNTP happened to land first. `report_outcome` arms the interval now, so
> every attempt sets the clock for the next one.
>
> Two things fell out. A **duplicate netpoll daemon** on the multiboot2 path
> only — `boot_to_init` spawned one unconditionally after the suite had already
> spawned one, so every full-suite bare-metal boot ran two, each polling the
> stack on its own core; `spawn_netpoll` is idempotent now. And the check is
> **two checks**: "is being scheduled" now asks `NETPOLL_ENTERED`, which is the
> question its name always claimed, and "completes laps" asks the throughput
> question the budget actually bounds.
>
> Verified QEMU **441/0**, **bare metal 432/0** (laps 101 in 101 yields, from
> `laps 0` in 410534), and — the strong result — the OVMF/GRUB rig at
> **432/0**, which had *never* passed this check because its NIC is undrivable
> and DNS can therefore never work there.

> **C1 step 3, batch 3 — the leaf tier is finished (2026-09-07)** —
> `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md` § "batch 3".
> `getrandom` and `prlimit64` fold; the rest of the hand-off prompt's step-3
> list does not, and each reason is written down rather than left as a gap.
>
> Both needed something built first and both closed a real defect, which is now
> the pattern for this step rather than a coincidence. **`getrandom`** could not
> fold because glue's body named `akuma_virtio::rng::fill_bytes` outright and
> **no rig of this target has a virtio-rng device** — the bare-metal box takes
> its entropy from `RDRAND` — so the fold would have returned `EIO` to every
> ring-3 caller, `sshd`'s key exchange included. The source is named rather than
> the device now: `akuma_primitives::rng`, a `OnceCopy` hook in the same shape
> as the `clock` beside it, registered from `boot::install_shared_sinks`. Glue
> still falls back to the virtio device when nothing is registered, so the
> AArch64 kernel registers nothing and behaves as before. Two silent divergences
> closed with it: the amd64 arm capped at one 256-byte chunk, and it returned
> the byte count **whether or not the fill succeeded** — a `RDRAND` out of
> entropy handed ring 3 a buffer whose tail was kernel stack.
>
> **`prlimit64`** was `=> 0`, which is not a stub but a wrong answer: success
> without writing `old_rlim` leaves the caller reading its own stack as its
> limits, and musl's `getrlimit` is this syscall. The fold is the fix — and it
> made a dormant placeholder load-bearing. `ExecConfig::user_stack_size` was
> `sched::STACK_SIZE`, **the per-thread kernel stack**, correct only because
> nothing read it; `busybox ulimit -s` printed `32`. It is the real 512 KiB now,
> and on this target that is a literal edge rather than a policy hint —
> `build_stack` maps exactly `ELF_STACK_PAGES` eagerly with no growth and no
> guard page. This is **Caution 2 arriving from a direction it does not name**:
> the divergence was not in an arm at all, it was in a config field, and the
> fold is what read it.
>
> Eleven new checks, and unlike batch 2's these are **value** checks with a
> working negative control — both arms change their answer. Then a real ring-3
> caller on the metal, because the suite's own checks are vacuous under its
> `BypassValidationGuard`: `ssh akuma "ulimit -s"` → `512` (was `32`),
> `ulimit -n` → `1024`, and the session existing at all is the `getrandom` proof.
>
> QEMU/TCG **453/0** (`SMP=4`), OVMF/GRUB **444/0**, **bare metal 444/0**. Host
> tests 1360. **AArch64 is touched this time** — glue's `getrandom` gains one
> branch, behaviour-preserving because the hook is unregistered there — so this
> batch cannot claim the byte-identical sections the earlier ones did, and does
> not.
>
> Step 4 (the VFS surface) is next and is where the fold pays: glue's `fs.rs` is
> 3,112 lines against `fd.rs`'s hand-rolled equivalents.

Measurements as of 2026-09-07:

- `cargo check -p akuma-mmu --target x86_64-unknown-none` **passes**. The crate
  reaches amd64 transitively through `akuma-user-access`, and carries a real x86
  backend: `x86_map_page_in`/`x86_unmap_page_in`/`x86_translate_in`, `invlpg`,
  CR3 rewrite for full flushes, and its own x86 `Prot`/`MemAttr` encode
  (`crates/akuma-mmu/src/lib.rs`, the Phase-4 block,
  `proposals/AKUMA_MMU_ARCH_PORTABILITY.md`).
- **[Corrected 2026-09-07, same day — this is what B3 did.]**
  `cargo check -p akuma-syscalls-glue --target x86_64-unknown-none` **passed**
  as of the B3 landing above. It read, when this plan was written:
  it **fails**,
  and the error list is the work: `UserAddressSpace` on x86 is the scoped-down
  six-method Phase-4 type, missing the AArch64 surface glue programs against —
  `ttbr0()`, `is_shared`, `map_user_page_tracked`, the `UserPages` impl,
  `is_mapped`/`is_range_mapped`/`read_l3_page_entry`, and the ledger forwards
  (`resident_pages`, `tracks_user_frame`, `user_frame_count`,
  `user_frame_total_refs`, `page_table_frame_count` — all of which exist
  arch-neutrally in `akuma-user-space`; the x86 type just does not forward
  them), plus `invalidate_icache_for_page_va`, which on x86 is a no-op that
  still has to exist.
- `MmapRegion::flags` is no longer a raw AArch64 `u64` — the opaque `Prot` token
  landed 2026-09-06 (`crates/akuma-mmap/src/types.rs`), so the §1 prerequisite
  the survey's mm section names is **gone**.

---

## The walk so far

The branch `i-am-about-to-regret-this-joke-down-the-line` *is* the amd64 story:
129 commits, 2026-09-03 → 09-07, 25 docs. The name was the assessment; the
diagram is the receipt. Read downwards; it ends where "The tree" below begins.

```
      THE WALK SO FAR — 2026-09-03 → 09-07 · 129 commits · 25 docs

 [09-03] ═══ THE REGRET BEGINS ═══  "preparing for a dumpster computer"
   │
   ▼   PVH → long mode; akuma-alloc + akuma-pmm wire up unchanged
   │   heap-before-PMM ordering survives the move; the neutral-crates
   │   claim — the whole plan rests on it — is proven, not argued
   │                        docs: AKUMA_FIRECRACKER_AMD64.md
   ▼   Stages B–F, one day: 4-level paging · #PF servicing · LAPIC
   │   tick · round-robin scheduler · GDT/STAR/FMASK · ring 3 via
   │   syscall/sysret · dispatch table renumbered to x86_64 Linux ABI
   │   (asm-generic aarch64 numbers ≠ x86 — every number checked)
   │                        docs: AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md
   ▼
 [09-04] ═══ USERSPACE, VMM-FIRST ═══
   │
   ▼   networking: akuma-net + smoltcp build unchanged; the twelve
   │   NetRuntime hooks written out one by one, each a decision with a
   │   reason · DHCP lease · DNS (resolve_host, syscall 300) · SNTP
   │   boot-time clock for TLS dates
   ▼   userspace runs: apk (select/fdset marshalling un-wedges its TLS
   │   fetch) · paws · tcc · busybox · hget HTTPS
   │   crate-reuse audit: what already built for x86_64-unknown-none,
   │   and the two things that never would (akuma-mmu, glue)
   │                        docs: FIRECRACKER_PORT.md,
   │                              AMD64_CRATE_REUSE_AUDIT.md,
   │                              REDUCING_PLATFORM_DEPENDENCY.md (§1 Prot)
   ▼
 [09-05] ═══ 48h: 0 → SSH ON THE METAL ═══ "actually boots on real hardware"
   │
   ▼   multiboot2/GRUB entry: akuma-multiboot2 crate (the tag-offset-32
   │   bug becomes a 0.2 s test) · akuma-fbcon (no UART on this board —
   │   the framebuffer is the console, fonts chosen at runtime) ·
   │   rdmsr-clobbers-%edx and the 64 KiB boot stack found by recording
   │   %eax/%ebx at entry (__entry_eax/__entry_ebx stay for next time)
   ▼   the rig that ended the reboot cycle: KVM + OVMF pflash + monitor
   │   unix socket — screendump and xp/3wx as debug channels, two
   │   minutes per iteration instead of a walk to another room
   ▼   the metal, five firmware-only bugs: #UD without EFER.SCE ·
   │   UEFI's fragmented memory map · GRUB modules not marked used ·
   │   4K scroll = seven million pixels · PHYSMAP_LIMIT lying
   ▼   RTL8169 on real silicon: link=up/1000M/full, rx off a real LAN,
   │   ARP + ICMP answered (on a real tap — slirp cannot be pinged),
   │   TCP handshake, ssh session · PCI enumeration · SMP bring-up
   │   (PerCpu via %gs, ticket BKL, transfer-on-switch) · x86
   │   UserAddressSpace (MMU Phase 4) · user-copy fixup path
   │                        docs: AKUMA_AMD64_ON_HP_500_502NJ.md (log),
   │                              AMD64_SMP_BRINGUP.md,
   │                              AKUMA_MMU_X86_ADDRESS_SPACE.md,
   │                              AKUMA_THREADING_X86_SWITCH.md,
   │                              AKUMA_USER_ACCESS_X86_FIXUP.md
   ▼
   ┌────────────────────────────────────────────────────────────────┐
   │ THE TRASHCAN LOOP — the agent goes semi-autonomous             │
   │                                                                │
   │ one box, two personalities, one IP:                            │
   │   Ubuntu @ .123:22 — builds, stages, arms GRUB                 │
   │   Akuma @ .123:2222 — the thing under test                     │
   │                                                                │
   │   ssh akuma "reboot -f"        # Akuma resets → Ubuntu         │
   │   rsync + cargo build on Ubuntu # (-F /dev/null or it talks    │
   │   grub-reboot "Akuma/amd64"    #  to the wrong OS!)            │
   │   ssh … "reboot"               # → Akuma                       │
   │   ssh akuma "<test>"           # observe, repeat               │
   │                                                                │
   │ scripts/utils/hpbox.py wraps it (which_system / wait_for /     │
   │ reboot_to / push): deploy-build-reboot-debug on real silicon,  │
   │ unattended, from a laptop — the loop every later fix ran       │
   │ through. Ask which system is running; never assume.            │
   │                        docs: runbooks/amd64-bare-metal-loop.md │
   └────────────────────────────────────────────────────────────────┘
   ▼
 [09-06] ═══ MEMORY GETS REAL + THE PORT PAYS DEBT BACK ═══
   │
   ▼   the ssh-stall autopsy: the clock was never calibrated, then not
   │   running — fixed with two instruments built for the purpose
   ▼   CoW fork (x86 bit 9 marker; SMP=1-only, pinned) · a real Rust
   │   std binary reaches stage 6/6 and dies on exactly clone()
   │   — the threads wall, measured · threads + futex land (CLONE_VM,
   │   decisions from akuma-syscalls-sync) · xHCI USB mass storage →
   │   ext2 mount → busybox+apk from a disk · dynamic linking asked,
   │   deferred with reasons
   │
   │   ── the yield nobody planned: the port forces extraction —
   │      akuma-dmesg (the 4 KiB dmesg ceiling bug) · akuma-procfs
   │      (the 41-vs-44-field stat line) · akuma-user-space (replaces
   │      the capped, refcount-less FrameSet) · akuma-pipes (end
   │      refcounts; `yes | head` worked or nothing) · akuma-elf
   │      arch-neutral split · futex decisions — each crate AArch64
   │      debt, host-tested, both kernels now eat from it
   │                        docs: AKUMA_AMD64_COW.md,
   │                              AKUMA_AMD64_RUST_STD.md,
   │                              AKUMA_AMD64_USB_XHCI.md,
   │                              AKUMA_AMD64_DYNAMIC_LINKING.md,
   │                              AKUMA_USER_SPACE_LEDGER.md,
   │                              AKUMA_PIPES_EXTRACTION.md,
   │                              AKUMA_ELF_ARCH_NEUTRAL.md,
   │                              AKUMA_SELF_HEALING_PORT.md,
   │                              AKUMA_AMD64_STREAMLINING.md (survey)
   ▼
 [09-07] ═══ YOU ARE HERE ═══
   │
   └──► the unlock tree below — A1, A2, B1, B2, the six memory gaps and
        B3 all landed this day; the gate
        (`cargo check -p akuma-syscalls-glue --target x86_64-unknown-none`)
        is GREEN, and C1 is next
```

The shape worth naming: days 1–2 *consumed* shared crates, day 3 *proved*
them on hardware, and day 4 *produced* them — the port's biggest effect on
the tree was not amd64 code, it was five crates of AArch64 debt turned into
host-tested shared code. The branch regretted itself into an audit.

---

## The tree

Trunks A and B are independent and meet only at B3; C needs the gate; D is
parity with what the AArch64 self-host already proves.

```
                    QUEST: SELF-HOSTING ON amd64
          (cargo + rustc building the kernel on the guest,
           parity with what the AArch64 self-host already has)

  ┌─────────────────────────────┬─────────────────────────────────┐
  │  TRUNK A: scheduler         │  TRUNK B: memory                │
  │  (independent of B)         │  (independent of A)             │
  └──────────────┬──────────────┴──────────────┬──────────────────┘
                 ▼                             ▼
  ┌──────────────────────────┐   ┌────────────────────────────────┐
  │ A1. sched.rs → akuma-    │   │ B1. ENABLE akuma-mmap          │
  │     threading            │   │     (Prot token landed,        │
  │                          │   │      2026-09-06 — blocker gone)│
  │  brings: park/wake,      │   │                                │
  │  WAITING state, real     │   │  brings: MmapRegion bookkeeping│
  │  context switch          │   │  lazy regions, munmap          │
  │                          │   │  clip-and-split, mprotect      │
  │  deletes: sched.rs 1134, │   │  splitting, MAP_FIXED          │
  │  thread.rs 391, smp.rs   │   │                                │
  │  ticket-lock BKL →       │   │  keeps: x86 Prot encoder in    │
  │  akuma-bkl               │   │  paging.rs (bits live with     │
  └──────────────┬───────────┘   │  the walker, by design)        │
                 │               └───────────────┬────────────────┘
                 │                               ▼
                 │               ┌────────────────────────────────┐
                 │               │ B2. #PF handler + uaccess      │
                 │               │     demand paging (not-present │
                 │               │     fault + region table),     │
                 │               │     is-mapped/prefault walk    │
                 │               └───────────────┬────────────────┘
                 ▼                               ▼
  ┌────────────────────────────────────────────────────────────────┐
  │ A2. wakes become real (pure wins, no port needed after A1)     │
  │   futex.rs:  poll loop → park on tid          (ThreadWaker)    │
  │   pipe.rs:   fire() drops Wakes → fires them                   │
  │   net.rs:    park_until/waker/netpoll doorbell un-collapse     │
  │   wait4:     spin → block;  fcntl/O_NONBLOCK reachable         │
  │   signals:   is_current_interrupted hook becomes true          │
  └──────────────────────────────┬─────────────────────────────────┘
                                 │
                 ┌───────────────┘ (B2's probes are region queries,
                 │  so B1 → B3; A and B only meet here)
                 ▼
  ┌────────────────────────────────────────────────────────────────┐
  │ B3. WIDEN x86 UserAddressSpace (~12 methods, walker done)      │
  │   ledger forwards ← akuma-user-space (exists, thin)            │
  │   ttbr0()→root getter, is_shared, map_user_page_tracked,       │
  │   UserPages impl, is_mapped/is_range_mapped/read_l3_page_entry │
  │   invalidate_icache_for_page_va = no-op (x86 has no VIVT icache)│
  └──────────────────────────────┬─────────────────────────────────┘
                                 ▼
  ╔══════════════════════════════════════════════════════════════╗
  ║  THE GATE: akuma-syscalls-glue builds for x86_64-unknown-none ║
  ║  ✔ PASSED 2026-09-07 — AKUMA_AMD64_B3_ADDRESS_SPACE.md        ║
  ╚══════════════════════════════┬═══════════════════════════════╝
                                 ▼
  ┌────────────────────────────────────────────────────────────────┐
  │ C1. FOLD usermode.rs IN (4378 → entry seam)                    │
  │   ~32 dispatch arms + bodies → glue   (diff for pinned         │
  │                                        divergences while folding)│
  │   Spawn/PROCS → akuma-exec: real fork/exec/lifecycle/reclaim   │
  │   loader.rs placement → akuma-elf load half                    │
  │   keeps: syscall/sysret asm, swapgs bracketing (= el0-entry    │
  │          shape on AArch64)                                     │
  └──────────────┬───────────────────┬─────────────────────────────┘
                 ▼                   ▼
  ┌────────────────────────┐ ┌─────────────────────────────────────┐
  │ C2. fd.rs cache dies   │ │ C3. clock.rs dies                   │
  │   VfsHooks inode reads │ │   akuma-syscalls-time builds here   │
  │   (block cache comes   │ │   real clock: re-sync, drift,       │
  │    back via ext2)      │ │   itimers, adjtimex                 │
  └───────────┬────────────┘ └────────────────┬────────────────────┘
              └───────────────┬───────────────┘
                              ▼
  ┌────────────────────────────────────────────────────────────────┐
  │ D. RUSTC/CARGO APPETITE (the last mile, mostly by then free)   │
  │   file-backed + lazy mmap  ← B1/B2 (rustc mmaps rlibs)         │
  │   threads + futex          ← A1/A2 (rayon, jobserver)          │
  │   wait4, pipes, signals    ← A2 (cargo -j, SIGINT)             │
  │   big VA overcommit        ← B1 (rustc asks for GBs, lazily)   │
  │   procfs stat/status       ← already shared (akuma-procfs)     │
  └──────────────────────────────┬─────────────────────────────────┘
                                 ▼
              scripts/run_selfhost_kernelbuild.py, amd64 arm: GREEN
```

Two properties of the shape:

- **A and B never block each other** — they meet only at B3, which needs B1's
  regions (the probes are region queries) and nothing from A. The scheduler
  port can start today.
- **The gate is one `cargo check`**: `cargo check -p akuma-syscalls-glue
  --target x86_64-unknown-none` going clean is the objective mid-point.
  Everything above it is deletions from `amd64/src`; everything below it is
  parity with what the AArch64 self-host already has.

## Open issues found by probing the bare-metal box (2026-09-07)

Found by running the probes on the HP box after the A1 fold landed, not by
looking for them. Neither is caused by the fold; both are on the path to D.

### 1. `open(O_CREAT)` does not validate the parent directory — writes vanish silently

```
akuma:/# echo A > /nosuchdir/file ; echo $?
0
akuma:/# ls /nosuchdir
ls: /nosuchdir: No such file or directory
```

**Exit 0, no error, and the data is gone.** On Linux that open is `ENOENT` and
the shell says `cannot create`. Reproduced identically on QEMU and on bare
metal, so it is the fd layer rather than anything machine-specific.

The console shows where it surfaces:

```
[close] persist failed for "/nosuchdir/file": not found
```

`amd64/src/fd.rs` buffers a created file in memory and writes it through at
`close`. That close path is *deliberately* report-and-return-0 — its comment
explains why (the data is already unreachable and Linux's errno slots are
taken), and it exists because a silent loss here once surfaced as `apk`'s
rename finding no tmp file, an `ENOENT` pointing three layers from the real
failure. **That is a symptom-catcher doing its job.** The defect is upstream:
the `open` should have failed and never handed out a descriptor.

Why it matters for self-hosting: a build writes constantly to paths under
directories it assumes exist. Every such write reports success and produces
nothing, and the first *visible* symptom is somewhere else entirely — a missing
object file, a link error naming a file whose source is plainly present. It is
the same shape as the stale-source trap in the deploy loop, one layer down.

**Fix at `open`, not at `close`**: resolve the parent, `ENOENT` if it is not a
directory that exists. Item **C1** deletes this code path wholesale by folding
`usermode.rs`/`fd.rs` into `akuma-syscalls-glue`, which resolves paths through
the VFS properly — so the cheap move is a targeted parent check now and the
real fix is C1. Worth a boot self-test either way, because the current suite
has no case for "create in a directory that is not there".

### 2. There is no `/dev` at all

`ls /dev` → `No such file or directory`, on both the RAM image and QEMU's
generated disk. So `/dev/null`, `/dev/zero`, `/dev/urandom` and `/dev/tty` are
all absent.

Right now this is masked by issue 1 — `> /dev/null` "succeeds" — which is a bad
pairing: fixing the open check without adding `/dev/null` turns a lot of
currently-"working" shell into hard failures. **Do them together**, and expect
the transition to be noisy: `/dev/null` in particular appears in almost every
non-trivial script and in most build systems.

The image builders are `amd64/mkdisk.sh` and the devbox rootfs; the kernel side
is whether these want to be real device nodes (a VFS device table) or ordinary
files, which is a decision this target has not had to make yet.

### 3. A one-shot `ssh` command never returns — it hangs *after* printing

Reported 2026-09-07, on the bare-metal box:

```
$ ssh -i ~/.ssh/id_ed25519 root@192.168.1.123 -p 2222 "uname -a"
Akuma akuma 0.1.0-amd64 Akuma/amd64 (x86_64 bring-up) x86_64 GNU/Linux
<hangs here — no exit, no prompt>
```

The command **runs** and its output arrives in full. What never happens is the
session teardown: the exit status, the channel close, the TCP close. So this is
not a command-execution failure and not a networking-reachability failure — it
is whatever is supposed to notice that the child has finished and propagate that
through the pipe to the channel to the socket.

Two candidates, and they are the two A2 items:

- **Pipes.** `amd64/src/pipe.rs`'s `fire()` currently drops `Wake`s on the floor
  (A2 names this explicitly). A reader blocked on the child's stdout after the
  child exits gets no EOF wake, so `sshd` never learns the command finished.
- **`wait4`.** It spins rather than blocking (A2 again), and the spin is what
  the session thread would be doing while it should be reaping.

Both are `A2` — "wakes become real" — which is scheduled *after* the A1 fold
that landed 2026-09-07 and is not part of the B trunk. **Deliberately not
investigated now.** Revisit after A2 lands; if the hang survives it, it is a
real third bug and worth its own autopsy rather than a guess.

### 4. **aarch64**, not amd64: `test_spawn_ext_passes_env` panics the boot suite

Found 2026-09-07 while running `scripts/lima_aarch64_run.sh` to prove the amd64
`mmap` work had not touched the other kernel. It had not — `git diff` over
`src/`, `crates/` and `Cargo.toml` between the session's start commit and its end
is **empty**, so the aarch64 kernel binary is unchanged and this failure belongs
to the branch, not to that work.

```
[Test] spawn_ext env FAILED, child saw:
[Test] spawn_ext default env FAILED, child saw:
[Test] spawn_ext_passes_env FAILED (2 of 2)
!!! PANIC !!!  src/process_tests.rs:3411
```

Both cases fail the same way and the informative part is what is *missing*: the
child's output is **empty**, not wrong. The test spawns `/bin/busybox env`
through `SPAWN_EXT` and reads what came back on the spawn channel; an empty
string means either the child produced nothing or the channel did not deliver it,
and those need opposite fixes. `check_binary_exists` passed, so busybox is on
the image.

Worth holding next to issue 3 above — an amd64 `ssh` command whose output
arrives and whose *completion* never does — because both are "a child ran and
something about the end of it did not propagate". That is a resemblance, not a
diagnosis: nobody has looked yet, and the two kernels do not share this code.

Not investigated. It gates nothing in the B trunk and it is the aarch64 kernel's
boot suite, so it wants its own pass.

## What stays different forever, by design

Not work items — pinned seams. The end state is not zero platform differences;
it is every difference either behind a named seam or written down as a pinned
divergence. Difference-by-duplication (the third category, most of
`amd64/src` today) is what this plan deletes.

| seam | AArch64 | x86_64 |
|---|---|---|
| entry | `eret`, `akuma-el0-entry` | `syscall`/`sysret`, `swapgs` bracketing |
| PTE encoding | `Prot` → AP/PXN/UXN bits | `Prot` → bits 1/2/63 ("bits live with the walker") |
| TLB maintenance | ASID + `tlbi`, inner-shareable broadcast | `invlpg`, CR3 rewrite (no broadcast invalidate) |
| CoW marker | no marker bit — `refs > 0` | PTE bit 9 (pinned in `akuma-cow`) |
| icache maintenance | required after W^X flip | no VIVT icache — no-op |
| IPI delivery | GIC SGI | LAPIC IPI |
| per-CPU register | `TPIDR_EL1`/`TPIDRRO_EL0` | `GS` base + `swapgs` |

## Cautions carried over from the survey

1. **Same type name, two `cfg` impls is a waypoint, not the goal.** The x86
   `UserAddressSpace` hardcodes `SHARED_PML4_SLOTS = [256, 257, 511]` to this
   target's layout and says so in its own doc. Widening the surface must port
   the AArch64 contract, not invent a second design beside it — the
   `prot_roundtrips_to_todays_bits` pattern in `akuma-mmu` is how `Prot` was
   pinned; do the same for each new method.
   **B3 followed this (2026-09-07)** and it paid twice: every divergence is
   named at the method that has it (the table in
   `AKUMA_AMD64_B3_ADDRESS_SPACE.md` §5), and pinning `PteProt::from_region`
   against `amd64/src/paging.rs`'s six literals is what keeps the two x86
   walkers from drifting before C1 deletes one of them. It also found the
   sharper form of this caution: same type name, two `cfg` impls, **plus a glob
   re-export** is a silent per-architecture type substitution — see §3.
2. **Diff the dispatch arms against glue while folding (C1).** The arms have
   drifted: no signals, no `fcntl`, blocking-only sockets. Each divergence is
   either a pinned decision to carry over explicitly or a gap glue closes for
   free — the one thing it must not be is silent.
3. **Do not let unification reach the pinned divergences** — the six wait-loop
   fields (`akuma-net-yarn`), the seven pinned Linux divergences in
   `akuma-syscalls-{mem,poll,sync}`, the `akuma-net` vs `akuma-net-unix`
   layering. Those exist because measurements said so.

---

## Background

- `docs/archive/AKUMA_AMD64_STREAMLINING.md` — the survey this plan sequences;
  its §11 items are A1/B1/B3/C1 here.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` — §1 is the `Prot` token
  (landed 2026-09-06, unblocking B1); the seams table above is its end state.
- `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` — Phase 4 is the x86
  `UserAddressSpace` B3 widens; its non-goals define what "port, not redesign"
  means there.
- `docs/archive/AKUMA_SELF_HOSTING.md` — the AArch64 self-host, i.e. what "D"
  is parity with; `scripts/run_selfhost_kernelbuild.py` is the gate's oracle.
- `docs/archive/AKUMA_AMD64_RUST_STD.md` — the measurement that fixed threads
  (`clone(CLONE_VM|CLONE_THREAD)`) as the wall A1 removes.
- `docs/archive/AKUMA_AMD64_COW.md` — the pinned CoW-marker divergence, and why
  CoW fork is SMP=1-only on this target today.
