# Acceptance: tcc -static prerequisites (extreme kernel, 4 MB)

Verify that `tcc -static` can compile and run a C program on the **extreme-size**
kernel at **4.0 MB** RAM, using only pre-staged files (no `apk add` at runtime).

All prerequisites — `tcc`, `libtcc1.a`, musl headers, musl static libs, and
`busybox-static` — are installed into the disk image by `populate_disk.sh` before
boot so the 4 MB budget is not spent on package downloads.

---

## VM shell notes

The VM runs a custom mini-shell (not `/bin/sh`). It lacks `printf`, `which`,
`find -name`, `head`, `tail`, and the `/dev/null` device. Pipes and complex
redirections are unreliable. Use Python for all SSH commands (see step 3).

SSH commands often return rc=255 with `Connection to localhost closed by remote
host.` even when the command succeeded — the VM drops the connection after
long-running commands. **Check stdout for success output, not the return code.**

---

## Preparation (host)

### 1. Build the extreme kernel

```bash
scripts/build_extreme_size.sh
```

This produces `target/aarch64-unknown-none/extreme-size/akuma` (~800 KB stripped
binary, RSA off, debug instrumentation off, ext2 block-cache disabled).

### 2. Populate the disk

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

This pre-installs into the disk image:
- `busybox-static` and `musl-dev` via Alpine apk (offline, no network needed in VM)
- Extracts `libtcc1.tar` → `/usr/lib/tcc/`
- Wipes `/tmp` and re-stages `bootstrap/tmp/` (including `hello.c`)

After this step:
- `/bin/tcc` — tcc binary
- `/usr/lib/tcc/libtcc1.a` — tcc runtime
- `/usr/include/stdio.h` — C headers
- `/usr/lib/libc.a` — musl static libc
- `/tmp/hello.c` — pre-staged test source (`printf("Hello, Akuma!\n")`)

### 3. Start the VM at 4 MB

```bash
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=4096K SNAPSHOT=1 INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 07_tcc_static.log
```

`MEMORY=4096K` = 4.0 MB. `SNAPSHOT=1` ensures every boot starts from the
clean populate-disk state.

The QEMU process runs forever — do NOT block on it or call job_output with
wait=true. Poll the log instead:

```bash
until grep -q "\[SSH Server\] Listening" 07_tcc_static.log 2>/dev/null; do sleep 2; done
```

Define the SSH helper for all VM steps:

```python
import subprocess, re

def ssh(cmd, timeout=120):
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-p", "2222", "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout
    )
    out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stdout).strip()
    err = '\n'.join(
        l for l in re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stderr).strip().splitlines()
        if '@@@@' not in l and 'Warning: Permanently' not in l
    ).strip()
    return r.returncode, out, err
```

---

## Steps (in VM)

### 4. Verify prerequisites

```python
_, out, _ = ssh("ls /usr/include")
assert "stdio.h" in out, f"/usr/include/stdio.h missing: {out}"
print("headers: OK")

_, out, _ = ssh("ls /usr/lib")
assert "libc.a" in out, f"/usr/lib/libc.a missing: {out}"
print("libc.a: OK")

_, out, _ = ssh("ls /usr/lib/tcc")
assert "libtcc1.a" in out, f"/usr/lib/tcc/libtcc1.a missing: {out}"
print("libtcc1.a: OK")
```

### 5. Compile hello.c with tcc -static

`/tmp/hello.c` is pre-staged from `bootstrap/tmp/hello.c` by `populate_disk.sh`.
`-B /usr/lib/tcc` tells tcc where `libtcc1.a` lives.

```python
rc, out, err = ssh("tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c")
print(f"compile rc={rc} | out={out!r} | err={err!r}")
# rc=255 is normal (SSH drop); success = no "error" in output
assert "error" not in out.lower(), f"tcc compile failed: {out}"
```

### 6. Run the compiled binary

```python
rc, out, err = ssh("exec /tmp/hello_c")
print(f"run rc={rc} | out={out!r}")
assert "Hello" in out, f"Expected 'Hello' in output, got: {out!r}"
print("PASS")
```

---

## Expected output

Step 4 prints:
```
headers: OK
libc.a: OK
libtcc1.a: OK
```

Step 5 produces no output (tcc compiles silently on success).

Step 6 prints:
```
Hello, Akuma!
```

---

## Memory profile at 4.0 MB

| Stage | Free RAM low-water |
|---|---|
| Post-boot idle | ~2520 KB |
| During tcc compile peak | ~1988 KB |
| After tcc exits | ~2520 KB |

`tcc -static` floor verified at 4.0 MB (`scripts/our_tcc_floor.py`, 2026-06-06).

---

## Failure modes

| Symptom | Diagnosis |
|---|---|
| VM never reaches SSH | boot OOM — kernel image grew; check `IMAGE_RESERVE` |
| `/usr/include/stdio.h missing` | `--with-musl-dev` not passed to `populate_disk.sh` |
| `/usr/lib/tcc/libtcc1.a missing` | `libtcc1.tar` not in `bootstrap/archives/` or extract failed |
| `tcc: error: file 'libtcc1.a' not found` | `-B /usr/lib/tcc` flag missing |
| `memory full` from tcc | RAM < 4 MB; use `MEMORY=4608K` |
| `exec /tmp/hello_c` → rc=255, empty out | compile failed silently; check step 5 output |
