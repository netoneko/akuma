# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). Networking, ext2 VFS, containers, JS engine, C compiler.
SSH is a userspace daemon (`userspace/sshd`); the kernel has no SSH server, no shell,
no editor and no cryptography (all removed 2026-08-10 — `docs/archive/BUILTIN_SSH_REMOVAL.md`).

## Layout

- `src/` — Kernel (no_std Rust)
- `crates/` — Host-testable extracted crates:
  `akuma-{exec,ext2,firecracker,isolation,kacho,net,net-yarn,pmm,primitives,rump,terminal,timer,vfs,virtio}`
  plus the `akuma-syscalls*` family below.
  The **syscall family** is three layers, leaf-first and named so the
  layering is visible — an ABI crate, a shape crate, and one crate per syscall
  family (three of those so far): `akuma-syscalls-linux` is the ABI (numbers, `repr(C)`
  wire structs and their layout assertions, flag tables — zero dependencies);
  `akuma-syscalls` is the *shape* of an excursion (which counter bucket, which
  hooks run, where the epilogue's identity comes from) and depends only on the
  ABI crate; `akuma-syscalls-time` is one syscall *family* implementation —
  `clock_gettime`/`clock_settime`/`adjtimex`/itimers/`nanosleep` plus the
  boot-time SNTP client for platforms with no RTC (Firecracker) —
  `docs/reference/subsystems/syscalls/time.md`. It was named `akuma-time`
  until 2026-08-28; the rename is what stops it reading as a sibling of
  `akuma-timer`, the hardware CNTV/PL031 + tick-policy crate below, which it
  is not. `akuma-syscalls-sync` is the second family (2026-08-29): the futex
  op decode, the `(tgid, uaddr)` waiter table, the deadline algebra, the
  `WAKE_OP` opcode and the wait loop's outcome decision — chosen because every
  futex bug in `docs/archive/` is a property of one of those four things and
  each previously cost a `-j4` self-host build to find. The crate decides; the
  lock, the IRQ masking, the in-hold user read and every wake stay in
  `src/syscall/sync.rs`. Gates: `scripts/futex_suite.py` (correctness) and
  `userspace/futexprobe/` + `scripts/benchmarks/futex_ab_run.sh` (cost, run
  A/B/A — `docs/reference/subsystems/syscalls/sync.md`).
  `akuma-syscalls-poll` is the third family (2026-08-29): the fd-state ->
  event-bits **readiness map**, the interest list and `epoll_ctl`'s errno set,
  the `EPOLLET` armed-state decision, and the `ppoll`/`pselect6` wire
  marshalling — chosen because every epoll incident in `docs/archive/` except
  the lock inversion is one of those, and each previously cost a live VM and a
  network client (bun, tokio, nginx, cargo's libcurl) to find. The seam is
  inside what was one function: `epoll_check_fd_readiness` **probed** an fd and
  **mapped** the result in one `match`, and only the mapping was testable.
  Every probe, waker registration, `EPOLL_TABLE` hold and user copy stays in
  `src/syscall/poll.rs`. **The wait loop is NOT in it** — that is
  `akuma-net-yarn`, load-bearing for four syscalls; this extraction stops at
  the readiness edge. Seven known divergences from Linux are preserved and
  pinned rather than fixed. Gates: `scripts/epoll_suite.py` (correctness;
  `--linux` runs the same static binary on real Linux to prove the probe) and
  `userspace/epollprobe/` + `scripts/benchmarks/epoll_ab_run.sh` (cost, run
  A/B/A — `docs/reference/subsystems/syscalls/poll.md`).
  Further families move out on the same model when a family has real
  pure logic worth testing.
  `akuma-kacho` is the shared observe/decide/hysteresis layer every self-tuning
  policy uses (timer-tick demotion, file-page cache cap, netpoll wake rate).
  `akuma-net-yarn` is the readiness wait loop as a pure state machine, driven by
  **all four** blocking-wait paths: `akuma_net::socket::wait_until` and
  `src/syscall/poll.rs`'s `sys_epoll_pwait` / `sys_pselect6` / `sys_ppoll`.
  Callers supply only the effects, so the drain budget, fruitless-progress
  escape, epoch guard, timeout comparison, interrupt precedence and park kind
  are `WaitPolicy` fields with host tests instead of a devbox boot. **The two
  families differ in six of those fields and every difference is a real
  divergence** — don't "unify" one without measuring it (`docs/reference/
  subsystems/syscalls/poll.md` § "The wait loop is one machine"). The crate also
  carries a differential test against the pre-extraction `wait_until`; keep that
  oracle in the shipped loop's shape rather than tidying it.
  `akuma-scheduler` models scheduler placement / netpoll wake policies so a
  candidate can be ranked in a second instead of a devbox boot
  (`docs/archive/AKUMA_SCHEDULING_EXTRACTION.md`). The simulator core (`lib.rs`)
  is `no_std` and in `default-members` like its siblings; only its CLI report
  (`src/main.rs`, `--bin sched-sim`) stays plain `std` and is gated behind
  `required-features = ["cli"]` so a bare `cargo build`/`test` at the repo root
  never tries to cross-compile it for the kernel's own target.
  `akuma-boot` holds the Linux `reboot(2)` ABI decode for the `sc-reboot`
  syscall (`src/syscall/reboot.rs`) — in `default` since 2026-08-25 (only
  `extreme-size`, which builds `--no-default-features`, excludes it) — the
  PSCI SMC call itself stays in `src/smp_shared.rs`, which already owns
  SMC/HVC conduit selection for `CPU_ON`; `sc-reboot` depends on
  `smp-shared` for exactly that reason. Named `-boot`, not `-reboot`: a
  natural future home for `src/boot.rs`'s logic too.
- `userspace/` — ELF binaries (musl libc); current member list + one-liners: `docs/reference/userspace-layout.md`
- `docs/` — Documentation (see below)
- `scripts/` — Build and debug helpers
- `overlays/devbox/` — Devbox distro rootfs + `run.sh` / `run-smoltcp.sh`
- `bootstrap/` — Alpine apk bootstrap assets
- `acceptance/` — Numbered acceptance test playbooks: 05, 10, 11, 13 are the
  live set (extreme-size 4 MB floor, self-host, rump, `cargo install`-built
  agent); `acceptance/archive/` holds the rest, superseded or subsumed
  (`docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md`).
- `proposals/` — In-flight design proposals

Never glob or list the repo root — it has 1000+ files. Always use a specific subdirectory path.

## Documentation

`docs/README.md` is the front door: it has a symptom matrix ("I see X, what do I
read?"), a task list, and the subsystem index. Read it before searching docs by hand.

```
docs/runbooks/     Action-first procedures. "Do X, expect to see Y." Debugging + building.
docs/reference/    Current-state architecture and invariants. No history.
docs/userspace/    Per-binary docs (pointers to source-co-located docs).
docs/archive/      200+ historical investigation docs. Linked from new docs. Correct a
                   doc whose findings a later measurement disproves — a stale "FIXED"
                   is worse than an edited record; date the correction.
```

- Reference subsystem docs live in `docs/reference/subsystems/` (memory, scheduler,
  smp, smp-shared, networking, rump-stack, ssh, vfs, containers, exceptions,
  boot, irq, console, rng, async-fs, config-flags, drivers/),
  with 17 per-family syscall docs under `docs/reference/subsystems/syscalls/`.
- Linux ABI / musl notes: `docs/reference/abi/`.
- Build targets: `docs/reference/build-profiles.md`; every feature and env knob:
  `docs/reference/subsystems/config-flags.md`.
- Reference docs carry a **stability grade** — A (stable, trust it), B (verify
  behaviour), C (active risk, expect surprises). Check the grade before relying on a doc.

When writing docs: runbooks are named after the task/symptom and end with a
**Verify** section; new docs get a "Background" footer linking the `archive/`
originals; add a row to the relevant triage matrix (`docs/README.md`,
`docs/runbooks/README.md`).

## Build & Run

```bash
cargo build --release
cargo run --release                     # QEMU via scripts/cargo_runner.sh
MEMORY=2048 cargo run --release         # Override RAM
GDB=1 cargo run --release               # QEMU gdbstub on :1234
scripts/create_disk.sh                  # (Re)create ext2 disk image
scripts/populate_disk.sh                # Populate disk with userspace binaries
userspace/build.sh                      # Build all userspace binaries
userspace/build.sh --apk-tools-only     # Build apk bootstrap assets only
userspace/build.sh --meow-only          # Build a single member (--<name>-only)
cargo check                             # Fast diagnostics
```

### Other build targets

A build target is **profile + feature set**, always selected together. Details
and tradeoffs in `docs/reference/build-profiles.md`.

```bash
scripts/build_extreme_size.sh                             # extreme-size (4.0 MB floor, userspace sshd + paws, single-core)
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh  # default devbox (userspace sshd)
scripts/build_devbox.sh && overlays/devbox/run.sh         # rump devbox (deferred; needs RUMP_NIC=1)
```

**`cargo build --release` is real SMP.** `smp-shared` — one shared kernel across
all cores under real locks — is in the default feature set; run it with `SMP=N`.
That is *the* SMP. The `smp` feature is the separate, experimental
**multikernel** (one whole kernel per core); the two are mutually exclusive
(build.rs panics), so building it needs `--no-default-features`.

Profiles are only `release`, `extreme-size` and `release-debug` — a build target
is `--release` plus a feature set.

## VM Access

SSH on port 2222: `ssh -o StrictHostKeyChecking=no root@localhost -p 2222`

The `ssh` CLI command is blocked by security policy. Use Python to run SSH commands:
```python
import subprocess
subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-p", "2222", "root@localhost", "<cmd>"])
```

### Waiting for a VM

**Poll the guest with an ssh round-trip. Never grep the boot log for a marker,
and NEVER** call `job_output` with `wait: true` on the QEMU process (it runs
forever).

```bash
scripts/vm_ready.py 2222            # exits 0 once the guest answers ssh
# or, in Python:  import vm_ready; vm_ready.wait_ready(port=2222, proc=qemu)
```

The old `grep -aqE "sshd started|Started sshd"` recipe is wrong in **both**
directions and cost real time in both:

- **False negative.** At SMP>1 the cores interleave console output, so the line
  arrives torn (`[herd] Starting service: sshd` / `sshd (pid= 2)`). Worse, some
  builds never print either spelling: measured 2026-08-28, a VM served ssh for
  570 s with `bind=1 listen=1 accept=371380` in `[PSTATS]` and **zero** marker
  matches, so a 10-minute wait timed out against a VM that was ready in seconds.
- **False positive.** The marker means sshd *started*, not that it can accept —
  and it never expires, so a stale log from the previous run reads as ready.

Connecting to the forwarded TCP port is not readiness either: QEMU opens the
`hostfwd` listener on the host the moment it starts, long before the guest is up.
Only a completed command counts.

`scripts/vm_ready.py` is the one implementation; harnesses import it rather than
re-deriving the check. Marker greps that remain in `scripts/` are fallbacks for
the "ssh never came up at all" diagnostic path only.

If you *are* reading a boot log for something else, `-a` is mandatory — QEMU
emits a control byte that makes plain `grep` treat the log as binary and print
nothing. And note there are two startup paths for sshd: `extreme-size` has the
kernel spawn it, every other profile lets herd do it.

**Never `pkill -f qemu-system-aarch64`.** Other VMs belonging to the user may be
running (they use `INSTANCE=N`, which maps ssh to `2222 + 100*N`). Kill only the
instance you started, matched on its own forward, e.g.
`hostfwd=tcp::2222-:22` for INSTANCE=0.

If the VM wedges (100% CPU, unresponsive), see `docs/runbooks/recover-wedged-vm.md`.

## Kernel conventions

**Analyze every new or changed path for allocations.** The best code allocates
nothing; every kernel allocation is a potential problem — it can fail, it can
fragment, it can recurse into the allocator on the path that is trying to report
the allocator broke, and it can turn a bounded operation into an unbounded one.
Before finishing a change, look at what it allocates and justify each one: prefer
fixed arrays, `static` per-slot state, stack buffers and borrows over `Vec`,
`String`, `Box` and `format!`. Fallible allocation beats infallible on any path
that can run under memory pressure.

**Console output must use `safe_print!` / `tprint!`.** No heap allocation on
any path that ends at the console — no `format!`, no `String`, no hand-rolled
stack writer, and no `-> String` helper feeding a print. The console is what
survives when the allocator is what broke. Exemptions (variable-cardinality
loops, no-runtime-registered paths) and the reasoning are in
`docs/reference/subsystems/console.md` § "Printing rules"; the violations that
motivated the rule are in `docs/archive/ALLOC_PRINT_AUDIT.md`.

## Testing

Host unit tests (crates only):
```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
# akuma-ssh-crypto lives under userspace/ (its only consumer is userspace/sshd):
(cd userspace && cargo test -p akuma-ssh-crypto --target $(rustc -vV | grep '^host:' | cut -d' ' -f2))
```

Two userspace binaries split their pure logic into a library half so it can be
host-tested; both need `--no-default-features` to drop `libakuma`, whose
`#[panic_handler]`/`#[global_allocator]` collide with std's, and `--lib` so the
`no_main` binary is not built:
```bash
cd userspace && HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p sshd --lib --no-default-features --target $HOST   # wire.rs
cargo test -p box  --lib --no-default-features --target $HOST   # boxlib: json, oci, paths, spec
```

Acceptance playbooks live in `acceptance/` — run them end-to-end for
integration coverage. `scripts/` has targeted harnesses too
(`ssh_harness.py`, `forktest_smp_matrix.py`, `run_selfhost_kernelbuild.py`,
`test_sched_bklfree_ticket_fix.py`, …).

Pre-commit hook runs clippy + tests automatically.

## Working with Claude Code in this repo

Never use the `fork` subagent type (or any multi-agent fan-out) in this repo — do the
work directly instead. Forking copies the whole conversation context into a background
agent, which costs far more tokens than doing it inline.
