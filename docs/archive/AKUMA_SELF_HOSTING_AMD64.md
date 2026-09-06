# amd64: the unlock tree to self-hosting

**Date:** 2026-09-07
**Scope:** the dependency order among the amd64 work items — scheduler port,
`akuma-mmap` adoption, the `akuma-mmu` x86 surface, and the `usermode.rs` fold —
as they stand one day after the survey
(`docs/archive/AKUMA_AMD64_STREAMLINING.md`).
**Status:** plan, with measurements taken 2026-09-07:

- `cargo check -p akuma-mmu --target x86_64-unknown-none` **passes**. The crate
  reaches amd64 transitively through `akuma-user-access`, and carries a real x86
  backend: `x86_map_page_in`/`x86_unmap_page_in`/`x86_translate_in`, `invlpg`,
  CR3 rewrite for full flushes, and its own x86 `Prot`/`MemAttr` encode
  (`crates/akuma-mmu/src/lib.rs`, the Phase-4 block,
  `proposals/AKUMA_MMU_ARCH_PORTABILITY.md`).
- `cargo check -p akuma-syscalls-glue --target x86_64-unknown-none` **fails**,
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
   └──► the unlock tree below — A1/B1 start whenever; the gate is
        `cargo check -p akuma-syscalls-glue --target x86_64-unknown-none`
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
