# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). Networking, ext2 VFS, containers, JS engine, C compiler.
SSH is a userspace daemon (`userspace/sshd`); the kernel has no SSH server, no shell,
no editor and no cryptography (all removed 2026-08-10 — `docs/archive/BUILTIN_SSH_REMOVAL.md`).

## Layout

- `src/` — Kernel (no_std Rust)
- `crates/` — Host-testable extracted crates:
  `akuma-{exec,ext2,isolation,kacho,net,net-yarn,pmm,primitives,rump,terminal,timer,vfs,virtio}`.
  `akuma-kacho` is the shared observe/decide/hysteresis layer every self-tuning
  policy uses (timer-tick demotion, file-page cache cap, netpoll wake rate).
  `akuma-net-yarn` is the socket readiness wait loop (`wait_until`) as a pure
  state machine — the kernel supplies only the effects, so the drain budget,
  the fruitless-progress escape, the epoch guard and the park policy all have
  host tests instead of a devbox boot. It also carries a differential test
  against the pre-extraction loop; keep that oracle in the shipped loop's shape.
  `akuma-scheduler` is host-only and **not** in `default-members`: it models
  scheduler placement / netpoll wake policies so a candidate can be ranked in a
  second instead of a devbox boot (`docs/archive/AKUMA_SCHEDULING_EXTRACTION.md`).
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
docs/archive/      200+ historical investigation docs, verbatim. Linked from new docs, never rewritten.
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

To wait for VM boot, poll the log file — NEVER call `job_output` with `wait: true` on the QEMU process (it runs forever):
```bash
until grep -aqE "sshd started|Started sshd" 01_verify_apk_bootstrap_acceptance.log 2>/dev/null; do sleep 2; done
```

Two markers because there are two startup paths: `extreme-size` has the kernel
spawn sshd (`[Main] sshd started`), every other profile lets herd do it
(`[herd] Started sshd`). `-a` is required — QEMU emits a control byte that makes
plain `grep` treat the log as binary.

If the VM wedges (100% CPU, unresponsive), see `docs/runbooks/recover-wedged-vm.md`.

## Kernel conventions

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
