# Gaps — what the kernel does not do yet

Ranked by what blocks the ladder (`../README.md` § Status). Each entry names
the probe that shows it. Known-open items from the M4 slice work are listed
first.

## Open on the current slice

| # | gap | the probe that shows it |
|---|---|---|
| G1 | **PK2 open** (M4 slice-3 PH) | `docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` — the PK2 gate does not pass |
| G2 | **Rare boot stall** | a boot that stops before the banner with no `[Fault]`; intermittent, not yet root-caused (same PH result doc) |
| G4 | **Init is not a Process**: a program run as init cannot fork or create a thread (EPERM); `pm` S1, with herd as pid 1 (the user's Q4) | `init=/usr/local/rust/bin/rustc` dies at the ctrlc handler thread |
| G5 | **No linker yet**: `rustc hello.rs` reaches `cc` and the spawn needed `socketpair` (added 2026-10-30; the link itself is being retried) | `rustc -C codegen-units=1 hello.rs -o hello` over ssh |
| G6 | **HTTPS `git clone` / TLS in the guest** untested | `git clone https://...` |
| G3 | **No pump while a spawned child runs** | TCP state stalls during a long-running spawned child; frames move again at its exit (`reference/devices.md` § the pump) |

## Pinned exclusions (deliberate, load-bearing — not bugs)

| exclusion | why | what lifts it |
|---|---|---|
| No scheduler beyond the slice-3 switch | one core; `switch_to`/the switching tick/parked waits landed (M4 slice 3), priorities and timeslices did not | SMP slice 5 |
| No BKL | every crate builds without `smp-shared`; no-ops are *correct* for one core | SMP slice 5 |
| No pty, no signals, no users | the ladder has not needed them | a stage that does |
| No PCI | Firecracker-style virtio-mmio is the pinned machine | a second machine |
| No VFS mount table | one ext2 root | a second filesystem |
| No lazy mmap (CoW fork exists) | `fork` shares leaf frames copy-on-write (`fault_trap` → `cow_resolve`, 2026-10-30), but `mmap` still populates every page at call time, anonymous and file-backed alike. Building this kernel in the guest peaked at >= 2,460 MiB against Linux's 447 MiB for the same compile, because each `rustc` pays for its whole 311 MB `librustc_driver` mapping | demand paging in the fault path (`archive/TAOXICHUAN_EFFICIENCY_PLAN_GLM.md`, top item) |
| `open` after `close(0..2)` skips the closed std slot | fds 0-2 are implicit console slots, so `open` allocates from 3: `close(0); open("/dev/null")` returns 3, which ash's `cmd &` treats as failure (`archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` §12) | remember closed std fds per task, allocate lowest-free from 0 |
| `register_process` is infallible | a genuine 256-live-process workload still panics `Process table full` instead of answering `EAGAIN` (the thread-exit leak that reached it is fixed) | a `Result` through the spawn callers (`reference/subsystems/memory.md` OPEN item) |
| `PROT_NONE` is not enforced | `munmap` returns address space and `mprotect` re-maps PTEs, but `PROT_NONE` stays a no-op, so a guard page is still readable | a PTE-level none |
| W^X is enforced (stricter than Linux) | `W+X` refused; `PROT_EXEC` refused on any page written since mapping; `wx=0` restores the old RWX widening. No JIT can run | see `archive/TAOXICHUAN_WX_POLICY.md` |
| No stdin path | the console is write-only; CLOSE_CHILD_STDIN is a no-op success | an input device + a reader |
| No wall clock on inodes | mount clock is `|| 0`; `utimensat` validates, does not stamp | `epoch=` anchoring reaching ext2 |
| Identity map kept | the lower-half handoff has no customer | never required; per-task roots already lack it |

## Ladder gaps (not started)

| stage | target | notes |
|---|---|---|
| 8 | `git clone` | brief: `scripts/kvm/goose/taoxichuan-09j-stage8-git-clone.md` |
| 9–12 | cargo fetch → rustc → cargo → fixed point | the agent log's gap list; tcc already compiles and runs a C program in the guest (agent log, "First compiler probes") |

### The rustc syscall list

Scored against `docs/archive/RUST_TOOLCHAIN.md` in
[`reference/rust-toolchain.md`](reference/rust-toolchain.md): **7 of 10
served (70%)**; the missing 30% is the AF_UNIX family (`socketpair`,
`recvmsg`/`sendmsg`) plus `close_range`, which is the stage-9 blocker in
dependency order.
