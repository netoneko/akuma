# tcc -static prerequisites

Minimal requirements for `tcc -static hello.c` to succeed inside the VM.

## What tcc needs

| Requirement | Source | Path on disk |
|---|---|---|
| `tcc` binary | `bootstrap/bin/tcc` (built via `userspace/build.sh --tcc-only`) | `/bin/tcc` |
| libtcc1.a + tcc headers | `bootstrap/archives/libtcc1.tar` | `/usr/lib/tcc/` |
| C headers | `musl-dev` Alpine package | `/usr/include/` |
| musl static libs (`libc.a`, `crt1.o`, …) | `musl-dev` Alpine package | `/usr/lib/` |
| `busybox` (shell, env) | `bootstrap/bin/busybox` **or** `busybox-static` Alpine package | `/bin/busybox` |

The tcc invocation used in tests:
```
tcc -static -B /usr/lib/tcc -o /tmp/hello /tmp/hello.c
```

`-B /usr/lib/tcc` tells tcc where `libtcc1.a` and its internal headers live.
`-static` links against `/usr/lib/libc.a` and `/usr/lib/crt1.o`.

## Disk preparation — one command

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

This runs inside a Docker container (same Alpine image used for populating):

1. Copies all `bootstrap/` files (including `tcc`, `libtcc1.tar`, `tmp/t.c`, etc.)
2. Runs `apk --root /mnt/disk --no-scripts add busybox-static musl-dev` — downloads and
   installs the aarch64 Alpine packages directly into the disk image (reads arch
   from `etc/apk/arch` = `aarch64`; `--no-scripts` avoids executing aarch64 triggers)
3. Extracts `archives/libtcc1.tar` into the disk root

`busybox-static` (not `busybox`) is required so busybox works at 4 MB — the dynamic
`busybox` package pulls in musl.so which causes a SIGSEGV under memory pressure when
forking/exec'ing child processes.

After this, the VM boots ready for `tcc -static` with no `apk add` needed at runtime.

## Verification (inside VM after boot)

`/tmp/t.c` is pre-staged by `populate_disk.sh` from `bootstrap/tmp/t.c`.  Run tcc and
the resulting binary directly via the kernel SSH (no busybox shell needed).

```python
import subprocess, re

def ssh(cmd, timeout=60):
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-p", "2222", "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout
    )
    out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stdout).strip()
    return r.returncode, out, r.stderr.strip()

# Check headers and libs are present (list the directory, not individual files)
_, out, _ = ssh("ls /usr/include")
assert "stdio.h" in out, f"/usr/include/stdio.h missing: {out}"
_, out, _ = ssh("ls /usr/lib")
assert "libc.a" in out, f"/usr/lib/libc.a missing: {out}"
_, out, _ = ssh("ls /usr/lib/tcc")
assert "libtcc1.a" in out, f"/usr/lib/tcc/libtcc1.a missing: {out}"

# Compile the pre-staged hello-world (bootstrap/tmp/t.c -> /tmp/t.c on disk)
rc, out, err = ssh("tcc -static -B /usr/lib/tcc -o /tmp/t /tmp/t.c")
assert "error" not in out.lower(), f"tcc compile failed: {out}"

# Run via exec (kernel SSH exec supports full paths)
_, out, _ = ssh("exec /tmp/t")
assert "hello tcc" in out, f"Expected 'hello tcc', got: {out}"
print("PASS")
```

## What is NOT required

- `scratch` — only needed for the clone step in `05_meow_tcc_extreme_4mb.md`
- `apk add` at VM runtime — avoided by `--with-apk --with-musl-dev` preparation
- Network access in the VM — not needed for compilation itself (only for `scratch clone`)
- Any shared libraries — `tcc -static` produces a fully self-contained ELF
