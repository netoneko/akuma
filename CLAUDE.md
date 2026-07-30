# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). In-kernel SSH, networking, ext2 VFS, containers, JS engine, C compiler.

## Layout

- `src/` — Kernel (no_std Rust)
- `crates/` — Host-testable extracted crates: `akuma-{editor,exec,ext2,isolation,net,rump,shell,smp,ssh,ssh-crypto,terminal,vfs}`
- `userspace/` — ELF binaries (musl libc): paws, dash, herd, meow, quickjs, tcc, sbase, box, sshd, httpd, crush, tar, llama.cpp, rumpkernel, plus small repro/stress programs
- `docs/` — Documentation (see below)
- `scripts/` — Build and debug helpers
- `overlays/devbox/` — Devbox distro rootfs + `run.sh` / `run-smoltcp.sh`
- `bootstrap/` — Alpine apk bootstrap assets
- `acceptance/` — Numbered acceptance test playbooks (`01_verify_apk_bootstrap.md` … `12_multikernel_demo.md`)
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
  boot, irq, console, rng, async-fs, editor, shell, config-flags, drivers/),
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
scripts/build_size.sh                                     # size (slim)
scripts/build_extreme_size.sh                             # extreme-size (4 MB RAM floor, no HTTPS)
cargo build --profile release-smp --features smp          # multikernel (one kernel per core)
cargo build --profile release-smp-shared --features smp-shared  # real shared-kernel SMP
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh  # default devbox (smoltcp + SMP + userspace sshd)
scripts/build_devbox.sh && overlays/devbox/run.sh         # rump devbox (deferred; needs RUMP_NIC=1)
```

`smp` and `smp-shared` are mutually exclusive (build.rs enforces).
`cargo build --release` is single-core.

## VM Access

SSH on port 2222: `ssh -o StrictHostKeyChecking=no root@localhost -p 2222`

The `ssh` CLI command is blocked by security policy. Use Python to run SSH commands:
```python
import subprocess
subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-p", "2222", "root@localhost", "<cmd>"])
```

To wait for VM boot, poll the log file — NEVER call `job_output` with `wait: true` on the QEMU process (it runs forever):
```bash
until grep -q "SSH Server\] Listening" 01_verify_apk_bootstrap_acceptance.log 2>/dev/null; do sleep 2; done
```

If the VM wedges (100% CPU, unresponsive), see `docs/runbooks/recover-wedged-vm.md`.

## Testing

Host unit tests (crates only):
```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Acceptance playbooks live in `acceptance/` — run them end-to-end for
integration coverage. `scripts/` has targeted harnesses too
(`ssh_harness.py`, `forktest_smp_matrix.py`, `run_selfhost_kernelbuild.py`,
`test_sched_bklfree_ticket_fix.py`, …).

Pre-commit hook runs clippy + tests automatically.
