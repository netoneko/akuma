# Run the merge gate

**Do X:** `scripts/gate.py` from the repo root. **Expect:** a job list, a capability matrix (capability x kernel) and
`gate: PASS in N min`, exit 0. Target: 10-15 minutes (measured 1.5 min warm, 4-5 min with every target dir cold) (`$GATE_DIR`, default `/var/tmp/akuma-gate`); the
first run on a clean machine compiles sixteen kernels and takes longer.

Why it exists: `docs/archive/HOW_MAIN_BROKE_2026_11.md`. A build of the main kernel says nothing about the platform
kernels that share its crates, and no build sees a behaviour change.

## What runs (all in parallel, bounded by build and boot slots)

| tier | job | what it proves |
|---|---|---|
| prep | `gateprobe` | the probe (static musl, std only) passes on the host Linux first |
| static | `check_*.py` ratchets, `host-tests` | the budgets, and the extracted crates' tests (per crate; AArch64-only crates skipped on an x86 host) |
| build | the 16 steps of `scripts/premerge_build.sh --emit-steps` | every kernel x architecture x feature set builds `--release` |
| boot | `akuma-amd64-selftests-smp1/2` | the in-kernel boot suite: `Akuma/amd64 self-test: N passed, 0 failed` |
| boot | `<kernel>-amd64-gateprobe` (akuma, taoxichuan, shanghai, denver) | one `GATE <capability> PASS/FAIL` line per capability, below |

`gateprobe` capabilities: `threads` (clone + futex), `time` (sleep, wall clock), `memory` (mmap, brk, mremap, demand
faults), `files`, `files_meta` (symlink, link, chmod, truncate, pread/pwrite, holes), `big_file` (8 MiB), `tcp_bind`,
`spawn_wait`, `spawn_many`, `threads_and_children`, `pipe_throughput` (4 MiB), `signal_kill`, `env_args_cwd`,
`stack_growth`, `udp_loopback`, `spawn_churn` (120 sequential children), `thread_churn` (200 sequential threads),
`many_threads` (32 alive together), `tcp_loopback`, `threads_128` (capacity: an xfail on the tokio kernels' 48-slot ring). Order matters: the probe prints as it goes and a
wedged guest hides every check after the wedge, so hang-prone checks run last. Add one in `scripts/gate/gateprobe/src/main.rs`; the gate picks the name up from `--list`.

Boots use QEMU microvm + KVM on a **snapshot** of a shared read-only image, so they run side by side. A boot job kills only
its own QEMU (its own process group).

## Known failures

`scripts/gate/xfail.txt` (`kernel<TAB>capability<TAB>reason`). A listed failure does not fail the gate; a listed capability
that passes does (`XPASS`), so the list cannot rot. Never add a line without a reason that names the bug.

## Options

`--only taoxichuan` (plus what it needs), `--no-build` (boot jobs on kernels already in `$GATE_DIR`), `--jobs BUILD,BOOT`,
`--list`, `--with-jvm` (the Denver Clojure/JDK gate, `overlays/denver/check.sh`; it boots the kernel and image in
`overlays/denver/out/`, not the gate's own build, and needs `build-image.sh` first).

## Not covered (be honest about it)

* **aarch64 kernels are built, not booted.** This x86 box has no aarch64 KVM and TCG blows the budget. Boot them on the Mac.
* `tokioprobe`'s HTTPS step (needs the internet) and the main kernel's live suites (`scripts/bwrap_suite.py`,
  `box_amd64_suite.py`, `epoll_suite.py`, ...) are separate; run the one for the subsystem you touched.
* Only one amd64 SMP shape per boot job; the SMP matrix scripts are separate.

## Verify

`scripts/gate.py --list` shows the jobs; `$GATE_DIR/logs/<job>.log` is each job's output, and `<job>.log.serial` a boot's console.

## Background

`docs/archive/HOW_MAIN_BROKE_2026_11.md`, `proposals/PREVENTING_BROKEN_MAIN.md`, `docs/archive/TOKIO_KERNEL_GATES_RERUN.md`.
