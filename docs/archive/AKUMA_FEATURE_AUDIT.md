# Cargo feature audit (2026-11-10)

Part of the multiarch attempt ([`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md)): the
research doc's end state says "features are devices, same names in both machine halves"
([`AKUMA_ARCH_CONVERGENCE_RESEARCH.md`](AKUMA_ARCH_CONVERGENCE_RESEARCH.md) §1). This is the
measured starting point. Read-only audit; nothing changed yet.

## Counts

| manifest | features |
|---|---:|
| root `Cargo.toml` (AArch64 kernel) | 39 (+ `default`) |
| `amd64/Cargo.toml` | 22 (+ `default`) |
| `akuma-kernel-glue` / `-core` (forwarding layers) | 22 / 9 |
| all `crates/*/Cargo.toml` | 193 |

## Findings

1. **Two parallel feature vocabularies.** 16 names exist in both root and `amd64/` with the same
   job and *different forwarding lists*, written by hand twice: `smp-shared`, `fs-cache`,
   `syscall-debug-info`, `no-tests`, `net-noalloc`, the 7 `sc-*` (`sc-epoll sc-eventfd sc-timerfd
   sc-pidfd sc-aio sc-sysv-ipc sc-containers`) and four `no-bkl-*`. Examples of drift already
   present: root `smp-shared` also turns on `no-bkl-vfs` and `no-bkl-irq` forwards through
   different crates than amd64's; root `syscall-debug-info` lacks the `dep:` entries amd64 needs;
   `no-tests` in root forwards to 6 crates, in amd64 it is `[]` (a cfg flag only).
2. **The six `no-bkl-*` switches are, in every shipped build, one switch.** `smp-shared` enables
   all six (root) or four (amd64) and `smp-shared` is in both `default`s. They gate 76 `cfg` sites
   (`kernel_no_bkl_{network 28, vfs 17, process 9, mm 8, drivers 7, irq 7}`) via per-crate
   `build.rs`. Independent use is only the A/B drivers (`scripts/cargo_runner.sh` comment,
   `scripts/validate_fork_smp.py`) — and `akuma_bkl::policy` already has *runtime* toggles for the
   same phases. **Not dead; do not delete blind.** Candidate: keep the runtime toggles as the A/B
   mechanism and fold the compile-time six into `smp-shared`.
3. **Eleven diagnostic/instrumentation flags**, all off by default except `reboot-trace` on amd64:
   `leak-instr`, `pmm-instr`, `bkl-profile`, `net-profile`, `read-profile`, `syscall-debug-info`,
   `reboot-trace`, `pmm-forensics`, `mm-forensics`, `net-waker-park`, `net-direct-waker`.
   **Checked 2026-11-10 against the whole tree (not just scripts/docs): none is dead.** My first
   pass grepped only `scripts/ overlays/ acceptance/ docs/runbooks docs/reference` and reported
   `leak-instr`, `pmm-instr`, `net-direct-waker`, `reboot-trace` as zero-reference — wrong. Each has
   live `cfg` sites (`leak-instr` 40, `pmm-instr` 12, `reboot-trace` 2 in `amd64/src/reboot.rs`,
   added 2026-10-07 for "who called reboot(2)") and archive write-ups; `pmm-forensics` and
   `mm-forensics` are named in the `docs/README.md` symptom matrix (the `mmap` double-populate
   hunt) and used by the tokio kernel. They are hunting tools kept for the next hunt.
   The one real candidate is **`net-direct-waker`**: A/B'd *null* (`LONG_ROAD_TO_REDIS_PART_2.md`
   §"net-direct-waker A/B — null, correctly"), 3 cfg sites, one of three compile-time
   `WaitPolicy` choices in `akuma-net/src/socket.rs` (`direct_waker` / `targeted` via
   `net-waker-park` / `promiscuous`). Retiring it, or making the policy a runtime field, is an
   owner call.

4. **Bundles that are really build targets, not features:** `devbox` (`rump-default userspace-sshd
   sc-reboot`), `devbox-smoltcp` (`userspace-sshd smp-shared`), `taoxichuan` (`no-tests extreme
   userspace-sshd`). Referenced 24–107 times, so they stay, but `build-profiles.md` already says
   "a build target is profile + feature set": these are the named feature sets.
5. **`rump` now has three spellings:** `rump` (compiled in; *is in root `default`*), `rump-default`
   (starts it), `rump-tests`. With D1 (universal optional feature) this wants one place in the
   shared spine and the same name on amd64, off by default there.
6. **amd64 has switches AArch64 lacks and vice versa** (`linux-file-io`, `console-notify`,
   `no-tests`-as-flag on amd64; `smoltcp`, `sound`, `many-sessions`, `platform-firecracker`,
   `extreme`, `sc-reboot`, `userspace-sshd` on root). Under the device-names rule (`console disk net
   smp shell`) these are the arch-specific remainder or should be named the same.
7. **Features unification risk** (research doc §7) is a direct consequence of 1: two lists, so a
   flag added to one kernel is easy to forget on the other.

## F1 applied (2026-11-10): `scripts/check_feature_parity.py`

A ratchet, not an equality check: it records today's differences between the 16 common features in
`docs/reference/feature-parity-baseline.json` and fails only on a **new** one (the way through is
`--update`, like `check_unsafe_budget.py`). Three of the 16 differ today, none changed by this commit:

- `no-tests`: amd64's is a bare cfg flag; AArch64's also forwards `akuma-exec/no-tests`,
  `akuma-syscalls-glue/no-tests` and `akuma-net/small-sockets`. So an amd64 `no-tests` build keeps
  the full-size socket table and the test-only code in those crates.
- `sc-sysv-ipc`: amd64 does not forward `akuma-vfs-glue/sc-sysv-ipc`.
- `smp-shared`: amd64 does not forward `akuma-net/smp-shared` or `akuma-primitives/smp-shared`
  (AArch64 does), and forwards the `no-bkl-vfs` leaf crates directly where AArch64 goes through
  `no-bkl-vfs`/`no-bkl-irq`.

Whether each is intentional for the metal build is the owner's call; the checker only stops a
fourth from appearing unnoticed.

## F2 applied (2026-11-11): the `no-bkl-*` features are folded into `smp-shared`

All six are gone from every manifest: the root, `amd64`, `amd64/bin`, glue, the tests crate and
the leaf crates. Their cfgs read `kernel_smp_shared`. 26 code sites changed: 20 were
`all(kernel_smp_shared, kernel_no_bkl_X)`, and `PreemptGuard`'s IRQ-mask arm was
`any(kernel_no_bkl_network, kernel_no_bkl_vfs)` nested inside `kernel_smp_shared`. `akuma-ext2`'s
`no-bkl-vfs` became its own `smp-shared` (forwarding `akuma-primitives/smp-shared`), which both
kernels' `smp-shared` now forwards.

- **Nothing changes in a shipped build.** Both kernels' `smp-shared` already enabled every
  `no-bkl-*` that applied to them (amd64 had all but `no-bkl-irq`). Every site was already
  gated on `kernel_smp_shared` too, and every crate that read one gets `smp-shared` on both
  kernels (`akuma-primitives` through `akuma-exec`).
- **Section sizes.** The default AArch64 image's `.text`/`.rodata`/`.data`/`.bss` are
  identical. amd64's `.text` is 16 bytes smaller: per-function sizes are identical, and the only
  difference is which name identical-code-folding kept for one 0x65-byte `drop_glue`.
- **`no-bkl-irq` was already dead.** Its last code site was the SGI scheduler's IRQ path, deleted
  in `cd1d8f17`. Only `akuma-exceptions/build.rs` still derived the cfg, and nothing read it.
- **A/B** stays with the runtime toggles in `akuma_bkl::policy`. No script passed a `no-bkl-*`
  feature; the two that named one did so in comments, now repointed.
- **Feature parity.** The ratchet's shared set went from 16 names to 12, and `smp-shared`'s
  forwarding differs between the kernels only by `akuma-net`/`akuma-primitives`, which amd64 gets
  transitively.

Gates: clippy on default, devbox-smoltcp, devbox (rump), amd64 (base and
`ring0-preempt,no-tests`) and the host. Unsafe 414, addr 700. HVF boot suite: 314/1, 323/0 and
321/0 at SMP=1, 2 and 4, the same as before the change (the one failure is the open
`thread_slot_reclaim_on_spawn`, `AARCH64_SPAWN_RECLAIM_SKIPPED.md`). amd64 TCG: 829/0 at SMP=1 and
839/0 at SMP=2.

## `net-noalloc` removed (2026-11-11)

Owner's call: nobody uses it. It was off in both kernels, and every measurement went against it:
HTTP p90 2.9x worse (`AKUMA_NET_ISSUES.md` §7), and −14.6 % redis throughput on the workload
predicted to suit it (`REDIS_ROUND_TRIP_CEILING.md`). Removed:

- `akuma-net-nic/src/virtio_rings.rs` (370 lines);
- the ring arm of `device.rs`'s receive and transmit paths;
- `irq::nic_kick_tx`;
- the four `Nic` methods only the ring called (`submit_tx`, `complete_tx`, `poll_transmit`,
  `fill_buffer_header`) and `nicstat::record_tx_complete`;
- the feature from all six manifests.

`[NICSTAT]` keeps its `orphan`/`tx_stall` columns, always 0, so parsers do not shift.
`akuma-net-nic`'s unsafe budget went from 13 to 9. The default image's sections are unchanged.

## F4 applied: `linux-extra-syscalls` (2026-11-11)

Both kernels have `linux-extra-syscalls` = the seven `sc-*` families, in `default`. Every script
already selected all seven or none, so the devbox lists name the group. The families stay
individually settable. `sc-reboot` is not a member, because only AArch64 has it. The parity
baseline gains one common name (12).

Verification:

- AArch64: the default, devbox and `extreme-size` images have the same sections as before. The
  `extreme-size` size did not move.
- amd64: the default image's `.text` is byte-identical to a build listing the seven families
  explicitly.

## F5 applied: `rump` out of `default`, `rump-tests` and `devbox` folded (2026-11-11)

- `rump` is off by default. Acceptance 11 builds `--features rump`.
- `rump-tests` is gone: every `rump` build runs the rump regression guards at boot, `no-tests` or
  not. The devbox already did; a plain `rump` build ran them anyway, because it does not set
  `no-tests`.
- The `devbox` bundle is gone. `scripts/build_devbox.sh`, `overlays/devbox/run.sh` and
  `scripts/sched_audit_matrix.py` name `rump-default,userspace-sshd,sc-reboot` directly.
- `rump-default` stays a feature. The plan merged it into `rump`, but acceptance 11 runs a rump
  kernel with box 0 on the native stack and checks the kernel-as-client sysproxy test, which
  `rump-default` compiles out. The choice would have to be made at boot, and the AArch64 kernel
  reads no command line yet (`akuma-params` is wired only on amd64).

Verification:

- Identity: the new `--features rump` image has the same `.text`/`.rodata`/`.data`/`.bss` as the
  old default, and the new devbox set gives the same image as the old
  `devbox,…,rump-tests` set.
- The default image is 81 KB smaller in `.text`.
- HVF on the default image: `SMP=2` 323/0, `SMP=4` 321/0, `SMP=1` 314/1 (the open
  `thread_slot_reclaim_on_spawn` failure).
- HVF on the `rump` image: all four rump guards pass.
- Lima devbox-smoltcp at `SMP=4`: epoll 0 FAIL / 1 DIVERGE, futex all pass.

## `many-sessions` and `small-sockets` removed (2026-11-11)

`many-sessions` was the kernel half of the process-per-session sshd. It raised the listener
backlog from 8 to 32 and lifted `small-sockets` builds' socket table to 512. The owner's
call was that the kernel should not carry a mode for one daemon; whoever needs sshd sized
differently configures sshd. Every build now has `MAX_BACKLOG` = 8, the value amd64,
`extreme-size` and the rump devbox always had.

`small-sockets` went with it. It was set only by AArch64 `no-tests`, and without
`many-sessions` it would have dropped the devbox to 32 smoltcp sockets: one 8-deep listener
plus 24 sessions would fill it. The table is now 256 everywhere but `kernel_profile_extreme`
(32). This also takes `small-sockets` out of `no-tests`, one of its three AArch64-only
entries (the parity baseline ratcheted).

Known cost: a burst of more than 8 simultaneous connections gets RSTs (measured on sshd:
8/8, 12/16, 17/24). The note on what to do is `docs/reference/build-profiles.md`
§ Listener backlog. Gates: HVF `SMP=2` 323/0, `SMP=4` 321/0, and `SMP=1` 314/1 (the open
`thread_slot_reclaim_on_spawn` failure). Lima devbox `SMP=4`: epoll 0 FAIL / 1 DIVERGE,
futex all pass.

## amd64 `console-notify`, `linux-file-io`, `reboot-trace` made unconditional (2026-11-11)

All three were in amd64's `default`, and no amd64 kernel can build without its defaults: the
`smp-shared` const assert in `smp.rs` stops the build. So the opt-out each comment offered could not
be exercised. The features are gone and their code is always compiled. The image is the
same: the boot suite still runs the `console_notify` checks and counts 829/0 at `SMP=1` and 839/0 at
`SMP=2`, as before. `console_notify` (322) stays amd64-only; the AArch64 kernel still answers
`ENOSYS`.

## `net-waker-park` and `net-direct-waker` removed (2026-11-11)

These were the two targeted parks for a blocking socket waiter (F3). `net-waker-park` registered on
the kernel socket's waker list and measured worse (`AKUMA_NET_ISSUES.md` §8). `net-direct-waker`
registered on the smoltcp socket and A/B'd null (`LONG_ROAD_TO_REDIS_PART_2.md`). Both were off,
and no script enabled either. Removed:

- `akuma-net-yarn`'s `ParkKind::{Targeted, DirectWaker}` and `WaitPolicy::{targeted, direct_waker}`;
- `wait_park`'s register/re-check arm;
- `smoltcp_net::register_socket_waker`;
- the `idx` parameter `wait_until` carried only to register a waker;
- the feature-gated boot test `test_socket_wait_registers_waker`.

`wait_until` uses `WaitPolicy::promiscuous()`, as every build did. The epoll family's
`ScanRegistered` kind and the model-only `LightSleep` stay.

## `extreme`, `userspace-sshd` and `fs-cache` removed (2026-11-11)

`extreme` selected the 4 MB-floor build: `cfg(kernel_profile_extreme)` (also emitted by several
build scripts on `OPT_LEVEL=z` alone) shrank thread stacks, the heap seed, `MAX_THREADS` (64),
`MAX_SOCKETS` (32), the NIC RX buffer (2 KB), the dmesg ring, `MAX_ARG_STRLEN`, `HEAP_SLURP_MAX`,
the boot stack (32 KB) and the timer probe, dropped the ext2 block cache and the console lock,
and turned the boot suite off. `userspace-sshd` was only ever selected with it (the herd-less
`AUTO_START_SSHD` path). Both are gone; `AUTO_START_HERD` is `true` on every build.

The `extreme-size` **profile** stays, as codegen only (`opt-level = "z"`, fat LTO, one CGU,
stripped). `AKUMA_BUILD_PROFILE` still reports it. `--profile extreme-size --no-default-features
--features no-tests,smoltcp` is the pre-commit hook's size-profile clippy line.

`fs-cache` (the ext2 `ClockBlockCache`) is folded in: `BlockRingCache` and the direct-device arms
are deleted, and every build has the clock cache - including the rump devbox and Taoxichuan,
which did not before (16 MB cap unless the kernel sets one).

How: a cfg partial evaluator over the atoms `kernel_profile_extreme=false`,
`feature="extreme"=false`, `feature="userspace-sshd"=false`, `ext2_fs_cache=true`,
`feature="fs-cache"=true`, one rewrite per rescan, every change printed for review; then a hand
sweep of comments that described the removed behaviour. Four build scripts became empty and were
deleted (`akuma-{config,elf,net,ext2}`). `HookConfig::extreme` is `HookConfig::quiet`.
`scripts/build_extreme_size.sh` and `scripts/build_taoxichuan_fc.sh` (which used the removed
root `taoxichuan` feature) are deleted; `docs/runbooks/build-extreme-size.md` is
`EXTREME_SIZE_BUILD.md` here and `acceptance/05` is in `acceptance/archive/`.

No crate dropped out of any image: the removal deleted constants and arms, not dependencies.

| image | before | after |
|---|---|---|
| default | `.text` 3,340,252 | identical (`.rodata` +24 B) |
| devbox-smoltcp | 1,883,516 | +8 B |
| rump devbox | 1,672,352 | +10,740 B (gains the block cache) |
| amd64 | 3,035,375 | +32 B |
| `extreme-size` profile | 632,224 / bss 462,016 (old `extreme`) | 654,324 / bss 595,584 (full-size constants) |

`smp-shared` is not yet removable: Taoxichuan, the host tests and the rump devbox build without
it (252 cfg sites in 35 files). It needs its own step - deciding what Taoxichuan gets.

## Proposed simplifications

| # | change | risk | gate |
|---|---|---|---|
| F1 | One shared feature table: a `akuma-kernel-features`-style doc + a check script that asserts the 16 common names forward the same crates in root and `amd64/` | none (a checker) | script in CI/pre-commit |
| F2 | **Applied 2026-11-11, see above.** Fold the six compile-time `no-bkl-*` into `smp-shared`; A/B stays on the runtime toggles | medium: 76 cfg sites, SMP behaviour | AArch64 boot suite byte-identical; amd64 TCG SMP=1 826/0; SMP gates need KVM/metal |
| F3 | **Revised 2026-11-10:** keep the instrumentation (none is dead); optionally retire `net-direct-waker` (null A/B) | low | build matrix |
| F4 | **Applied 2026-11-11.** Make `sc-*` one group feature `linux-extra-syscalls` (still individually settable) | low | images unchanged |
| F5 | `rump` per D1: one name, off by default on amd64, `cargo tree` leak check | low | tree check |

## Background

[`docs/reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md) ·
[`docs/reference/build-profiles.md`](../reference/build-profiles.md) ·
research doc §7 (feature unification) and §9.
