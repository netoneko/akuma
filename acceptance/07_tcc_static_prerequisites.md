# tcc -static prerequisites

Minimal requirements for `tcc -static hello.c` to succeed inside the VM.

## What tcc needs

| Requirement | Source | Path on disk |
|---|---|---|
| `tcc` binary | `bootstrap/bin/tcc` (built via `userspace/build.sh --tcc-only`) | `/bin/tcc` |
| libtcc1.a + tcc headers | `bootstrap/archives/libtcc1.tar` | `/usr/lib/tcc/` |
| C headers | `musl-dev` Alpine package | `/usr/include/` |
| musl static libs (`libc.a`, `crt1.o`, …) | `musl-dev` Alpine package | `/usr/lib/` |
| `busybox` (shell, env) | `bootstrap/bin/busybox` **or** `busybox` Alpine package | `/bin/busybox` |

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

1. Copies all `bootstrap/` files (including `tcc`, `libtcc1.tar`, etc.)
2. Runs `apk --root /mnt/disk --no-scripts add busybox musl-dev` — downloads and
   installs the aarch64 Alpine packages directly into the disk image (reads arch
   from `etc/apk/arch` = `aarch64`; `--no-scripts` avoids executing aarch64 triggers)
3. Extracts `archives/libtcc1.tar` into the disk root

After this, the VM boots ready for `tcc -static` with no `apk add` needed at runtime.

## Verification (inside VM after boot)

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

# Check headers and libs are present
rc, out, _ = ssh("ls /usr/include/stdio.h /usr/lib/libc.a /usr/lib/tcc/libtcc1.a")
assert rc == 0, f"Missing prerequisites: {out}"

# Compile and run a hello-world
rc, out, err = ssh(
    "printf '#include <stdio.h>\\nint main(){puts(\"hello tcc\");return 0;}' > /tmp/t.c && "
    "tcc -static -B /usr/lib/tcc -o /tmp/t /tmp/t.c && /tmp/t"
)
assert rc == 0 and "hello tcc" in out, f"tcc -static failed: rc={rc} out={out} err={err}"
print("PASS")
```

## What is NOT required

- `scratch` — only needed for the clone step in `05_meow_tcc_extreme_4mb.md`
- `apk add` at VM runtime — avoided by `--with-apk --with-musl-dev` preparation
- Network access in the VM — not needed for compilation itself (only for `scratch clone`)
- Any shared libraries — `tcc -static` produces a fully self-contained ELF
