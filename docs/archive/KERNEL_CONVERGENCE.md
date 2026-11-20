# Converging the kernels (2026-11-19)

**Status: proposal and inventory.** Akuma ships two kernel *families* that run the same Linux userspace and share most
of their crates, and keep re-fixing the same bugs separately. This records what is shared, what is duplicated, what the
duplication has already cost, and the order in which to converge. It is the case for doing this *before* nested boxes,
not a schedule.

The kernels:

| family | members | shape |
|---|---|---|
| **main** | `akuma` on aarch64 and amd64 (`src/`, `crates/akuma-kernel-glue`, `crates/akuma-kernel-amd64`) | threading and process model of its own (`akuma-exec`), BKL, boxes and namespaces (`akuma-isolation`), a `netpoll` thread, 448-thread table on amd64, an in-kernel boot suite |
| **tokio** | `taoxichuan`, `shanghai`, `denver` on amd64 and aarch64 (`crates/akuma-tokio-kernel*`) | composed from `akuma-tokio-kernel-core` behind a machine trait; spawn ring of 48 slots; no boxes; no `netpoll` thread (tick, idle lap and wait laps); smaller and easier to reason about |

**C1 update (2026-11-20):** `akuma-net::wait_hooks` now supplies the shared socket wait hooks through machine adapters. On AArch64 QEMU TCG, the main kernel and taoxichuan both passed `gateprobe`'s `udp_loopback` and `tcp_loopback` (64 KiB echo and half-close) with two virtual CPUs; both also passed a separate sleeping-child accept/connect probe. The separate AArch64 tokio parking issue C2 remains open.

## What is already shared (and is why the gate's capability matrix is identical)

`akuma-syscalls-*` families, `akuma-net` and `akuma-net-nic` (smoltcp, loopback ring), `akuma-threading` (the scheduler
core), `akuma-user-access`, `akuma-mmu` / `akuma-user-space` / `akuma-cow` (memory), `akuma-ext2` / `akuma-vfs`, `akuma-tty`,
`akuma-fdtable`. A change in one of them reaches every kernel, which is the point and also the risk
(`HOW_MAIN_BROKE_2026_11.md`).

## What is duplicated, and has diverged

| surface | main | tokio | evidence it cost something |
|---|---|---|---|
| **Wait hooks** for the socket layer (`NetRuntime`: `yield_now`, `blocking_relax`, `park_until`) | `crates/akuma-kernel-amd64/src/net.rs`, `akuma-kernel-glue` (aarch64) | `akuma-tokio-kernel-core/src/net.rs` | three copies. The tokio one lacked "a sleeper whose deadline passed needs the scheduler, not a halt" that the main amd64 one had, so a blocking `accept` starved its sibling thread (`TOKIO_KERNELS_LOOPBACK_STARVATION.md`) |
| **Who pumps the NIC** | `netpoll` thread plus doorbell | tick, idle lap, wait laps | loopback frames "ride the next lap" on tokio (`wake_netpoll` is a no-op); latency differs and is unmeasured |
| **Task table** | `akuma-threading` thread table, 448 threads | `proc.rs` spawn ring, `NSLOTS = 48`, `TASK_OF` | `clone` past 48 is `EAGAIN` and std aborts; tokio blocking pools ask for 512 |
| **Boxes and namespaces** | `akuma-isolation`: box registry, mount namespace, `private_loopback`, capability drops | none: `current_box_id: || 0`, no box syscalls | the Denver guests of `AKUMA_ECOSYSTEM_DEMO.md` have no `box`; bubblewrap-in-box and nested boxes (`AKUMA_NESTED_BOXES.md`) have nowhere to land on them |
| **Locking** | BKL (`akuma-bkl`) | the machine's own lock, `bkl_enter`/`bkl_leave` around switches | different rules for "may this thread park here"; the `can_park` gate exists because of it |
| **Dispatch** | `akuma-syscalls-glue` (aarch64), `-glue-amd64` | `akuma-syscalls-*` families called from `akuma-tokio-kernel-*` | a signature change in glue breaks the platform kernels silently; fixed only by building them |
| **Self-test** | 851 in-kernel tests | none; the gate's `gateprobe` from outside | tokio kernels learn of a regression only when someone runs the gate |
| **aarch64 boot** | boots on the author's Mac | built only | `can_park` is still `false` on the aarch64 tokio kernels; nobody here can boot them |

## Why now

* **Nested boxes** (`AKUMA_NESTED_BOXES.md`) need a box registry, a per-namespace socket policy and a builder authority. Those
  exist on one family only. Building them there deepens the split; building them where both can reach them is convergence.
* **The demo** (`AKUMA_ECOSYSTEM_DEMO.md`) runs agents in boxes on Denver guests, and Denver has no boxes. Either the guest is
  the main kernel (boxes yes, JVM unverified) or Denver gains boxes. That is the same decision.
* The tokio kernels now sit inside the merge gate; the first run found three of their bugs in one afternoon. Drift will keep
  surfacing at that rate until the surfaces above are one implementation.

## Proposed order (each step is gate-green on its own)

1. **One wait-hook implementation.** Move `blocking_relax`, `park_until` and `yield_now` into one shared crate, parameterised
   by a small machine trait (`now`, `halt`, `can_park`, `release_lock_around(f)`). The rule is written once: park if the
   scheduler can switch away; yield if a sleeper is due; halt otherwise. Deletes three copies, including the one the main
   kernel got right. Touches every kernel; needs the aarch64 boot on the Mac.
2. **Measure pump models before choosing.** Add a loopback round-trip latency probe and a ping-pong throughput probe to the
   gate; compare the daemon plus doorbell against tick plus idle lap on both families. Pick on numbers.
3. **One task table.** The main kernel's thread table behind the spawn-ring interface, or the ring sized by feature (the
   cheapest first move: raise `NSLOTS` and see what the static arrays cost). `threads_128` in the gate is the check.
4. **Isolation reachable from both.** `akuma-isolation` already is a crate; what ties it to the main kernel is the box
   syscalls and the `HostRoot`/`CallerBox` plumbing in glue. Wire the box registry and `MOUNT_IN_NS` into the tokio kernels
   behind a feature (the `akuma-native`/`middleware` pattern already used by `shanghai`). Then nested boxes are one
   implementation.
5. **One boot suite.** Extract the main kernel's self-tests that are machine-independent into a crate both families run
   (`akuma-kernel-tests-*` is the amd64/aarch64 pair; add the tokio machines).

Do **not** converge by deleting a family. The tokio kernels' value is that they are small and read in an afternoon; the main
kernel's is that it has the features. The target is the *shared crates owning every rule* and each kernel being a machine
description, which is what `akuma-tokio-kernel-core` already is for the tokio side.

## Risks

Every step in 1-4 changes behaviour on at least one kernel that has no boot test on this machine (aarch64). Do each with the
gate green and a boot on the Mac. Step 4 is the sensitive one: it adds an isolation boundary to kernels that have none, so
it needs its own review before it is enabled by default.

## Background

`HOW_MAIN_BROKE_2026_11.md`, `MERGE_GATE_FIRST_RUN.md`, `TOKIO_KERNELS_LOOPBACK_STARVATION.md`, `AKUMA_NESTED_BOXES.md`,
`AKUMA_ECOSYSTEM_DEMO.md`, `docs/reference/subsystems/networking.md` § "Stacks, loopback, namespaces and the threads that drive them".
