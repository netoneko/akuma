# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). In-kernel SSH, networking, ext2 VFS, containers, JS engine, C compiler.

## Layout

- `src/` — Kernel (no_std Rust)
- `userspace/` — ELF binaries (musl libc): paws, dash, herd, meow, quickjs, tcc, sbase
- `crates/` — Host-testable extracted crates
- `docs/` — Design docs
- `scripts/` — Build and debug helpers
- `bootstrap/` — Alpine apk bootstrap assets
- `acceptance/` — Acceptance test playbooks

Never glob or list the repo root — it has 1000+ files. Always use a specific subdirectory path.

## Build & Run

```bash
cargo build --release
cargo run --release                     # QEMU via scripts/cargo_runner.sh
MEMORY=2048 cargo run --release         # Override RAM
GDB=1 cargo run --release              # QEMU gdbstub on :1234
scripts/create_disk.sh                 # (Re)create ext2 disk image
scripts/populate_disk.sh               # Populate disk with userspace binaries
userspace/build.sh                     # Build all userspace binaries
userspace/build.sh --apk-only          # Build apk bootstrap assets only
cargo check                            # Fast diagnostics
```

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

## Testing

Host unit tests (crates only):
```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Pre-commit hook runs clippy + tests automatically.
